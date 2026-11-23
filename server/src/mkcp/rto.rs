//! mKCP RTO estimator — port of mihomo v1.19.34
//! `transport/internet/kcp/connection.go` RoundTripInfo (RFC6298).
//!
//! Byte-exact against the Go reference; the golden sequence from the Zig
//! reference (`conn.zig` "RoundTripInfo RFC6298") is locked in tests.

/// Round-trip time estimator. Uses wrapping arithmetic in the same places
/// Go does, so 32-bit timer wrap behaves identically.
#[derive(Debug, Clone)]
pub struct RoundTripInfo {
    pub variation: u32,
    pub srtt: u32,
    pub rto: u32,
    pub min_rtt: u32,
    pub updated_timestamp: u32,
}

impl RoundTripInfo {
    pub fn new(min_rtt: u32) -> Self {
        Self {
            variation: 0,
            srtt: 0,
            rto: 100,
            min_rtt,
            updated_timestamp: 0,
        }
    }

    /// Adopt a peer-advertised RTO, but ignore updates arriving within 3s of
    /// the previous one (Go's `current -% updated < 3000` guard).
    pub fn update_peer_rto(&mut self, rto: u32, current: u32) {
        if current.wrapping_sub(self.updated_timestamp) < 3000 {
            return;
        }
        self.updated_timestamp = current;
        self.rto = rto;
    }

    /// Feed a measured RTT (milliseconds).
    pub fn update(&mut self, rtt: u32, current: u32) {
        if rtt > 0x7FFF_FFFF {
            return;
        }
        if self.srtt == 0 {
            self.srtt = rtt;
            self.variation = rtt / 2;
        } else {
            let delta = rtt.wrapping_sub(self.srtt);
            let delta = if self.srtt > rtt {
                self.srtt.wrapping_sub(rtt)
            } else {
                delta
            };
            self.variation = (3u32.wrapping_mul(self.variation).wrapping_add(delta)) / 4;
            self.srtt = (7u32.wrapping_mul(self.srtt).wrapping_add(rtt)) / 8;
            if self.srtt < self.min_rtt {
                self.srtt = self.min_rtt;
            }
        }
        let rto = if self.min_rtt < 4u32.wrapping_mul(self.variation) {
            self.srtt.wrapping_add(4u32.wrapping_mul(self.variation))
        } else {
            self.srtt.wrapping_add(self.variation)
        };
        let capped = rto.min(10_000);
        self.rto = capped * 5 / 4;
        self.updated_timestamp = current;
    }

    pub fn timeout(&self) -> u32 {
        self.rto
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn golden_rfc6298_sequence() {
        let mut info = RoundTripInfo::new(20);
        // First: srtt=100 var=50 -> rto=100+200=300 -> *5/4=375
        info.update(100, 1000);
        assert_eq!(info.srtt, 100);
        assert_eq!(info.rto, 375);
        // Second: delta=100 var=(150+100)/4=62 srtt=112 rto=112+248=360 -> 450
        info.update(200, 2000);
        assert_eq!(info.srtt, 112);
        assert_eq!(info.rto, 450);
    }

    #[test]
    fn min_rtt_floors_srtt_on_subsequent_updates() {
        let mut info = RoundTripInfo::new(100);
        info.update(10, 1000);
        // First sample adopts RTT directly; the floor is applied only from
        // the second update onward (matching Go/Zig control flow).
        assert_eq!(info.srtt, 10);
        info.update(10, 2000);
        assert!(info.srtt >= 100);
    }

    #[test]
    fn peer_rto_ignored_within_3_seconds() {
        let mut info = RoundTripInfo::new(20);
        // updated_timestamp starts at 0, so a "current" below 3000 is within
        // the guard window and ignored — exactly the Go behaviour.
        info.update_peer_rto(500, 1000);
        assert_eq!(info.rto, 100);
        info.update_peer_rto(500, 3000); // 3000-0=3000 -> accepted
        assert_eq!(info.rto, 500);
        info.update_peer_rto(700, 5000); // 2000ms later -> ignored
        assert_eq!(info.rto, 500);
        info.update_peer_rto(700, 6000); // 3000ms later -> accepted
        assert_eq!(info.rto, 700);
    }

    #[test]
    fn absurd_rtt_is_ignored() {
        let mut info = RoundTripInfo::new(20);
        info.update(0x8000_0000, 1000);
        assert_eq!(info.srtt, 0);
        assert_eq!(info.rto, 100);
    }

    #[test]
    fn rto_is_capped_at_12500() {
        let mut info = RoundTripInfo::new(20);
        info.update(10_000, 1000);
        assert!(info.rto <= 12_500);
    }
}
