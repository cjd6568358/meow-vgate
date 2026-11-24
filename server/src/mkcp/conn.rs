//! mKCP connection — port of mihomo v1.19.34 `transport/internet/kcp/connection.go`.
//!
//! This is the glue layer joining the sending window, receiving window, and
//! RTO estimator. Unlike the Zig reference (which uses futex-driven threads
//! for synchronous streams), this version is driven explicitly by `tick()`
//! from a tokio timer, which fits the async listener model.

use crate::mkcp::receiving::ReceivingWindow;
use crate::mkcp::rto::RoundTripInfo;
use crate::mkcp::segment::{AckFields, Command, Segment, DATA_OVERHEAD, OPTION_CLOSE};
use crate::mkcp::window::SendingWindow;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Active,
    ReadyToClose,
    PeerClosed,
    Terminating,
    PeerTerminating,
    Terminated,
}

#[derive(Debug, Clone)]
pub struct KcpConfig {
    pub conv: u16,
    pub mtu: u32,
    pub tti: u32,
    pub sending_in_flight: u32,
    pub receiving_in_flight: u32,
    pub sending_buffer_size: u32,
    pub cwnd_multiplier: u32,
}

impl KcpConfig {
    pub fn mss(&self) -> u32 {
        self.mtu - DATA_OVERHEAD as u32
    }

    /// Derive the standard mKCP config from the YAML fields.
    pub fn from_inbound(
        conv: u16,
        mtu: u32,
        tti: u32,
        uplink_capacity: u32,
        downlink_capacity: u32,
        write_buffer: u32,
    ) -> Self {
        let mtu = if mtu == 0 { 1350 } else { mtu };
        let tti = if tti == 0 { 50 } else { tti };
        let uplink = if uplink_capacity == 0 {
            5
        } else {
            uplink_capacity
        };
        let downlink = if downlink_capacity == 0 {
            20
        } else {
            downlink_capacity
        };
        let write_buffer = if write_buffer == 0 {
            2 * 1024 * 1024
        } else {
            write_buffer
        };
        let sending_in_flight = (uplink * 1024 * 1024 / mtu / (1000 / tti)).max(8);
        let receiving_in_flight = (downlink * 1024 * 1024 / mtu / (1000 / tti)).max(8);
        Self {
            conv,
            mtu,
            tti,
            sending_in_flight,
            receiving_in_flight,
            sending_buffer_size: write_buffer / mtu,
            cwnd_multiplier: 1,
        }
    }
}

/// A transport-independent KCP connection. `peek_output()` returns segments
/// the caller must send as UDP datagrams; `input()` feeds received segments.
#[derive(Debug, Clone)]
pub struct KcpConnection {
    config: KcpConfig,
    state: State,
    state_begin: u32,
    last_incoming: u32,
    last_ping: u32,
    rto: RoundTripInfo,
    sending: SendingWindow,
    receiving: ReceivingWindow,
    output: Vec<Segment>,
    /// Epoch-ms value observed on first input/tick; all internal clocks are
    /// relative to this so segment timeouts and RTT checks match Go's
    /// small `elapsed()` values. `None` until the first observation.
    clock_epoch_base: Option<u32>,
}

impl KcpConnection {
    pub fn new(config: KcpConfig) -> Self {
        let rto = RoundTripInfo::new(config.tti);
        let mut sending = SendingWindow::new(
            config.sending_in_flight,
            config.sending_buffer_size,
            config.cwnd_multiplier,
        );
        sending.rto_snapshot = rto.timeout();
        let receiving = ReceivingWindow::new(config.receiving_in_flight, config.mss());
        Self {
            config,
            state: State::Active,
            state_begin: 0,
            last_incoming: 0,
            last_ping: 0,
            rto,
            sending,
            receiving,
            output: Vec::new(),
            clock_epoch_base: None,
        }
    }

    pub fn conv(&self) -> u16 {
        self.config.conv
    }

    pub fn state(&self) -> State {
        self.state
    }

    fn set_state(&mut self, new_state: State, current: u32) {
        if self.state != new_state {
            self.state = new_state;
            self.state_begin = current;
        }
    }

    fn handle_option(&mut self, option: u8, current: u32) {
        if option & OPTION_CLOSE == 0 {
            return;
        }
        match self.state {
            State::Active | State::PeerClosed => self.set_state(State::PeerTerminating, current),
            State::ReadyToClose => self.set_state(State::Terminating, current),
            State::Terminating => self.set_state(State::Terminated, current),
            _ => {}
        }
    }

    /// Convert an epoch-millisecond timestamp into a small relative clock.
    /// Go's KCP uses `elapsed()` (ms since connection start); passing raw
    /// epoch-ms breaks the retransmit check `current - timeout >= HALF`,
    /// because a fresh segment's `timeout == 0` then looks "in the future"
    /// and every send is skipped. We pin a baseline on first use instead.
    fn init_clock(&mut self, epoch_ms: u32) -> u32 {
        let base = *self.clock_epoch_base.get_or_insert(epoch_ms);
        epoch_ms.wrapping_sub(base)
    }

    pub fn input(&mut self, segments: &[Segment], epoch_ms: u32) {
        let current = self.init_clock(epoch_ms);
        self.last_incoming = current;
        for segment in segments {
            if segment.conv() != self.config.conv {
                break;
            }
            let option = match segment {
                Segment::Data { option, .. } => *option,
                Segment::Ack { option, .. } => *option,
                Segment::Command { option, .. } => *option,
            };
            self.handle_option(option, current);

            match segment {
                Segment::Data {
                    number,
                    timestamp,
                    sending_next,
                    payload,
                    ..
                } => {
                    self.receiving
                        .process_segment(*number, *timestamp, *sending_next, payload);
                }
                Segment::Ack {
                    receiving_window,
                    receiving_next,
                    timestamp,
                    numbers,
                    ..
                } => {
                    self.sending.process_segment(
                        current,
                        &AckFields {
                            receiving_window: *receiving_window,
                            receiving_next: *receiving_next,
                            timestamp: *timestamp,
                            numbers: numbers.clone(),
                        },
                        &mut self.rto,
                    );
                }
                Segment::Command {
                    command,
                    sending_next,
                    receiving_next,
                    peer_rto,
                    ..
                } => {
                    if *command == Command::Terminate {
                        match self.state {
                            State::Active | State::PeerClosed => {
                                self.set_state(State::PeerTerminating, current)
                            }
                            State::ReadyToClose => self.set_state(State::Terminating, current),
                            State::Terminating => self.set_state(State::Terminated, current),
                            _ => {}
                        }
                    }
                    self.sending.process_receiving_next(*receiving_next);
                    self.receiving.process_sending_next(*sending_next);
                    self.rto.update_peer_rto(*peer_rto, current);
                }
            }
        }
    }

    /// Queue application data (segmented to MSS) into the sending window.
    pub fn write(&mut self, data: &[u8]) -> usize {
        let mss = self.config.mss() as usize;
        let mut offset = 0;
        while offset < data.len() && self.state == State::Active {
            let end = (offset + mss).min(data.len());
            if !self.sending.push(&data[offset..end]) {
                break;
            }
            offset = end;
        }
        offset
    }

    /// Read reassembled in-order data. Returns 0 when none is available.
    pub fn read(&mut self, out: &mut [u8]) -> usize {
        self.receiving.read(out)
    }

    pub fn has_data(&self) -> bool {
        self.receiving.has_data()
    }

    fn ping(&mut self, current: u32, command: Command) {
        let option = if self.state == State::ReadyToClose {
            OPTION_CLOSE
        } else {
            0
        };
        self.output.push(Segment::Command {
            conv: self.config.conv,
            command,
            option,
            sending_next: self.sending.first_unacknowledged,
            receiving_next: self.receiving.next_number,
            peer_rto: self.rto.timeout(),
        });
        self.last_ping = current;
    }

    /// Advance the state machine and produce outgoing segments. Called on a
    /// TTI timer by the listener. Returns segments to send as UDP datagrams.
    pub fn tick(&mut self, epoch_ms: u32) -> Vec<Segment> {
        let current = self.init_clock(epoch_ms);
        self.output.clear();
        if self.state == State::Terminated {
            return Vec::new();
        }
        if self.state == State::Active && current.wrapping_sub(self.last_incoming) >= 30_000 {
            self.set_state(State::ReadyToClose, current);
        }
        if self.state == State::ReadyToClose && self.sending.is_empty() {
            self.set_state(State::Terminating, current);
        }
        if self.state == State::Terminating {
            self.ping(current, Command::Terminate);
            if current.wrapping_sub(self.state_begin) > 8_000 {
                self.set_state(State::Terminated, current);
            }
            return std::mem::take(&mut self.output);
        }
        if self.state == State::PeerTerminating && current.wrapping_sub(self.state_begin) > 4_000 {
            self.set_state(State::Terminating, current);
        }
        if self.state == State::ReadyToClose && current.wrapping_sub(self.state_begin) > 15_000 {
            self.set_state(State::Terminating, current);
        }

        self.sending.rto_snapshot = self.rto.timeout();
        let conv = self.config.conv;
        let mut data_segments = Vec::new();
        let ping_needed = self
            .sending
            .flush(current, &mut |number, first, option, payload| {
                data_segments.push(Segment::Data {
                    conv,
                    option,
                    timestamp: current,
                    number,
                    sending_next: first,
                    payload: payload.to_vec(),
                });
            });
        self.output.extend(data_segments);

        let rto = self.rto.timeout();
        for (fields, option) in self.receiving.flush_acks(current, rto) {
            self.output.push(Segment::Ack {
                conv,
                option,
                receiving_window: fields.receiving_window,
                receiving_next: fields.receiving_next,
                timestamp: current,
                numbers: fields.numbers,
            });
        }
        if ping_needed || current.wrapping_sub(self.last_ping) >= 3_000 {
            self.ping(current, Command::Ping);
        }
        std::mem::take(&mut self.output)
    }

    pub fn close(&mut self, current: u32) {
        self.set_state(State::ReadyToClose, current);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(conv: u16) -> KcpConfig {
        KcpConfig {
            conv,
            mtu: 1250,
            tti: 20,
            sending_in_flight: 8,
            receiving_in_flight: 8,
            sending_buffer_size: 512,
            cwnd_multiplier: 1,
        }
    }

    #[test]
    fn write_emits_data_on_tick() {
        let mut conn = KcpConnection::new(config(1));
        assert_eq!(conn.write(b"hello"), 5);
        let segments = conn.tick(1000);
        assert_eq!(segments.len(), 1);
        match &segments[0] {
            Segment::Data {
                number, payload, ..
            } => {
                assert_eq!(*number, 0);
                assert_eq!(payload, b"hello");
            }
            other => panic!("expected DATA, got {other:?}"),
        }
    }

    #[test]
    fn two_connections_exchange_data_loopback() {
        // A sends to B through an in-memory "UDP" pipe.
        let mut a = KcpConnection::new(config(1));
        let mut b = KcpConnection::new(config(1));
        a.write(b"ping over kcp");
        let outbound = a.tick(1000);

        b.input(&outbound, 1000);
        let mut received = [0u8; 64];
        let n = b.read(&mut received);
        assert_eq!(&received[..n], b"ping over kcp");

        // B's ACK goes back to A; A's window drains.
        let acks = b.tick(1010);
        a.input(&acks, 1010);
        assert!(a.sending.is_empty());
    }

    #[test]
    fn bidirectional_exchange_with_epoch_timestamps() {
        // Use epoch-millis-like timestamps (the listener passes now_millis()).
        // Small test timestamps (1000/1010) can hide wrapping-time bugs.
        // Real epoch-ms truncated to u32 is > HALF (0x7FFF_FFFF) in 2026;
        // the connection must convert it to a relative clock internally.
        let t0: u32 = 0xB296_8400;
        let mut a = KcpConnection::new(config(7));
        let mut b = KcpConnection::new(config(7));

        // A -> B request.
        a.write(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n");
        let to_b = a.tick(t0);
        assert!(!to_b.is_empty(), "A must emit request segments");
        b.input(&to_b, t0 + 1);
        let mut req = [0u8; 128];
        let n = b.read(&mut req);
        assert_eq!(&req[..n], b"GET / HTTP/1.1\r\nHost: x\r\n\r\n");

        // B -> A response (this is what the e2e stalls on).
        b.write(b"HTTP/1.1 200 OK\r\n\r\nbody");
        let to_a = b.tick(t0 + 2);
        assert!(!to_a.is_empty(), "B must emit response segments");
        a.input(&to_a, t0 + 3);
        let mut resp = [0u8; 128];
        let n = a.read(&mut resp);
        assert_eq!(&resp[..n], b"HTTP/1.1 200 OK\r\n\r\nbody");
    }

    #[test]
    fn config_defaults_match_go() {
        let cfg = KcpConfig::from_inbound(7, 0, 0, 0, 0, 0);
        assert_eq!(cfg.mtu, 1350);
        assert_eq!(cfg.tti, 50);
        // Go: 5MiB/1350/(1000/50) = 194; downlink 20MiB -> 776.
        // The floor of 8 only applies when the computed value is smaller.
        assert_eq!(cfg.sending_in_flight, 194);
        assert_eq!(cfg.receiving_in_flight, 776);
    }

    #[test]
    fn terminated_connection_emits_nothing() {
        let mut conn = KcpConnection::new(config(1));
        conn.set_state(State::Terminated, 1000);
        assert!(conn.tick(2000).is_empty());
    }
}
