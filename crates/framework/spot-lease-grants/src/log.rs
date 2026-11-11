//! The grant log: every range is written here before the node can use it.
//!
//! The traits in this crate are **synchronous**, like [`RangeAllocator`](spot_partition::RangeAllocator).
//! Whether a durable (PostgreSQL) log is sync or async is the decision of WP-CON-01.T4; this
//! crate does not pre-empt it and adds no async.

use std::collections::BTreeMap;

use spot_partition::RangeLease;
use spot_storage_contract::SeriesKey;
use spot_sync_protocol::NodeId;
use thiserror::Error;
use time::OffsetDateTime;

use crate::identity::RequestId;

/// One granted range, as the log keeps it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrantRecord {
    /// The range, with its lease id and node.
    pub lease: RangeLease,
    /// The node the range was granted to (the authenticated node).
    pub node: NodeId,
    /// How many numbers the node asked for.
    pub requested_size: u64,
    /// The instant of the grant.
    pub granted_at: OffsetDateTime,
    /// The envelope id the request carried, when it carried one.
    ///
    /// It is part of the record so that a service restarted between the grant and the node's
    /// retry can answer the retry with this range: the log is the only memory that survives.
    pub request_id: Option<RequestId>,
}

/// The log could not take a record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum GrantLogError {
    /// The store is not available. Nothing was written.
    #[error("the grant log is unavailable")]
    Unavailable,
    /// The record would overlap a range already in the log, or reuse a lease id. A log
    /// refuses it however the allocator got there.
    #[error("the record overlaps a range already in the grant log")]
    Overlap,
}

/// Durable, append-only record of granted ranges.
pub trait GrantLog {
    /// Appends `record`, durably, or fails with nothing written.
    ///
    /// # Errors
    ///
    /// [`GrantLogError::Overlap`] when the record's range is not strictly above the series'
    /// [`cursor`](Self::cursor) or its lease id is already used, and
    /// [`GrantLogError::Unavailable`] when the store fails.
    fn append(&mut self, record: GrantRecord) -> Result<(), GrantLogError>;

    /// The highest number ever granted in `series` (the last `to`), or `None` when nothing was.
    fn cursor(&self, series: &SeriesKey) -> Option<u64>;

    /// Every record, oldest first, for rebuilding the allocator after a restart.
    fn all(&self) -> Vec<GrantRecord>;
}

/// An in-memory [`GrantLog`], with a failure switch for tests.
#[derive(Clone, Debug, Default)]
pub struct MemoryGrantLog {
    records: Vec<GrantRecord>,
    cursors: BTreeMap<String, u64>,
    /// When set, every append fails with [`GrantLogError::Unavailable`] and writes nothing.
    pub fail: bool,
}

impl MemoryGrantLog {
    /// An empty log.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl GrantLog for MemoryGrantLog {
    fn append(&mut self, record: GrantRecord) -> Result<(), GrantLogError> {
        if self.fail {
            return Err(GrantLogError::Unavailable);
        }
        let key = record.lease.series.as_str();
        if self
            .cursors
            .get(key)
            .is_some_and(|c| record.lease.from <= *c)
        {
            return Err(GrantLogError::Overlap);
        }
        if self
            .records
            .iter()
            .any(|r| r.lease.lease_id == record.lease.lease_id)
        {
            return Err(GrantLogError::Overlap);
        }
        self.cursors.insert(key.to_owned(), record.lease.to);
        self.records.push(record);
        Ok(())
    }

    fn cursor(&self, series: &SeriesKey) -> Option<u64> {
        self.cursors.get(series.as_str()).copied()
    }

    fn all(&self) -> Vec<GrantRecord> {
        self.records.clone()
    }
}

/// The contract any [`GrantLog`] must pass (a later PostgreSQL log runs it too).
///
/// `make` returns a fresh, empty log each call. The checks panic with a message on failure.
pub mod contract {
    use spot_partition::{LeaseId, RangeLease};
    use spot_storage_contract::SeriesKey;
    use spot_sync_protocol::NodeId;
    use time::OffsetDateTime;

    use super::{GrantLog, GrantLogError, GrantRecord};
    use crate::identity::RequestId;

    fn record(series: &SeriesKey, from: u64, to: u64, id: u64) -> GrantRecord {
        let node = NodeId::new("edge-khi#branch_07").unwrap_or_else(|e| panic!("{e}"));
        GrantRecord {
            lease: RangeLease::new(series.clone(), from, to, node.clone(), LeaseId::new(id))
                .unwrap_or_else(|e| panic!("{e}")),
            node,
            requested_size: to - from + 1,
            granted_at: OffsetDateTime::UNIX_EPOCH,
            request_id: None,
        }
    }

    /// Runs the contract against fresh logs from `make`.
    ///
    /// # Panics
    ///
    /// When the log breaks a rule.
    pub fn run<L: GrantLog>(make: impl Fn() -> L) {
        let a = SeriesKey::new("INV-").unwrap_or_else(|e| panic!("{e}"));
        let b = SeriesKey::new("PO-").unwrap_or_else(|e| panic!("{e}"));

        // Empty.
        let log = make();
        assert_eq!(log.cursor(&a), None);
        assert!(log.all().is_empty());

        // The cursor is the last `to`, per series, and `all` is in order.
        let mut log = make();
        log.append(record(&a, 1, 100, 0))
            .unwrap_or_else(|e| panic!("{e}"));
        log.append(record(&b, 1, 10, 1))
            .unwrap_or_else(|e| panic!("{e}"));
        log.append(record(&a, 101, 150, 2))
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(log.cursor(&a), Some(150));
        assert_eq!(log.cursor(&b), Some(10));
        let ids: Vec<u64> = log.all().iter().map(|r| r.lease.lease_id.get()).collect();
        assert_eq!(ids, [0, 1, 2]);

        // Overlap, repeat and going backwards are refused and leave the log as it was.
        for (from, to, id) in [(150, 200, 3), (1, 5, 4), (101, 150, 5), (120, 300, 6)] {
            assert_eq!(
                log.append(record(&a, from, to, id)),
                Err(GrantLogError::Overlap),
                "{from}..={to} must be refused"
            );
        }
        // A reused lease id is refused even for a free range.
        assert_eq!(
            log.append(record(&a, 151, 160, 2)),
            Err(GrantLogError::Overlap)
        );
        assert_eq!(log.cursor(&a), Some(150));
        assert_eq!(log.all().len(), 3);

        // A gap is allowed (a lost range leaves one), and the cursor moves to the new `to`.
        log.append(record(&a, 200, 210, 7))
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(log.cursor(&a), Some(210));

        // The request id comes back with its record: a restart answers a retry from the log.
        let id = RequestId::new("contract-request").unwrap_or_else(|| panic!("bad request id"));
        let mut with_id = record(&b, 11, 20, 8);
        with_id.request_id = Some(id.clone());
        log.append(with_id).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            log.all().last().and_then(|r| r.request_id.clone()),
            Some(id)
        );
    }
}
