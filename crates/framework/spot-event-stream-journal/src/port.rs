//! The port the event stream asks for history through, and the seam check.

use std::future::Future;
use std::pin::Pin;

use spot_events_contract::envelope::{EventEnvelope, Position};

use crate::error::HistoryError;

/// The default page size (scanned entries).
pub const DEFAULT_PAGE: usize = 500;
/// The largest page size; a larger request is clamped.
pub const MAX_PAGE: usize = 5_000;

/// The consumer's selector, the same predicate the live path applies (the selector type of the
/// stream crate is wrapped by the caller, so this crate does not depend on it).
pub type Selector = dyn Fn(&EventEnvelope) -> bool + Send + Sync;

/// The boxed future of every history call (no async runtime dependency).
pub type HistoryFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, HistoryError>> + Send + 'a>>;

/// One page of history.
#[derive(Clone, Debug, PartialEq)]
pub struct HistoryPage {
    /// The selected envelopes, in position order.
    pub envelopes: Vec<EventEnvelope>,
    /// The last position **scanned** (selected or not): the consumer continues after it. Equal to
    /// the request's `after` when nothing was scanned.
    pub next_after: Position,
    /// History is exhausted up to the requested bound: the live outbox serves from the position
    /// after `next_after`. `false` means more history follows.
    pub reached_outbox: bool,
}

/// Read access to the journal for a consumer whose checkpoint is older than the outbox horizon.
///
/// Positions are `(generation, seq)`, ordered lexicographically; `after = (0, 0)` is the start.
pub trait JournalHistory: Send + Sync {
    /// Up to `max` entries scanned after `after` (clamped to [`MAX_PAGE`]; `0` means
    /// [`DEFAULT_PAGE`]), never past `through` (inclusive; the seam position `H`, the last position
    /// before the outbox horizon), filtered by `select`.
    ///
    /// # Errors
    /// [`HistoryError::ResyncRequired`] for a position in an abandoned generation,
    /// [`HistoryError::HistoryUnavailable`] when a needed object is missing or does not verify,
    /// [`HistoryError::ForeignKey`], [`HistoryError::RequestInvalid`].
    fn read_after<'a>(
        &'a self,
        after: Position,
        max: usize,
        through: Option<Position>,
        select: &'a Selector,
    ) -> HistoryFuture<'a, HistoryPage>;

    /// The first position the journal can serve, or `None` when it holds nothing.
    ///
    /// # Errors
    /// As [`JournalHistory::read_after`].
    fn first_available(&self) -> HistoryFuture<'_, Option<Position>>;
}

pub(crate) fn order(p: Position) -> (u64, u64) {
    (p.generation, p.seq)
}

/// The seam check: history was served through `history_end` and the outbox will serve from
/// `outbox_first_retained` (its earliest retained position). Ok when the outbox starts at or before
/// the position right after `history_end` (the next sequence number of the same generation, or
/// position 1 of a later generation); otherwise someone trimmed the outbox during the call and
/// continuing would jump silently, so it fails closed with
/// [`HistoryError::ResyncRequired`].
///
/// # Errors
/// [`HistoryError::ResyncRequired`] naming the first position that would be skipped.
pub fn check_seam(
    history_end: Position,
    outbox_first_retained: Position,
) -> Result<(), HistoryError> {
    let ok = match outbox_first_retained
        .generation
        .cmp(&history_end.generation)
    {
        std::cmp::Ordering::Less => true,
        std::cmp::Ordering::Equal => outbox_first_retained.seq <= history_end.seq.saturating_add(1),
        std::cmp::Ordering::Greater => outbox_first_retained.seq <= 1,
    };
    if ok {
        Ok(())
    } else {
        Err(HistoryError::ResyncRequired {
            generation: history_end.generation,
            seq: history_end.seq.saturating_add(1),
        })
    }
}
