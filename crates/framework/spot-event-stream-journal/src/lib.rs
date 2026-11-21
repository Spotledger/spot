//! The journal history reader for the event stream (WP-JRN-09.T2, ADR-061 s5 and s6).
//!
//! A consumer whose checkpoint is older than the outbox retention would get
//! `resync_required`. With a journal the history is still there: [`JournalReader`] answers from
//! the sealed segments (verified against the signed manifest before anything is served), then the
//! unsealed tail chunks (ADR-061 Appendix A.6), and [`check_seam`] makes the hand-over to the live
//! outbox fail closed instead of jumping.
//!
//! # The seam contract
//!
//! The engine asks history only when the checkpoint is below the outbox horizon. History is read
//! `through` the position `H` just before the horizon; the outbox serves from `H + 1`; if the
//! outbox's earliest retained position is beyond `H + 1` (someone trimmed during the call),
//! [`check_seam`] returns `resync_required`. The concatenation of history pages and outbox pages
//! is then the full stream, with no gap and no duplicate (a property test shows it).
//!
//! # Differences from the brief's sketch (see the report)
//!
//! The port is asynchronous (boxed futures) because the journal store is; `read_after` takes the
//! seam bound `through`; the page carries `next_after`, the last position scanned, so a selector
//! that matches nothing still advances the checkpoint.
//!
//! # Not here
//!
//! Routes and the adapter into `spot-event-stream` (T3), the access surface (T4) and writing the
//! journal.

#![forbid(unsafe_code)]

mod error;
mod port;
mod reader;

pub use error::HistoryError;
pub use port::{
    DEFAULT_PAGE, HistoryFuture, HistoryPage, JournalHistory, MAX_PAGE, Selector, check_seam,
};
pub use reader::JournalReader;
