//! A minimal PostgreSQL wire-protocol frame reader: message **type bytes and lengths only**,
//! never query text.
//!
//! The proxy ([`crate::proxy`]) must cut a connection at a precise point in the protocol — when
//! the client sends its commit, or just after the server replies to it — without understanding
//! the SQL. It does not need to: every frontend and backend message carries a one-byte type and
//! a 32-bit big-endian length, and the commit boundary in the extended query protocol is the
//! frontend `Sync` (or the frontend `Query` in the simple protocol), whose reply ends with the
//! backend `ReadyForQuery`.
//!
//! [`FrameScanner`] consumes bytes in order and yields one [`Frame`] per complete message,
//! tracking the total byte count per direction. The first message on a fresh connection is the
//! untyped startup packet (an `Int32` length with no type byte); the scanner handles it and every
//! later message is typed. A frame's payload is skipped, never inspected.
//!
//! # Honest limit
//!
//! The scanner sees message *shapes*, not statements. A `Sync` is the commit boundary of a
//! prepared-statement exchange, but the same byte also ends a read-only exchange; a workload
//! that mixes reads and writes through one connection cannot be cut "on the commit" by shape
//! alone. The scenarios that need a precise commit boundary therefore use a single connection and
//! a write-only workload (see [`crate::scenarios`]).

use serde::{Deserialize, Serialize};

/// The largest frame the scanner accepts; a larger length is treated as a corrupt stream.
pub const MAX_FRAME_LEN: u32 = 256 * 1024 * 1024;

/// A PostgreSQL frontend/backend message type the harness names.
///
/// Only the types a fault schedule needs are named; every other byte is [`PgMessage::Other`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PgMessage {
    /// The untyped startup packet (`StartupMessage`, `SSLRequest` or `CancelRequest`).
    Startup,
    /// Frontend `Parse`.
    Parse,
    /// Frontend `Bind`.
    Bind,
    /// Frontend `Describe`.
    Describe,
    /// Frontend `Execute`.
    Execute,
    /// Frontend `Sync`: the end of a prepared-statement exchange, and so the commit boundary in
    /// the extended query protocol.
    Sync,
    /// Frontend `Query`: a simple-protocol statement, and so the commit boundary there.
    Query,
    /// Frontend `Close`.
    Close,
    /// Frontend `Terminate`.
    Terminate,
    /// Backend `CommandComplete`: the server finished a statement.
    CommandComplete,
    /// Backend `ReadyForQuery`: the server finished the whole exchange; the client's commit
    /// acknowledgement is complete once it arrives.
    ReadyForQuery,
    /// Backend `ErrorResponse`.
    ErrorResponse,
    /// Any other message type byte.
    Other(u8),
}

impl PgMessage {
    /// The message type byte, or `None` for the untyped startup packet.
    pub fn code(self) -> Option<u8> {
        match self {
            PgMessage::Startup => None,
            PgMessage::Parse => Some(b'P'),
            PgMessage::Bind => Some(b'B'),
            PgMessage::Describe => Some(b'D'),
            PgMessage::Execute => Some(b'E'),
            PgMessage::Sync => Some(b'S'),
            PgMessage::Query => Some(b'Q'),
            PgMessage::Close => Some(b'C'),
            PgMessage::Terminate => Some(b'X'),
            PgMessage::CommandComplete => Some(b'C'),
            PgMessage::ReadyForQuery => Some(b'Z'),
            PgMessage::ErrorResponse => Some(b'E'),
            PgMessage::Other(b) => Some(b),
        }
    }

    /// The name used in reports.
    pub fn name(self) -> &'static str {
        match self {
            PgMessage::Startup => "startup",
            PgMessage::Parse => "parse",
            PgMessage::Bind => "bind",
            PgMessage::Describe => "describe",
            PgMessage::Execute => "execute",
            PgMessage::Sync => "sync",
            PgMessage::Query => "query",
            PgMessage::Close => "close",
            PgMessage::Terminate => "terminate",
            PgMessage::CommandComplete => "command_complete",
            PgMessage::ReadyForQuery => "ready_for_query",
            PgMessage::ErrorResponse => "error_response",
            PgMessage::Other(_) => "other",
        }
    }

    /// The backend message a frontend `Sync` is answered by; the frontend `Query` is answered
    /// the same way.
    pub fn is_commit_boundary(self) -> bool {
        matches!(self, PgMessage::Sync | PgMessage::Query)
    }
}

/// Whether a frame is the untyped startup packet or a typed message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameKind {
    /// The first message of a connection: a length with no type byte.
    Startup,
    /// A typed message.
    Typed(PgMessage),
}

impl FrameKind {
    /// The named message, or [`PgMessage::Startup`].
    pub fn message(self) -> PgMessage {
        match self {
            FrameKind::Startup => PgMessage::Startup,
            FrameKind::Typed(m) => m,
        }
    }
}

/// One complete wire message: its kind, its length field (payload plus the four length bytes;
/// for the startup packet, the whole packet length) and where it sits in the direction's byte
/// stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame {
    /// Whether it was the untyped startup packet or a typed message.
    pub kind: FrameKind,
    /// The protocol length field, in bytes.
    pub len: u32,
    /// The cumulative byte offset where the frame's first byte arrived.
    pub start: u64,
    /// The cumulative byte offset just after the frame's last byte.
    pub end: u64,
}

/// A malformed frame header: the stream cannot be read further.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WireError {
    /// A length field smaller than its own four bytes.
    LengthTooSmall(u32),
    /// A length field larger than [`MAX_FRAME_LEN`].
    LengthTooLarge(u32),
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WireError::LengthTooSmall(len) => write!(f, "frame length {len} is smaller than 4"),
            WireError::LengthTooLarge(len) => write!(f, "frame length {len} exceeds the maximum"),
        }
    }
}

impl std::error::Error for WireError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    /// The startup packet's length, or a typed message's length.
    Length,
    /// A typed message's one-byte type.
    Type,
    /// A frame's payload.
    Payload,
}

/// An incremental frame reader. Feed it bytes in order; it yields the frames that completed.
#[derive(Clone, Debug)]
pub struct FrameScanner {
    startup_done: bool,
    stage: Stage,
    len_bytes: [u8; 4],
    len_count: usize,
    cur_len: u32,
    cur_type: Option<u8>,
    payload_left: u32,
    frame_start: Option<u64>,
    bytes_seen: u64,
    frames_seen: u64,
    failed: Option<WireError>,
}

impl Default for FrameScanner {
    fn default() -> Self {
        FrameScanner::new()
    }
}

impl FrameScanner {
    /// A scanner at the start of a fresh connection (expecting the untyped startup packet).
    pub fn new() -> FrameScanner {
        FrameScanner {
            startup_done: false,
            stage: Stage::Length,
            len_bytes: [0; 4],
            len_count: 0,
            cur_len: 0,
            cur_type: None,
            payload_left: 0,
            frame_start: None,
            bytes_seen: 0,
            frames_seen: 0,
            failed: None,
        }
    }

    /// Total bytes consumed on this direction so far, headers and payloads included.
    pub fn bytes_seen(&self) -> u64 {
        self.bytes_seen
    }

    /// Frames completed so far.
    pub fn frames_seen(&self) -> u64 {
        self.frames_seen
    }

    /// The first malformed header seen, if any.
    pub fn error(&self) -> Option<WireError> {
        self.failed
    }

    /// Consumes `bytes` and returns every frame that completed, in order.
    ///
    /// Once a malformed header is seen the scanner is poisoned: it yields no more frames and
    /// [`FrameScanner::error`] returns the cause.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Frame> {
        let mut out = Vec::new();
        for &byte in bytes {
            self.bytes_seen += 1;
            if self.failed.is_some() {
                continue;
            }
            if self.frame_start.is_none() {
                self.frame_start = Some(self.bytes_seen - 1);
            }
            match self.stage {
                Stage::Length => {
                    self.len_bytes[self.len_count] = byte;
                    self.len_count += 1;
                    if self.len_count == 4 {
                        let len = u32::from_be_bytes(self.len_bytes);
                        self.len_count = 0;
                        if len < 4 {
                            self.failed = Some(WireError::LengthTooSmall(len));
                            continue;
                        }
                        if len > MAX_FRAME_LEN {
                            self.failed = Some(WireError::LengthTooLarge(len));
                            continue;
                        }
                        self.cur_len = len;
                        self.payload_left = len - 4;
                        if self.payload_left == 0 {
                            out.push(self.finish_frame());
                        } else {
                            self.stage = Stage::Payload;
                        }
                    }
                }
                Stage::Type => {
                    self.cur_type = Some(byte);
                    self.stage = Stage::Length;
                }
                Stage::Payload => {
                    self.payload_left -= 1;
                    if self.payload_left == 0 {
                        out.push(self.finish_frame());
                    }
                }
            }
        }
        out
    }

    fn finish_frame(&mut self) -> Frame {
        self.frames_seen += 1;
        let start = self.frame_start.take().unwrap_or(0);
        let end = self.bytes_seen;
        if !self.startup_done {
            self.startup_done = true;
            self.stage = Stage::Type;
            return Frame {
                kind: FrameKind::Startup,
                len: self.cur_len,
                start,
                end,
            };
        }
        let byte = self.cur_type.take().unwrap_or(b'?');
        self.stage = Stage::Type;
        Frame {
            kind: FrameKind::Typed(PgMessage::from_code(byte)),
            len: self.cur_len,
            start,
            end,
        }
    }
}

impl PgMessage {
    /// Names the message with this type byte; unknown bytes become [`PgMessage::Other`].
    pub fn from_code(byte: u8) -> PgMessage {
        match byte {
            b'P' => PgMessage::Parse,
            b'B' => PgMessage::Bind,
            b'D' => PgMessage::Describe,
            b'E' => PgMessage::Execute,
            b'S' => PgMessage::Sync,
            b'Q' => PgMessage::Query,
            b'C' => PgMessage::CommandComplete,
            b'X' => PgMessage::Terminate,
            b'Z' => PgMessage::ReadyForQuery,
            other => PgMessage::Other(other),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// A typed frame: type byte, Int32 length, payload.
    fn typed(byte: u8, payload: &[u8]) -> Vec<u8> {
        let mut v = vec![byte];
        let len = u32::try_from(payload.len() + 4).unwrap();
        v.extend_from_slice(&len.to_be_bytes());
        v.extend_from_slice(payload);
        v
    }

    fn startup(payload: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        let len = u32::try_from(payload.len() + 4).unwrap();
        v.extend_from_slice(&len.to_be_bytes());
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn a_captured_exchange_reads_as_its_frames() {
        // Startup(version 3.0) then Parse(""), Bind, Execute, Sync — the shape tokio-postgres
        // sends for one prepared statement, with a 4-byte body on each typed message.
        let mut stream = startup(&[0, 3, 0, 0]);
        stream.extend(typed(b'P', &[0, 0, 0, 0]));
        stream.extend(typed(b'B', &[0, 0, 0, 0]));
        stream.extend(typed(b'E', &[0, 0, 0, 0]));
        stream.extend(typed(b'S', &[0, 0, 0, 0]));
        let mut scanner = FrameScanner::new();
        let frames = scanner.feed(&stream);
        assert_eq!(
            frames,
            vec![
                Frame {
                    kind: FrameKind::Startup,
                    len: 8,
                    start: 0,
                    end: 8
                },
                Frame {
                    kind: FrameKind::Typed(PgMessage::Parse),
                    len: 8,
                    start: 8,
                    end: 17
                },
                Frame {
                    kind: FrameKind::Typed(PgMessage::Bind),
                    len: 8,
                    start: 17,
                    end: 26
                },
                Frame {
                    kind: FrameKind::Typed(PgMessage::Execute),
                    len: 8,
                    start: 26,
                    end: 35
                },
                Frame {
                    kind: FrameKind::Typed(PgMessage::Sync),
                    len: 8,
                    start: 35,
                    end: 44
                },
            ]
        );
        assert_eq!(scanner.bytes_seen(), stream.len() as u64);
        assert_eq!(scanner.frames_seen(), 5);
    }

    #[test]
    fn the_backend_reply_reads_as_command_complete_then_ready() {
        let mut stream = typed(b'C', &[0, 0, 0, 0]);
        stream.extend(typed(b'Z', b"I"));
        let mut scanner = FrameScanner::new();
        // Pretend the startup already happened.
        let _ = scanner.feed(&startup(&[0, 3, 0, 0]));
        let frames = scanner.feed(&stream);
        assert_eq!(
            frames,
            vec![
                Frame {
                    kind: FrameKind::Typed(PgMessage::CommandComplete),
                    len: 8,
                    start: 8,
                    end: 17
                },
                Frame {
                    kind: FrameKind::Typed(PgMessage::ReadyForQuery),
                    len: 5,
                    start: 17,
                    end: 23
                },
            ]
        );
    }

    #[test]
    fn frames_split_across_reads_reassemble() {
        let stream = typed(b'Z', b"I");
        let mut scanner = FrameScanner::new();
        let _ = scanner.feed(&startup(&[0, 3, 0, 0]));
        let mut frames = Vec::new();
        for byte in stream {
            frames.extend(scanner.feed(&[byte]));
        }
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].kind, FrameKind::Typed(PgMessage::ReadyForQuery));
        assert_eq!(frames[0].len, 5);
        assert_eq!(frames[0].start, 8);
        assert_eq!(frames[0].end, 14);
    }

    #[test]
    fn a_short_length_is_an_error_and_poisons_the_scanner() {
        let mut scanner = FrameScanner::new();
        // A startup length of 3 is impossible (it must include itself).
        let frames = scanner.feed(&[0, 0, 0, 3, 9, 9, 9]);
        assert!(frames.is_empty());
        assert_eq!(scanner.error(), Some(WireError::LengthTooSmall(3)));
        assert!(scanner.feed(&[0, 0, 0, 8]).is_empty());
    }

    #[test]
    fn an_overlong_length_is_refused() {
        let mut scanner = FrameScanner::new();
        let _ = scanner.feed(&[0xFF, 0xFF, 0xFF, 0xFF]);
        assert_eq!(scanner.error(), Some(WireError::LengthTooLarge(u32::MAX)));
    }

    #[test]
    fn unknown_type_bytes_are_kept() {
        let mut scanner = FrameScanner::new();
        let _ = scanner.feed(&startup(&[0, 3, 0, 0]));
        let frames = scanner.feed(&typed(b'w', &[0, 0, 0, 0]));
        assert_eq!(frames[0].kind, FrameKind::Typed(PgMessage::Other(b'w')));
    }

    #[test]
    fn the_commit_boundaries_are_named() {
        assert!(PgMessage::Sync.is_commit_boundary());
        assert!(PgMessage::Query.is_commit_boundary());
        assert!(!PgMessage::Execute.is_commit_boundary());
        assert_eq!(PgMessage::ReadyForQuery.code(), Some(b'Z'));
    }
}
