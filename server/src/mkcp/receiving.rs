//! mKCP receiving window + ACK list — port of mihomo v1.19.34
//! `transport/internet/kcp/receiving.go` and the AckList in `connection.go`.
//!
//! Out-of-order segments are buffered in a HashMap keyed by sequence number;
//! `read` drains them in order into the caller buffer, keeping partial reads
//! in a leftover queue. Every accepted segment is queued for delayed ACK.

use std::collections::HashMap;

use crate::mkcp::segment::{AckFields, ACK_NUMBER_LIMIT, OPTION_CLOSE};

#[derive(Debug, Clone)]
struct AckEntry {
    number: u32,
    next_flush: u32,
}

/// Delayed-ACK list. Emits at most `(mss-17)/4` (capped 128) numbers per ACK
/// segment; already-sent numbers are retried no sooner than max(rto/2, 20ms).
#[derive(Debug, Clone)]
pub struct AckList {
    entries: Vec<AckEntry>,
    mss: u32,
    dirty: bool,
}

impl AckList {
    pub fn new(mss: u32) -> Self {
        Self {
            entries: Vec::new(),
            mss,
            dirty: false,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn limit(&self) -> usize {
        let mut limit = (self.mss.saturating_sub(17) / 4) as usize;
        if limit == 0 {
            limit = 1;
        }
        limit.min(ACK_NUMBER_LIMIT)
    }

    pub fn add(&mut self, number: u32, _timestamp: u32) {
        self.entries.push(AckEntry {
            number,
            next_flush: 0,
        });
        self.dirty = true;
    }

    /// Drop ACK entries below `una` (Go `clear`).
    pub fn clear(&mut self, una: u32) {
        let before = self.entries.len();
        self.entries.retain(|entry| entry.number >= una);
        if self.entries.len() != before {
            self.dirty = true;
        }
    }

    /// Produce ACK field sets to emit. Caller fills conv/receiving_next and
    /// receives one or more `AckFields` (never more than one per `flush` here
    /// because the caller owns MTU-level batching).
    pub fn flush(&mut self, current: u32, rto: u32, receiving_next: u32) -> Vec<AckFields> {
        let limit = self.limit();
        let delay = (rto / 2).max(20);
        let mut fields = AckFields {
            receiving_window: receiving_next.wrapping_add(1), // patched by caller
            receiving_next,
            timestamp: 0,
            numbers: Vec::new(),
        };
        let mut deferred = Vec::new();

        for entry in &mut self.entries {
            if entry.next_flush > current {
                if deferred.len() < ACK_NUMBER_LIMIT {
                    deferred.push(entry.number);
                }
                continue;
            }
            if fields.numbers.len() >= limit {
                break;
            }
            fields.numbers.push(entry.number);
            entry.next_flush = current.wrapping_add(delay);
        }

        if fields.numbers.is_empty() && self.dirty {
            for number in deferred {
                if fields.numbers.len() >= limit {
                    break;
                }
                fields.numbers.push(number);
            }
        }
        if !fields.numbers.is_empty() {
            self.dirty = false;
            vec![fields]
        } else {
            Vec::new()
        }
    }
}

/// Partial segment remainder from a read that did not fit in the caller's
/// buffer.
#[derive(Debug, Clone)]
struct PendingBuf {
    data: Vec<u8>,
    pos: usize,
}

/// Receiving sliding window: reorder buffer, ordered delivery, ACK list.
#[derive(Debug, Clone)]
pub struct ReceivingWindow {
    left_over: Vec<PendingBuf>,
    window: HashMap<u32, Vec<u8>>,
    pub ack_list: AckList,
    pub next_number: u32,
    window_size: u32,
    /// Set from connection state when generating ACK fields.
    pub ready_to_close: bool,
}

impl ReceivingWindow {
    pub fn new(receiving_in_flight: u32, mss: u32) -> Self {
        Self {
            left_over: Vec::new(),
            window: HashMap::new(),
            ack_list: AckList::new(mss),
            next_number: 0,
            window_size: receiving_in_flight,
            ready_to_close: false,
        }
    }

    /// Accept an inbound DATA segment. Returns true if it was newly buffered
    /// (false for duplicates or out-of-window numbers).
    pub fn process_segment(
        &mut self,
        number: u32,
        timestamp: u32,
        sending_next: u32,
        payload: &[u8],
    ) -> bool {
        let index = number.wrapping_sub(self.next_number);
        if index >= self.window_size {
            return false;
        }
        self.ack_list.clear(sending_next);
        self.ack_list.add(number, timestamp);
        if self.window.contains_key(&number) {
            return false;
        }
        self.window.insert(number, payload.to_vec());
        true
    }

    /// Read up to `out.len()` bytes of in-order data.
    pub fn read(&mut self, out: &mut [u8]) -> usize {
        let mut total = 0;
        let mut consumed = 0;
        while consumed < self.left_over.len() && total < out.len() {
            let pending = &mut self.left_over[consumed];
            let count = (out.len() - total).min(pending.data.len() - pending.pos);
            out[total..total + count]
                .copy_from_slice(&pending.data[pending.pos..pending.pos + count]);
            pending.pos += count;
            total += count;
            if pending.pos == pending.data.len() {
                consumed += 1;
            }
        }
        if consumed > 0 {
            self.left_over.drain(..consumed);
        }
        if total == out.len() {
            return total;
        }

        while total < out.len() {
            let Some(payload) = self.window.remove(&self.next_number) else {
                break;
            };
            self.next_number = self.next_number.wrapping_add(1);
            let count = (out.len() - total).min(payload.len());
            out[total..total + count].copy_from_slice(&payload[..count]);
            total += count;
            if count < payload.len() {
                self.left_over.push(PendingBuf {
                    data: payload[count..].to_vec(),
                    pos: 0,
                });
            }
        }
        total
    }

    pub fn has_data(&self) -> bool {
        self.window.contains_key(&self.next_number) || !self.left_over.is_empty()
    }

    /// Drop queued ACKs below the peer's reported send cursor.
    pub fn process_sending_next(&mut self, number: u32) {
        self.ack_list.clear(number);
    }

    /// Generate ACK segments to send. Returns (fields, close_option).
    pub fn flush_acks(&mut self, current: u32, rto: u32) -> Vec<(AckFields, u8)> {
        let next = self.next_number;
        let mut fields = self.ack_list.flush(current, rto, next);
        for field in &mut fields {
            field.receiving_window = next.wrapping_add(self.window_size);
        }
        let option = if self.ready_to_close { OPTION_CLOSE } else { 0 };
        fields.into_iter().map(|field| (field, option)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_order_delivery() {
        let mut window = ReceivingWindow::new(8, 1250);
        assert!(window.process_segment(0, 100, 0, b"hello"));
        assert!(window.process_segment(1, 100, 0, b" world"));
        let mut out = [0u8; 32];
        let n = window.read(&mut out);
        assert_eq!(&out[..n], b"hello world");
        assert_eq!(window.next_number, 2);
    }

    #[test]
    fn out_of_order_is_buffered_until_gap_fills() {
        let mut window = ReceivingWindow::new(8, 1250);
        assert!(window.process_segment(1, 100, 0, b"second"));
        let mut out = [0u8; 32];
        assert_eq!(window.read(&mut out), 0);
        assert!(window.process_segment(0, 100, 0, b"first-"));
        let n = window.read(&mut out);
        assert_eq!(&out[..n], b"first-second");
    }

    #[test]
    fn duplicate_segment_is_rejected() {
        let mut window = ReceivingWindow::new(8, 1250);
        assert!(window.process_segment(0, 100, 0, b"a"));
        assert!(!window.process_segment(0, 100, 0, b"a"));
        let mut out = [0u8; 8];
        let n = window.read(&mut out);
        assert_eq!(&out[..n], b"a");
    }

    #[test]
    fn out_of_window_is_dropped() {
        let mut window = ReceivingWindow::new(4, 1250);
        assert!(!window.process_segment(10, 100, 0, b"x"));
    }

    #[test]
    fn partial_read_keeps_leftover() {
        let mut window = ReceivingWindow::new(8, 1250);
        window.process_segment(0, 100, 0, b"abcdef");
        let mut small = [0u8; 2];
        let n = window.read(&mut small);
        assert_eq!(&small[..n], b"ab");
        let mut rest = [0u8; 8];
        let n = window.read(&mut rest);
        assert_eq!(&rest[..n], b"cdef");
    }

    #[test]
    fn ack_list_caps_numbers_per_segment() {
        let mut list = AckList::new(17 + 4 * 4); // limit = 4
        for i in 0..10 {
            list.add(i, 100);
        }
        let fields = list.flush(1000, 100, 0);
        assert_eq!(fields.len(), 1);
        assert_eq!(fields[0].numbers.len(), 4);
    }

    #[test]
    fn delayed_ack_retries_after_rto_half() {
        let mut list = AckList::new(1250);
        list.add(0, 100);
        let first = list.flush(1000, 100, 0); // delay 50 -> next_flush 1050
        assert_eq!(first[0].numbers, vec![0]);
        assert!(list.flush(1010, 100, 0).is_empty());
        let retry = list.flush(1060, 100, 0);
        assert_eq!(retry[0].numbers, vec![0]);
    }
}
