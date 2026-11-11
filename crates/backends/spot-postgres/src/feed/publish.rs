//! The compact publisher: publishes pending committed entries, then reads (ADR-053).
//!
//! # Why this is safe
//!
//! One publisher per log runs at a time: it holds a transaction-level advisory lock that only
//! publishers take, so business transactions are never delayed. Under the lock a single statement
//! (one snapshot, `READ COMMITTED`) selects the *visible* captured rows that have no position
//! yet, orders them by capture time, then transaction id, then ordinal, and appends them to the
//! positions table with the next sequence numbers.
//!
//! - **Safe bookmarks.** A row that is not visible belongs to a transaction that has not
//!   committed. It gets a position when a later publication sees it, which is a higher one: a
//!   reader that stored position `p` has never skipped it, because positions are assigned only
//!   to rows that are visible at that moment, in increasing order.
//! - **Transaction order.** The ordinals of one transaction have non-decreasing capture times
//!   (one session's clock reads), and ties break by ordinal.
//! - **Causal order.** An entry captured after transaction T committed has a later capture time
//!   than every entry of T, and T's entries are visible whenever it is, so they get lower
//!   positions. This relies on the server clock not stepping backwards (use a slewing time
//!   service); entries of concurrent transactions have no defined relative order.
//! - **Bounded work.** The pending test never scans the whole log. The state row keeps a
//!   `horizon`, the oldest running transaction id when the previous publication took its
//!   snapshot; every row below it was finished, so already published or rolled back. Only rows at
//!   or above the horizon can be pending. A long-running transaction holds the horizon back (and
//!   so makes the candidate set larger), never the correctness.
//!
//! Reading validates the bookmark against the log's generation, floor and head in one
//! repeatable-read snapshot with the page, so a bookmark is never judged against a head that
//! differs from the page returned.

use spot_storage_contract::{ErrorKind, FeedPosition, StorageError, StorageResult};
use tokio_postgres::{IsolationLevel, Row};

use super::{db, sql};
use crate::PgBackend;

/// Held for the publication's transaction: the bytes of `SPOTPUBO` and `SPOTPUBC`.
const OUTBOX_LOCK: i64 = 0x5350_4f54_5055_424f;
const CHANGES_LOCK: i64 = 0x5350_4f54_5055_4243;

/// Which log.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Log {
    Outbox,
    Changes,
}

impl Log {
    pub(super) fn name(self) -> &'static str {
        match self {
            Log::Outbox => "outbox",
            Log::Changes => "changes",
        }
    }

    pub(super) fn lock_key(self) -> i64 {
        match self {
            Log::Outbox => OUTBOX_LOCK,
            Log::Changes => CHANGES_LOCK,
        }
    }

    fn publish_sql(self) -> &'static str {
        match self {
            Log::Outbox => sql::PUBLISH_OUTBOX,
            Log::Changes => sql::PUBLISH_CHANGES,
        }
    }

    fn read_sql(self) -> &'static str {
        match self {
            Log::Outbox => sql::READ_OUTBOX,
            Log::Changes => sql::READ_CHANGES,
        }
    }
}

/// Publishes what is pending in `log`, then returns the generation and up to `limit` rows after
/// `after`. Fails with `ResyncRequired` for a bookmark that cannot be continued from.
pub(super) async fn publish_and_read(
    backend: &PgBackend,
    log: Log,
    after: FeedPosition,
    limit: u32,
) -> StorageResult<(i64, Vec<Row>)> {
    let mut conn = backend.checkout().await?;
    {
        let tx = conn
            .build_transaction()
            .isolation_level(IsolationLevel::ReadCommitted)
            .start()
            .await
            .map_err(db)?;
        tx.execute("SELECT pg_advisory_xact_lock($1)", &[&log.lock_key()])
            .await
            .map_err(db)?;
        tx.execute(log.publish_sql(), &[]).await.map_err(db)?;
        tx.commit().await.map_err(db)?;
    }
    let tx = conn
        .build_transaction()
        .isolation_level(IsolationLevel::RepeatableRead)
        .read_only(true)
        .start()
        .await
        .map_err(db)?;
    let state = tx.query_one(sql::STATE, &[&log.name()]).await.map_err(db)?;
    let generation: i64 = state.get(0);
    let head: i64 = state.get(1);
    let floor: i64 = state.get(2);
    let start = check_position(after, generation, head, floor)?;
    let rows = tx
        .query(log.read_sql(), &[&generation, &start, &i64::from(limit)])
        .await
        .map_err(db)?;
    tx.commit().await.map_err(db)?;
    Ok((generation, rows))
}

/// The sequence number to read after, or `ResyncRequired`.
fn check_position(
    after: FeedPosition,
    generation: i64,
    head: i64,
    floor: i64,
) -> StorageResult<i64> {
    if after == FeedPosition::START {
        return Ok(0);
    }
    let resync = |why: String| StorageError::new(ErrorKind::ResyncRequired, why);
    if i64::try_from(after.generation()) != Ok(generation) {
        return Err(resync(format!(
            "the bookmark is from stream generation {} but the feed is at generation {generation}; \
             read again from the start",
            after.generation()
        )));
    }
    let seq = i64::try_from(after.seq())
        .map_err(|_| resync("the bookmark's sequence is beyond the end of the feed".to_owned()))?;
    if seq > head {
        return Err(resync(format!(
            "the bookmark ({seq}) is beyond the end of the feed ({head})"
        )));
    }
    if seq < floor {
        return Err(resync(format!(
            "the bookmark ({seq}) is older than the retained feed, which starts after {floor}"
        )));
    }
    Ok(seq)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(r: StorageResult<i64>) -> Option<ErrorKind> {
        r.err().map(|e| e.kind)
    }

    #[test]
    fn start_is_always_valid() {
        assert_eq!(check_position(FeedPosition::START, 3, 10, 7), Ok(0));
    }

    #[test]
    fn bookmarks_are_checked_against_generation_head_and_floor() {
        let p = |g, s| FeedPosition::new(g, s);
        assert_eq!(check_position(p(3, 7), 3, 10, 7), Ok(7));
        assert_eq!(check_position(p(3, 10), 3, 10, 7), Ok(10));
        assert_eq!(
            kind(check_position(p(3, 11), 3, 10, 7)),
            Some(ErrorKind::ResyncRequired)
        );
        assert_eq!(
            kind(check_position(p(3, 6), 3, 10, 7)),
            Some(ErrorKind::ResyncRequired)
        );
        assert_eq!(
            kind(check_position(p(2, 8), 3, 10, 7)),
            Some(ErrorKind::ResyncRequired)
        );
        assert_eq!(
            kind(check_position(p(u64::MAX, 8), 3, 10, 7)),
            Some(ErrorKind::ResyncRequired)
        );
    }
}
