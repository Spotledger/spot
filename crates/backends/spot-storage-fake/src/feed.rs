//! The published outbox and change feed.
//!
//! The fake publishes a transaction's entries at commit, under the state mutex. That is a valid
//! publisher schedule: positions strictly increase, a transaction's events keep their enqueue
//! order, and anything written after a commit is published after that commit's entries.

use std::collections::HashSet;

use spot_storage_contract::{
    Change, ErrorKind, FeedPosition, OutboxRecord, RecordId, StorageError, StorageResult,
};

/// The stream generation of every position. The fake never restores, so it never changes.
pub(crate) const GENERATION: u64 = 1;

/// Both published logs. Entry `i` (from 0) has position `(GENERATION, i + 1)`.
#[derive(Debug, Default)]
pub(crate) struct Feeds {
    /// Published outbox events.
    pub outbox: Vec<OutboxRecord>,
    /// Published document changes.
    pub changes: Vec<Change>,
    /// Ids of every published event, for duplicate checks.
    pub event_ids: HashSet<RecordId>,
}

impl Feeds {
    /// The position the next outbox event gets.
    pub fn next_outbox_position(&self) -> FeedPosition {
        position(self.outbox.len())
    }

    /// The position the next change gets.
    pub fn next_change_position(&self) -> FeedPosition {
        position(self.changes.len())
    }
}

fn position(published: usize) -> FeedPosition {
    FeedPosition::new(
        GENERATION,
        u64::try_from(published)
            .unwrap_or(u64::MAX)
            .saturating_add(1),
    )
}

/// Up to `limit` entries of `log` after `after`.
pub(crate) fn page<T: Clone>(
    log: &[T],
    after: FeedPosition,
    limit: u32,
    what: &str,
) -> StorageResult<Vec<T>> {
    let skip = if after == FeedPosition::START {
        0
    } else {
        let len = u64::try_from(log.len()).unwrap_or(u64::MAX);
        if after.generation() != GENERATION || after.seq() > len {
            return Err(StorageError::new(
                ErrorKind::ResyncRequired,
                format!(
                    "{what} position {after:?} is not in this feed (generation {GENERATION}, \
                     {len} entries); resynchronise from a snapshot"
                ),
            ));
        }
        usize::try_from(after.seq()).unwrap_or(usize::MAX)
    };
    let limit = usize::try_from(limit).unwrap_or(usize::MAX);
    Ok(log.iter().skip(skip).take(limit).cloned().collect())
}
