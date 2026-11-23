//! VLESS inbound handshake (sing-vless / mihomo v1.19.34 wire format).
//!
//! Request header, in order:
//! `version(1) | uuid(16) | addons_len(1) | addons(N) | command(1) |
//!  port(2, BE) | address_type(1) | address(variable)`
//!
//! The port-then-address layout is `vmess.AddressSerializer.ReadAddrPort`, not
//! the more familiar address-then-port SOCKS layout. Response header is the
//! two bytes `[version=0, addons_len=0]`, prefixed before the first payload
//! write; see `sing_vless/service.go::serverConn.Write`.
//!
//! Out of scope for this card, deliberately rejected rather than guessed:
//! UDP / Mux commands, XTLS vision addons, packet-addr / XUDP, user hot-reload.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use tokio::io::{AsyncRead, AsyncReadExt};

use crate::error::{Result, ServerError};

/// VLESS protocol version understood by this crate.
pub const VERSION: u8 = 0;
/// `sing_vless` command values (note: 1-based, unlike SOCKS).
pub const COMMAND_TCP: u8 = 1;
pub const COMMAND_UDP: u8 = 2;
pub const COMMAND_MUX: u8 = 3;
/// `vmess.AddressSerializer` address types.
pub const ADDRESS_IPV4: u8 = 1;
pub const ADDRESS_DOMAIN: u8 = 2;
pub const ADDRESS_IPV6: u8 = 3;

/// Prefix written once, before the first relayed byte of a response.
pub const RESPONSE_HEADER: [u8; 2] = [VERSION, 0];

const HEAD_LEN: usize = 18;
const TAIL_LEN: usize = 4; // command(1) + port(2) + address_type(1)

/// Destination carried in a VLESS request header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Destination {
    Ip(IpAddr),
    Domain(String),
}

/// Decoded VLESS request header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VlessRequest {
    pub version: u8,
    pub user_id: [u8; 16],
    pub addons: Vec<u8>,
    pub command: u8,
    pub port: u16,
    pub destination: Destination,
}

impl VlessRequest {
    /// Decode one complete request header from `buf`.
    ///
    /// Returns the request and the number of bytes consumed, so callers that
    /// over-read from a socket can hand the remainder to the relay.
    pub fn decode(buf: &[u8]) -> Result<(Self, usize)> {
        if buf.len() < HEAD_LEN {
            return Err(ServerError::TruncatedHeader);
        }
        if buf[0] != VERSION {
            return Err(ServerError::UnsupportedVersion(buf[0]));
        }

        let mut user_id = [0u8; 16];
        user_id.copy_from_slice(&buf[1..17]);
        let addons_len = usize::from(buf[17]);
        let mut offset = HEAD_LEN;
        let header_end = offset
            .checked_add(addons_len)
            .and_then(|value| value.checked_add(TAIL_LEN))
            .ok_or(ServerError::TruncatedHeader)?;
        if buf.len() < header_end {
            return Err(ServerError::TruncatedHeader);
        }

        let addons = buf[offset..offset + addons_len].to_vec();
        offset += addons_len;
        let command = buf[offset];
        let port = u16::from_be_bytes([buf[offset + 1], buf[offset + 2]]);
        let address_type = buf[offset + 3];
        offset += TAIL_LEN;

        let destination = match address_type {
            ADDRESS_IPV4 => {
                let bytes: [u8; 4] = buf
                    .get(offset..offset + 4)
                    .ok_or(ServerError::TruncatedHeader)?
                    .try_into()
                    .map_err(|_| ServerError::TruncatedHeader)?;
                offset += 4;
                Destination::Ip(IpAddr::V4(Ipv4Addr::from(bytes)))
            }
            ADDRESS_IPV6 => {
                let bytes: [u8; 16] = buf
                    .get(offset..offset + 16)
                    .ok_or(ServerError::TruncatedHeader)?
                    .try_into()
                    .map_err(|_| ServerError::TruncatedHeader)?;
                offset += 16;
                Destination::Ip(IpAddr::V6(Ipv6Addr::from(bytes)))
            }
            ADDRESS_DOMAIN => {
                let length = usize::from(*buf.get(offset).ok_or(ServerError::TruncatedHeader)?);
                offset += 1;
                if length == 0 {
                    return Err(ServerError::InvalidDomain);
                }
                let raw = buf
                    .get(offset..offset + length)
                    .ok_or(ServerError::TruncatedHeader)?;
                let name = std::str::from_utf8(raw).map_err(|_| ServerError::InvalidDomain)?;
                offset += length;
                Destination::Domain(name.to_owned())
            }
            other => return Err(ServerError::InvalidAddressType(other)),
        };

        Ok((
            Self {
                version: buf[0],
                user_id,
                addons,
                command,
                port,
                destination,
            },
            offset,
        ))
    }

    /// Constant-time comparison against every configured user.
    pub fn authenticate(&self, users: &[[u8; 16]]) -> bool {
        let mut matched = false;
        for user in users {
            let mut difference = 0u8;
            for (actual, expected) in self.user_id.iter().zip(user.iter()) {
                difference |= actual ^ expected;
            }
            matched |= difference == 0;
        }
        matched
    }
}

/// Read exactly one request header from a stream, leaving payload bytes unread.
pub async fn read_request<S>(stream: &mut S) -> Result<VlessRequest>
where
    S: AsyncRead + Unpin,
{
    let mut head = [0u8; HEAD_LEN];
    stream.read_exact(&mut head).await?;

    let addons_len = usize::from(head[17]);
    let mut buf = Vec::with_capacity(HEAD_LEN + addons_len + TAIL_LEN + 256);
    buf.extend_from_slice(&head);
    buf.resize(HEAD_LEN + addons_len, 0);
    stream.read_exact(&mut buf[HEAD_LEN..]).await?;

    let mut tail = [0u8; TAIL_LEN];
    stream.read_exact(&mut tail).await?;
    buf.extend_from_slice(&tail);

    match tail[3] {
        ADDRESS_IPV4 => {
            let mut address = [0u8; 4];
            stream.read_exact(&mut address).await?;
            buf.extend_from_slice(&address);
        }
        ADDRESS_IPV6 => {
            let mut address = [0u8; 16];
            stream.read_exact(&mut address).await?;
            buf.extend_from_slice(&address);
        }
        ADDRESS_DOMAIN => {
            let mut length = [0u8; 1];
            stream.read_exact(&mut length).await?;
            buf.extend_from_slice(&length);
            let mut address = vec![0u8; usize::from(length[0])];
            stream.read_exact(&mut address).await?;
            buf.extend_from_slice(&address);
        }
        _ => return Err(ServerError::InvalidAddressType(tail[3])),
    }

    let (request, _) = VlessRequest::decode(&buf)?;
    Ok(request)
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER: [u8; 16] = [
        0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e,
        0x0f,
    ];
    const OTHER_USER: [u8; 16] = [0xff; 16];

    fn request(command: u8, address_type: u8, address: &[u8], addons: &[u8]) -> Vec<u8> {
        let mut buf = vec![VERSION];
        buf.extend_from_slice(&USER);
        buf.push(addons.len() as u8);
        buf.extend_from_slice(addons);
        buf.push(command);
        buf.extend_from_slice(&443u16.to_be_bytes());
        buf.push(address_type);
        buf.extend_from_slice(address);
        buf
    }

    #[test]
    fn decodes_ipv4_tcp_request() {
        let bytes = request(COMMAND_TCP, ADDRESS_IPV4, &[127, 0, 0, 1], &[]);
        let (decoded, used) = VlessRequest::decode(&bytes).expect("valid request");
        assert_eq!(used, bytes.len());
        assert_eq!(decoded.user_id, USER);
        assert_eq!(decoded.command, COMMAND_TCP);
        assert_eq!(decoded.port, 443);
        assert_eq!(
            decoded.destination,
            Destination::Ip(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)))
        );
    }

    #[test]
    fn decodes_domain_request_with_addons() {
        let bytes = request(
            COMMAND_TCP,
            ADDRESS_DOMAIN,
            &[7, b'e', b'x', b'a', b'm', b'p', b'l', b'e'],
            &[1, 2, 3],
        );
        let (decoded, used) = VlessRequest::decode(&bytes).expect("valid request");
        assert_eq!(used, bytes.len());
        assert_eq!(decoded.addons, vec![1, 2, 3]);
        assert_eq!(
            decoded.destination,
            Destination::Domain("example".to_owned())
        );
    }

    #[test]
    fn decodes_ipv6_request() {
        let address: Vec<u8> = Ipv6Addr::LOCALHOST.octets().to_vec();
        let bytes = request(COMMAND_TCP, ADDRESS_IPV6, &address, &[]);
        let (decoded, used) = VlessRequest::decode(&bytes).expect("valid request");
        assert_eq!(used, bytes.len());
        assert_eq!(
            decoded.destination,
            Destination::Ip(IpAddr::V6(Ipv6Addr::LOCALHOST))
        );
    }

    #[test]
    fn decode_returns_consumed_length_for_trailing_payload() {
        let mut bytes = request(COMMAND_TCP, ADDRESS_IPV4, &[127, 0, 0, 1], &[]);
        bytes.extend_from_slice(b"payload");
        let (decoded, used) = VlessRequest::decode(&bytes).expect("valid request");
        assert_eq!(&bytes[used..], b"payload");
        assert_eq!(decoded.port, 443);
    }

    #[test]
    fn rejects_unsupported_version() {
        let mut bytes = request(COMMAND_TCP, ADDRESS_IPV4, &[127, 0, 0, 1], &[]);
        bytes[0] = 1;
        assert!(matches!(
            VlessRequest::decode(&bytes),
            Err(ServerError::UnsupportedVersion(1))
        ));
    }

    #[test]
    fn rejects_truncated_header() {
        assert!(matches!(
            VlessRequest::decode(&[VERSION, 0, 1]),
            Err(ServerError::TruncatedHeader)
        ));
    }

    #[test]
    fn rejects_truncated_address() {
        let mut bytes = request(COMMAND_TCP, ADDRESS_IPV4, &[127, 0, 0, 1], &[]);
        bytes.truncate(bytes.len() - 1);
        assert!(matches!(
            VlessRequest::decode(&bytes),
            Err(ServerError::TruncatedHeader)
        ));
    }

    #[test]
    fn rejects_unknown_address_type() {
        let bytes = request(COMMAND_TCP, 9, &[127, 0, 0, 1], &[]);
        assert!(matches!(
            VlessRequest::decode(&bytes),
            Err(ServerError::InvalidAddressType(9))
        ));
    }

    #[test]
    fn rejects_empty_domain() {
        let bytes = request(COMMAND_TCP, ADDRESS_DOMAIN, &[0], &[]);
        assert!(matches!(
            VlessRequest::decode(&bytes),
            Err(ServerError::InvalidDomain)
        ));
    }

    #[test]
    fn decodes_udp_command_without_executing_it() {
        let bytes = request(COMMAND_UDP, ADDRESS_IPV4, &[8, 8, 8, 8], &[]);
        let (decoded, _) = VlessRequest::decode(&bytes).expect("parseable command");
        assert_eq!(decoded.command, COMMAND_UDP);
    }

    #[test]
    fn authenticates_known_user() {
        let bytes = request(COMMAND_TCP, ADDRESS_IPV4, &[127, 0, 0, 1], &[]);
        let (decoded, _) = VlessRequest::decode(&bytes).expect("valid request");
        assert!(decoded.authenticate(&[USER]));
        assert!(decoded.authenticate(&[OTHER_USER, USER]));
    }

    #[test]
    fn rejects_unknown_user() {
        let bytes = request(COMMAND_TCP, ADDRESS_IPV4, &[127, 0, 0, 1], &[]);
        let (decoded, _) = VlessRequest::decode(&bytes).expect("valid request");
        assert!(!decoded.authenticate(&[]));
        assert!(!decoded.authenticate(&[OTHER_USER]));
    }

    #[test]
    fn response_header_is_version_zero_addons_zero() {
        assert_eq!(RESPONSE_HEADER, [0, 0]);
    }
}
