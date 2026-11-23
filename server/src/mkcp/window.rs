//! mKCP sending/receiving windows — port of mihomo v1.19.34
//! `transport/internet/kcp/sending.go` + `receiving.go` via the Zig reference.
//!
//! Kept free of Connection/runtime dependencies so window arithmetic can be
//! unit-tested against Go's wrapping semantics. Output segments are emitted
//! through a closure supplied by the caller (the UDP session layer).

use crate::mkcp::rto::RoundTripInfo;
use crate::mkcp::segment::{AckFields, ACK_NUMBER_LIMIT};

const HALF: u32 = 0x7FFF_FFFF;

/// A queued outbound data segment with retransmission state.
#[derive(Debug, Clone)]
struct Pending {
    number: u32,
    payload: Vec<u8>,
    /// Absolute timestamp (ms) at which this segment was last sent.
    timeout: u32,
    timestamp: u32,
    transmit: u32,
}

/// Sending sliding window: ordered by segment number, ACK compaction,
/// fast retransmit, RTT sampling, and congestion-window adjustment.
#[derive(Debug, Clone)]
pub struct SendingWindow {
    window: Vec<Pending>,
    pub first_unacknowledged: u32,
    pub next_number: u32,
    remote_next_number: u32,
    control_window: u32,
    window_size: u32,
    pub retransmits: u32,
    first_unacknowledged_updated: bool,
    total_in_flight_size: u32,
    closed: bool,
    /// Config: max in-flight segments advertised by peer side.
    sending_in_flight: u32,
    cwnd_multiplier: u32,
    /// RTO snapshot for the current flush cycle (set by the caller).
    pub rto_snapshot: u32,
    /// Ready-to-close state sets the CLOSE option on outgoing segments.
    pub ready_to_close: bool,
}

impl SendingWindow {
    pub fn new(sending_in_flight: u32, sending_buffer_size: u32, cwnd_multiplier: u32) -> Self {
        Self {
            window: Vec::new(),
            first_unacknowledged: 0,
            next_number: 0,
            remote_next_number: 32,
            control_window: sending_in_flight,
            window_size: sending_buffer_size,
            retransmits: 0,
            first_unacknowledged_updated: false,
            total_in_flight_size: 0,
            closed: false,
            sending_in_flight,
            cwnd_multiplier,
            rto_snapshot: 100,
            ready_to_close: false,
        }
    }

    /// Queue a payload into the window. False when closed/full.
    pub fn push(&mut self, payload: &[u8]) -> bool {
        if self.closed || self.window.len() > self.window_size as usize {
            return false;
        }
        self.window.push(Pending {
            number: self.next_number,
            payload: payload.to_vec(),
            timeout: 0,
            timestamp: 0,
            transmit: 0,
        });
        self.next_number = self.next_number.wrapping_add(1);
        true
    }

    fn find_first_unacknowledged(&mut self) {
        let first = self.first_unacknowledged;
        self.first_unacknowledged = self
            .window
            .first()
            .map_or(self.next_number, |seg| seg.number);
        if first != self.first_unacknowledged {
            self.first_unacknowledged_updated = true;
        }
    }

    /// Remove all segments with number < una (Go `clear`).
    fn clear(&mut self, una: u32) {
        while let Some(seg) = self.window.first() {
            if seg.number >= una {
                break;
            }
            if self.total_in_flight_size > 0 {
                self.total_in_flight_size -= 1;
            }
            self.window.remove(0);
        }
    }

    fn handle_fast_ack(&mut self, number: u32, rto: u32) {
        let third = rto / 3;
        for seg in &mut self.window {
            if number == seg.number || number.wrapping_sub(seg.number) > HALF {
                break;
            }
            if seg.transmit > 0 && seg.timeout > third {
                seg.timeout -= third;
            }
        }
    }

    /// Process a single ACK number; true if it removed a queued segment.
    fn process_ack(&mut self, number: u32) -> bool {
        if number.wrapping_sub(self.first_unacknowledged) > HALF
            || number.wrapping_sub(self.next_number) < HALF
        {
            return false;
        }
        let Some(index) = self.window.iter().position(|seg| seg.number == number) else {
            return false;
        };
        if self.total_in_flight_size > 0 {
            self.total_in_flight_size -= 1;
        }
        self.window.remove(index);
        self.find_first_unacknowledged();
        true
    }

    /// Apply an incoming ACK segment; optionally emits a PING when the
    /// first-unacknowledged cursor moved (caller supplies the current time).
    pub fn process_segment(&mut self, current: u32, ack: &AckFields, rto: &mut RoundTripInfo) {
        if self.closed {
            return;
        }
        if self.remote_next_number < ack.receiving_window {
            self.remote_next_number = ack.receiving_window;
        }
        self.clear(ack.receiving_next);
        self.find_first_unacknowledged();
        if ack.numbers.is_empty() {
            return;
        }

        let mut max_ack = 0u32;
        let mut max_ack_removed = false;
        for &number in ack.numbers.iter().take(ACK_NUMBER_LIMIT) {
            let removed = self.process_ack(number);
            if max_ack < number {
                max_ack = number;
                max_ack_removed = removed;
            }
        }
        if max_ack_removed {
            self.handle_fast_ack(max_ack, rto.timeout());
            if current.wrapping_sub(ack.timestamp) < 10_000 {
                rto.update(current.wrapping_sub(ack.timestamp), current);
            }
        }
    }

    fn on_packet_loss(&mut self, loss_rate: u32) {
        if self.sending_in_flight == 0 {
            return;
        }
        if loss_rate >= 15 {
            self.control_window = 3 * self.control_window / 4;
        }
        if loss_rate <= 5 {
            self.control_window = self.control_window.wrapping_add(self.control_window / 4);
        }
        if self.control_window < 16 {
            self.control_window = 16;
        }
        if self.control_window > self.sending_in_flight {
            self.control_window = self.sending_in_flight;
        }
    }

    /// Flush segments due for (re)transmission. `emit` receives (number,
    /// sending_next, option, payload). Returns whether a PING should be sent.
    pub fn flush<F>(&mut self, current: u32, emit: &mut F) -> bool
    where
        F: FnMut(u32, u32, u8, &[u8]),
    {
        if self.closed {
            return false;
        }
        let mut cwnd = self.sending_in_flight;
        if cwnd
            > self
                .remote_next_number
                .wrapping_sub(self.first_unacknowledged)
        {
            cwnd = self
                .remote_next_number
                .wrapping_sub(self.first_unacknowledged);
        }
        if cwnd > self.control_window {
            cwnd = self.control_window;
        }
        cwnd *= self.cwnd_multiplier;

        let rto = self.rto_snapshot;
        let mut lost = 0u32;
        let mut in_flight = 0u32;
        if !self.window.is_empty() {
            for seg in &mut self.window {
                if current.wrapping_sub(seg.timeout) >= HALF {
                    continue;
                }
                if seg.transmit == 0 {
                    self.total_in_flight_size += 1;
                } else {
                    lost += 1;
                    self.retransmits += 1;
                }
                seg.timeout = current.wrapping_add(rto);
                seg.timestamp = current;
                seg.transmit += 1;
                let option = if self.ready_to_close { 1 } else { 0 };
                emit(seg.number, self.first_unacknowledged, option, &seg.payload);
                in_flight += 1;
                if in_flight >= cwnd {
                    break;
                }
            }
            if in_flight > 0 && self.total_in_flight_size != 0 {
                self.on_packet_loss(lost * 100 / self.total_in_flight_size);
            }
        }
        let updated = self.first_unacknowledged_updated;
        self.first_unacknowledged_updated = false;
        updated
    }

    pub fn is_empty(&self) -> bool {
        self.window.is_empty()
    }

    pub fn close_write(&mut self) {
        self.clear(u32::MAX);
    }

    pub fn close(&mut self) {
        self.window.clear();
        self.closed = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_and_advances_window() {
        let mut window = SendingWindow::new(8, 512, 1);
        window.rto_snapshot = 100;
        assert!(window.push(b"a"));
        assert!(window.push(b"b"));
        assert_eq!(window.next_number, 2);

        let mut sent = Vec::new();
        window.flush(1000, &mut |num, first, opt, payload| {
            sent.push((num, first, opt, payload.to_vec()));
        });
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0].0, 0);
        assert_eq!(sent[0].1, 0);

        // ACK both segments -> window empties, first-unacked advances.
        let ack = AckFields {
            receiving_window: 100,
            receiving_next: 2,
            timestamp: 1000,
            numbers: vec![0, 1],
        };
        let mut rto = RoundTripInfo::new(20);
        window.process_segment(1100, &ack, &mut rto);
        assert!(window.is_empty());
        assert_eq!(window.first_unacknowledged, 2);
    }

    #[test]
    fn retransmits_unacked_segment() {
        let mut window = SendingWindow::new(8, 512, 1);
        window.rto_snapshot = 100;
        window.push(b"hello");
        let mut count = 0;
        window.flush(1000, &mut |_, _, _, _| count += 1);
        assert_eq!(count, 1);
        // Before RTO -> no retransmit.
        window.flush(1050, &mut |_, _, _, _| count += 1);
        assert_eq!(count, 1);
        // After RTO -> retransmit.
        window.flush(1200, &mut |_, _, _, _| count += 1);
        assert_eq!(count, 2);
        assert_eq!(window.retransmits, 1);
    }

    #[test]
    fn stale_ack_is_ignored() {
        let mut window = SendingWindow::new(8, 512, 1);
        window.rto_snapshot = 100;
        window.push(b"a");
        window.flush(1000, &mut |_, _, _, _| {});
        let ack = AckFields {
            receiving_window: 100,
            receiving_next: 0,
            timestamp: 1000,
            numbers: vec![99],
        };
        let mut rto = RoundTripInfo::new(20);
        window.process_segment(1100, &ack, &mut rto);
        assert!(!window.is_empty());
    }

    #[test]
    fn window_full_rejects_push() {
        let mut window = SendingWindow::new(8, 1, 1);
        assert!(window.push(b"a"));
        assert!(window.push(b"b"));
        assert!(!window.push(b"c"));
    }
}
