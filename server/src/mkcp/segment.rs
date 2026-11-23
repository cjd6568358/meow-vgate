//! mKCP wire segments — byte-exact port of mihomo v1.19.34
//! `transport/internet/kcp/segment.go` via `zig-docs/source/transport/kcp/segment.zig`.
//!
//! All integers are BIG-ENDIAN. `conv` is u16 (not the u32 used by the
//! unrelated kcptun crate and off-the-shelf Rust KCP libraries), which is why
//! this cannot be replaced by a generic KCP implementation.
//!
//! A single UDP datagram may carry multiple back-to-back segments.

use std::fmt;

pub const DATA_OVERHEAD: usize = 18; // conv2 + cmd1 + opt1 + ts4 + num4 + snext4 + len2
pub const ACK_NUMBER_LIMIT: usize = 128;
pub const OPTION_CLOSE: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Ack,
    Data,
    Terminate,
    Ping,
    Unknown(u8),
}

impl Command {
    pub fn from_byte(byte: u8) -> Self {
        match byte {
            0 => Self::Ack,
            1 => Self::Data,
            2 => Self::Terminate,
            3 => Self::Ping,
            other => Self::Unknown(other),
        }
    }

    pub fn to_byte(self) -> u8 {
        match self {
            Self::Ack => 0,
            Self::Data => 1,
            Self::Terminate => 2,
            Self::Ping => 3,
            Self::Unknown(other) => other,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Segment {
    Data {
        conv: u16,
        option: u8,
        timestamp: u32,
        number: u32,
        sending_next: u32,
        payload: Vec<u8>,
    },
    Ack {
        conv: u16,
        option: u8,
        receiving_window: u32,
        receiving_next: u32,
        timestamp: u32,
        numbers: Vec<u32>,
    },
    /// TERMINATE / PING / unknown commands share a 16-byte layout.
    Command {
        conv: u16,
        command: Command,
        option: u8,
        sending_next: u32,
        receiving_next: u32,
        peer_rto: u32,
    },
}

impl Segment {
    pub fn conv(&self) -> u16 {
        match self {
            Self::Data { conv, .. } | Self::Ack { conv, .. } | Self::Command { conv, .. } => *conv,
        }
    }

    pub fn command(&self) -> Command {
        match self {
            Self::Data { .. } => Command::Data,
            Self::Ack { .. } => Command::Ack,
            Self::Command { command, .. } => *command,
        }
    }

    pub fn serialized_len(&self) -> usize {
        match self {
            Self::Data { payload, .. } => DATA_OVERHEAD + payload.len(),
            Self::Ack { numbers, .. } => 17 + numbers.len() * 4,
            Self::Command { .. } => 16,
        }
    }

    pub fn serialize(&self, out: &mut Vec<u8>) {
        match self {
            Self::Data {
                conv,
                option,
                timestamp,
                number,
                sending_next,
                payload,
            } => {
                out.extend_from_slice(&conv.to_be_bytes());
                out.push(Command::Data.to_byte());
                out.push(*option);
                out.extend_from_slice(&timestamp.to_be_bytes());
                out.extend_from_slice(&number.to_be_bytes());
                out.extend_from_slice(&sending_next.to_be_bytes());
                out.extend_from_slice(&(payload.len() as u16).to_be_bytes());
                out.extend_from_slice(payload);
            }
            Self::Ack {
                conv,
                option,
                receiving_window,
                receiving_next,
                timestamp,
                numbers,
            } => {
                out.extend_from_slice(&conv.to_be_bytes());
                out.push(Command::Ack.to_byte());
                out.push(*option);
                out.extend_from_slice(&receiving_window.to_be_bytes());
                out.extend_from_slice(&receiving_next.to_be_bytes());
                out.extend_from_slice(&timestamp.to_be_bytes());
                out.push(numbers.len() as u8);
                for number in numbers {
                    out.extend_from_slice(&number.to_be_bytes());
                }
            }
            Self::Command {
                conv,
                command,
                option,
                sending_next,
                receiving_next,
                peer_rto,
            } => {
                out.extend_from_slice(&conv.to_be_bytes());
                out.push(command.to_byte());
                out.push(*option);
                out.extend_from_slice(&sending_next.to_be_bytes());
                out.extend_from_slice(&receiving_next.to_be_bytes());
                out.extend_from_slice(&peer_rto.to_be_bytes());
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentError {
    NeedMore,
    InvalidLength,
}

impl fmt::Display for SegmentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NeedMore => write!(f, "need more bytes"),
            Self::InvalidLength => write!(f, "invalid segment length"),
        }
    }
}

/// Parse the first segment from `buf`, returning it and the number of bytes
/// consumed. An incomplete segment yields `NeedMore`; callers stop parsing
/// the datagram at that point (mihomo listener.go:75-77 semantics).
pub fn parse(buf: &[u8]) -> Result<(Segment, usize), SegmentError> {
    if buf.len() < 4 {
        return Err(SegmentError::NeedMore);
    }
    let conv = u16::from_be_bytes([buf[0], buf[1]]);
    let command = Command::from_byte(buf[2]);
    let option = buf[3];

    match command {
        Command::Data => {
            if buf.len() < DATA_OVERHEAD {
                return Err(SegmentError::NeedMore);
            }
            let timestamp = u32::from_be_bytes(buf[4..8].try_into().unwrap());
            let number = u32::from_be_bytes(buf[8..12].try_into().unwrap());
            let sending_next = u32::from_be_bytes(buf[12..16].try_into().unwrap());
            let length = u16::from_be_bytes([buf[16], buf[17]]) as usize;
            let total = DATA_OVERHEAD + length;
            if buf.len() < total {
                return Err(SegmentError::NeedMore);
            }
            Ok((
                Segment::Data {
                    conv,
                    option,
                    timestamp,
                    number,
                    sending_next,
                    payload: buf[DATA_OVERHEAD..total].to_vec(),
                },
                total,
            ))
        }
        Command::Ack => {
            if buf.len() < 17 {
                return Err(SegmentError::NeedMore);
            }
            let receiving_window = u32::from_be_bytes(buf[4..8].try_into().unwrap());
            let receiving_next = u32::from_be_bytes(buf[8..12].try_into().unwrap());
            let timestamp = u32::from_be_bytes(buf[12..16].try_into().unwrap());
            let count = buf[16] as usize;
            if count > ACK_NUMBER_LIMIT {
                return Err(SegmentError::InvalidLength);
            }
            let total = 17 + count * 4;
            if buf.len() < total {
                return Err(SegmentError::NeedMore);
            }
            let numbers = (0..count)
                .map(|i| u32::from_be_bytes(buf[17 + i * 4..21 + i * 4].try_into().unwrap()))
                .collect();
            Ok((
                Segment::Ack {
                    conv,
                    option,
                    receiving_window,
                    receiving_next,
                    timestamp,
                    numbers,
                },
                total,
            ))
        }
        _ => {
            if buf.len() < 16 {
                return Err(SegmentError::NeedMore);
            }
            let sending_next = u32::from_be_bytes(buf[4..8].try_into().unwrap());
            let receiving_next = u32::from_be_bytes(buf[8..12].try_into().unwrap());
            let peer_rto = u32::from_be_bytes(buf[12..16].try_into().unwrap());
            Ok((
                Segment::Command {
                    conv,
                    command,
                    option,
                    sending_next,
                    receiving_next,
                    peer_rto,
                },
                16,
            ))
        }
    }
}

/// Parse all back-to-back segments in a datagram, stopping at the first
/// incomplete or invalid one (already-parsed segments are still returned).
pub fn parse_all(buf: &[u8]) -> Vec<Segment> {
    let mut out = Vec::new();
    let mut offset = 0;
    while offset < buf.len() {
        match parse(&buf[offset..]) {
            Ok((segment, consumed)) => {
                offset += consumed;
                out.push(segment);
            }
            Err(_) => break,
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_segment_roundtrip() {
        let segment = Segment::Data {
            conv: 0x1234,
            option: 0,
            timestamp: 2000,
            number: 7,
            sending_next: 9,
            payload: b"hello".to_vec(),
        };
        let mut bytes = Vec::new();
        segment.serialize(&mut bytes);
        assert_eq!(bytes.len(), DATA_OVERHEAD + 5);
        assert_eq!(&bytes[..4], &[0x12, 0x34, 0x01, 0x00]);
        let (parsed, consumed) = parse(&bytes).unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(parsed, segment);
    }

    #[test]
    fn ack_segment_roundtrip() {
        let segment = Segment::Ack {
            conv: 0x1234,
            option: 0,
            receiving_window: 100,
            receiving_next: 42,
            timestamp: 2000,
            numbers: vec![1, 2, 3],
        };
        let mut bytes = Vec::new();
        segment.serialize(&mut bytes);
        assert_eq!(bytes.len(), 17 + 3 * 4);
        let (parsed, consumed) = parse(&bytes).unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(parsed, segment);
    }

    #[test]
    fn command_segment_roundtrip() {
        let segment = Segment::Command {
            conv: 0x1234,
            command: Command::Ping,
            option: 0,
            sending_next: 9,
            receiving_next: 11,
            peer_rto: 300,
        };
        let mut bytes = Vec::new();
        segment.serialize(&mut bytes);
        assert_eq!(bytes.len(), 16);
        let (parsed, consumed) = parse(&bytes).unwrap();
        assert_eq!(consumed, 16);
        assert_eq!(parsed, segment);
    }

    #[test]
    fn parse_all_stops_at_incomplete_tail() {
        let first = Segment::Data {
            conv: 1,
            option: 0,
            timestamp: 1,
            number: 1,
            sending_next: 1,
            payload: b"ok".to_vec(),
        };
        let mut bytes = Vec::new();
        first.serialize(&mut bytes);
        bytes.extend_from_slice(&[0, 2, 1]); // truncated second segment
        assert_eq!(parse_all(&bytes), vec![first]);
    }

    #[test]
    fn rejects_too_many_ack_numbers() {
        let bytes = vec![0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 200];
        assert_eq!(parse(&bytes), Err(SegmentError::InvalidLength));
    }
}
