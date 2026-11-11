//! An allocator that starts where the log says the series stopped.

use std::collections::BTreeMap;

use spot_partition::{LeaseId, PartitionError, RangeAllocator, RangeGrant, RangeLease};
use spot_storage_contract::SeriesKey;
use spot_sync_protocol::NodeId;
use time::OffsetDateTime;

use crate::log::GrantRecord;

/// A [`RangeAllocator`] whose starting cursors are given, so a restart resumes above every
/// number ever granted instead of at 1.
///
/// Like `MemoryRangeAllocator` the cursor of a series only moves forward, so ranges are
/// disjoint by construction; unlike it, [`RebuildableAllocator::from_records`] seeds the cursors
/// and the next lease id from the [`GrantLog`](crate::GrantLog).
#[derive(Debug, Default)]
pub struct RebuildableAllocator {
    /// Per series, the next number to grant.
    next: BTreeMap<String, u64>,
    next_id: u64,
    grants: Vec<RangeGrant>,
}

impl RebuildableAllocator {
    /// An allocator that starts every series at 1 (an empty log).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// An allocator that resumes above every range in `records`, and numbers its leases above
    /// every lease id in them.
    #[must_use]
    pub fn from_records(records: &[GrantRecord]) -> Self {
        let mut allocator = Self::new();
        for record in records {
            let key = record.lease.series.as_str().to_owned();
            let above = record.lease.to.saturating_add(1);
            let slot = allocator.next.entry(key).or_insert(1);
            *slot = (*slot).max(above);
            allocator.next_id = allocator
                .next_id
                .max(record.lease.lease_id.get().saturating_add(1));
        }
        allocator
    }
}

impl RangeAllocator for RebuildableAllocator {
    fn lease(
        &mut self,
        series: &SeriesKey,
        node: &NodeId,
        size: u64,
        at: OffsetDateTime,
    ) -> Result<RangeLease, PartitionError> {
        if size == 0 {
            return Err(PartitionError::InvalidArgument {
                reason: "a leased range holds at least one number".to_owned(),
            });
        }
        let exhausted = || PartitionError::RangeExhausted {
            series: series.as_str().to_owned(),
            node: node.to_string(),
        };
        let from = *self.next.get(series.as_str()).unwrap_or(&1);
        let to = from
            .checked_add(size - 1)
            .filter(|to| *to < u64::MAX)
            .ok_or_else(exhausted)?;
        let id = self.next_id;
        let next_id = id
            .checked_add(1)
            .ok_or_else(|| PartitionError::InvalidArgument {
                reason: "the allocator has issued every lease id".to_owned(),
            })?;
        let lease = RangeLease::new(series.clone(), from, to, node.clone(), LeaseId::new(id))?;
        // Only now does anything move: a failure above leaves the cursor where it was.
        self.next.insert(series.as_str().to_owned(), to + 1);
        self.next_id = next_id;
        self.grants.push(RangeGrant {
            lease: lease.clone(),
            granted_at: at,
        });
        Ok(lease)
    }

    fn grants(&self) -> Vec<RangeGrant> {
        self.grants.clone()
    }
}
