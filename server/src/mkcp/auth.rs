//! mKCP authenticator — port of mihomo v1.19.34 `transport/mkcp/auth.go`.
//!
//! The deployment config has no `seed`, so the active variant is
//! `simpleAuthenticator`: FNV-1a32 + a 4-byte-stride XOR difference over the
//! whole datagram, 6 bytes of overhead.
//!
//! The AES-128-GCM variant (non-empty `seed`) is library code only: the
//! listener never selects it (meow-config warns and drops `seed`, server
//! ADR-0002 Class B divergence #4), and its wire framing has NOT been
//! verified against the Go oracle — no golden vectors exist in this
//! checkout. The nonce guards below therefore fail closed (empty seal /
//! `None` open) instead of panicking on the empty nonce the simple callers
//! pass.

/// Security mode selected by the `seed` field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Security {
    Simple,
    Aes128Gcm([u8; 16]),
}

impl Security {
    pub fn from_seed(seed: &[u8]) -> Self {
        if seed.is_empty() {
            return Self::Simple;
        }
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(seed);
        let mut key = [0u8; 16];
        key.copy_from_slice(&digest[..16]);
        Self::Aes128Gcm(key)
    }
}

fn fnv1a32(data: &[u8]) -> u32 {
    let mut hash = 2_166_136_261u32;
    for byte in data {
        hash ^= *byte as u32;
        hash = hash.wrapping_mul(16_777_619);
    }
    hash
}

/// Forward XOR difference: x[i] ^= x[i-4] for i >= 4.
fn xor_forward(data: &mut [u8]) {
    for i in 4..data.len() {
        data[i] ^= data[i - 4];
    }
}

/// Reverse XOR difference (decrypt direction).
fn xor_backward(data: &mut [u8]) {
    let mut i = data.len();
    while i > 4 {
        i -= 1;
        data[i] ^= data[i - 4];
    }
}

/// Seal a plaintext segment with the simple authenticator.
///
/// Go pads the underlying byte slice to a 4-byte boundary before applying the
/// XOR difference, but only returns `dst[:dstLen]` (padding is not sent).
/// The caller therefore keeps the padding internally and sends `buf[..wire]`.
pub fn seal_simple(plain: &[u8]) -> (Vec<u8>, usize) {
    let total = 6 + plain.len();
    let extra = (4 - total % 4) % 4;
    let mut out = vec![0u8; total + extra];
    out[4..6].copy_from_slice(&(plain.len() as u16).to_be_bytes());
    out[6..6 + plain.len()].copy_from_slice(plain);
    let checksum = fnv1a32(&out[4..6 + plain.len()]);
    out[..4].copy_from_slice(&checksum.to_be_bytes());
    xor_forward(&mut out);
    (out, total)
}

/// Open a simple-authenticated datagram. Returns the plaintext payload.
pub fn open_simple(wire: &[u8]) -> Option<Vec<u8>> {
    let extra = (4 - wire.len() % 4) % 4;
    let mut padded = vec![0u8; wire.len() + extra];
    padded[..wire.len()].copy_from_slice(wire);
    xor_backward(&mut padded);
    if padded.len() < 6 {
        return None;
    }
    let expected = u32::from_be_bytes(padded[..4].try_into().ok()?);
    // FNV covers only the sealed bytes (length field + payload), not the
    // 4-byte-alignment padding appended for the XOR difference.
    if expected != fnv1a32(&padded[4..wire.len()]) {
        return None;
    }
    let length = u16::from_be_bytes([padded[4], padded[5]]) as usize;
    if wire.len() - 6 != length {
        return None;
    }
    Some(padded[6..6 + length].to_vec())
}

/// Seal with the configured security mode. Returns (buffer, wire length).
///
/// The AES variant fails closed on a wrong-size nonce — returning an empty
/// wire — rather than panicking; callers treat `wire == 0` as "nothing to
/// send" (see `pack_segment_datagrams`' caller). Never selected by the
/// listener today; wire framing unverified against the Go oracle.
pub fn seal(security: &Security, nonce: &[u8], plain: &[u8]) -> (Vec<u8>, usize) {
    match security {
        Security::Simple => seal_simple(plain),
        Security::Aes128Gcm(key) => {
            if nonce.len() != 12 {
                return (Vec::new(), 0);
            }
            use aes_gcm::aead::{Aead, KeyInit, Payload};
            use aes_gcm::{Aes128Gcm, Nonce};
            let cipher = Aes128Gcm::new_from_slice(key).expect("16-byte AES key");
            let mut sealed = cipher
                .encrypt(
                    Nonce::from_slice(nonce),
                    Payload {
                        msg: plain,
                        aad: b"",
                    },
                )
                .expect("AES-GCM encryption cannot fail");
            sealed.truncate(plain.len() + 16);
            let length = sealed.len();
            (sealed, length)
        }
    }
}

/// Open with the configured security mode. The AES variant fails closed
/// (returns `None`) on a wrong-size nonce instead of panicking.
pub fn open(security: &Security, nonce: &[u8], wire: &[u8]) -> Option<Vec<u8>> {
    match security {
        Security::Simple => open_simple(wire),
        Security::Aes128Gcm(key) => {
            if nonce.len() != 12 {
                return None;
            }
            use aes_gcm::aead::{Aead, KeyInit, Payload};
            use aes_gcm::{Aes128Gcm, Nonce};
            let cipher = Aes128Gcm::new_from_slice(key).expect("16-byte AES key");
            cipher
                .decrypt(
                    Nonce::from_slice(nonce),
                    Payload {
                        msg: wire,
                        aad: b"",
                    },
                )
                .ok()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_auth_roundtrip() {
        let (sealed, wire) = seal_simple(b"mkcp");
        assert_eq!(wire, 6 + 4);
        assert_eq!(open_simple(&sealed[..wire]), Some(b"mkcp".to_vec()));
    }

    #[test]
    fn simple_auth_rejects_corruption() {
        let (sealed, wire) = seal_simple(b"mkcp");
        let mut corrupted = sealed[..wire].to_vec();
        corrupted[7] ^= 0xff;
        assert_eq!(open_simple(&corrupted), None);
    }

    #[test]
    fn authentication_is_length_dependent() {
        // The XOR difference covers the padding, so lengths that are not a
        // multiple of 4 must still round-trip.
        for len in 1..=16 {
            let payload = vec![0x5a; len];
            let (sealed, wire) = seal_simple(&payload);
            assert_eq!(open_simple(&sealed[..wire]), Some(payload), "len={len}");
        }
    }

    #[test]
    fn empty_seed_selects_simple() {
        assert_eq!(Security::from_seed(b""), Security::Simple);
        assert_ne!(Security::from_seed(b"secret"), Security::Simple);
    }

    #[test]
    fn aes_variant_fails_closed_on_wrong_size_nonce() {
        // Callers pass an empty nonce for the simple variant; the AES arm
        // used to reach Nonce::from_slice(&[]) which panics (GenericArray
        // length assert fires in release too). Fail closed instead.
        let security = Security::from_seed(b"seed");
        let (sealed, wire) = seal(&security, &[], b"x");
        assert_eq!(wire, 0, "wrong-size nonce must fail closed, not panic");
        assert_eq!(open(&security, &[], b"whatever"), None);
        assert!(sealed.is_empty());
    }

    #[test]
    fn aes_variant_roundtrips_with_explicit_nonce() {
        // Crypto self-consistency ONLY — this is NOT a wire-format claim;
        // the mKCP AES framing has no Go-oracle vectors in this checkout
        // (module doc, ADR-0002 Class B divergence #4).
        let security = Security::from_seed(b"seed");
        let nonce = [7u8; 12];
        let plain = b"segment bytes";
        let (sealed, wire) = seal(&security, &nonce, plain);
        assert_eq!(wire, plain.len() + 16);
        assert_eq!(
            open(&security, &nonce, &sealed[..wire]).as_deref(),
            Some(&plain[..])
        );
    }
}
