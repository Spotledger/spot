//! Numeric range leases: the hub grants each branch a disjoint block of a naming series.
//!
//! A per-branch series must allocate numbers offline and never collide with another branch
//! (ADR-062 section 7, WP-EDG-03). The mechanism is a **range lease**: the hub's allocator hands
//! a node the consecutive numbers `from..=to` of one [`SeriesKey`], tagged with a [`LeaseId`].
//! The node allocates inside its lease with no coordination (see [`crate::BranchSeries`]); the
//! hub never grants the same number twice, so two branches never produce the same number.
//!
//! [`RangeAllocator`] is the hub-side trait; [`MemoryRangeAllocator`] is the in-memory
//! implementation. The database-backed allocator for the hub's control plane is a later task
//! behind the same trait.

use std::collections::HashMap;

use spot_storage_contract::SeriesKey;
use spot_sync_protocol::NodeId;
use time::OffsetDateTime;

use crate::error::PartitionError;

/// The identity of one granted range, unique within an allocator.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LeaseId(u64);

impl LeaseId {
    /// A lease id from its number.
    #[must_use]
    pub const fn new(number: u64) -> Self {
        Self(number)
    }

    /// The number.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// A hub-granted block of consecutive numbers of one series, for one node.
///
/// The interval `from..=to` is inclusive and non-empty. `from` is at least 1, so numbers never
/// start at zero, and `to` is below the largest `u64`, so the node can always record the
/// high-water mark just past the last number.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RangeLease {
    /// The naming series the numbers belong to.
    pub series: SeriesKey,
    /// The first number in the range.
    pub from: u64,
    /// The last number in the range, at or above `from`.
    pub to: u64,
    /// The node the range is leased to.
    pub node: NodeId,
    /// The lease's identity, unique within the granting allocator.
    pub lease_id: LeaseId,
}

impl RangeLease {
    /// A range lease, validated.
    ///
    /// # Errors
    ///
    /// [`PartitionError::InvalidRange`] when `from` is zero, `to` is below `from`, or `to` is
    /// the largest `u64` (which leaves no room for a high-water mark past the range).
    pub fn new(
        series: SeriesKey,
        from: u64,
        to: u64,
        node: NodeId,
        lease_id: LeaseId,
    ) -> Result<Self, PartitionError> {
        if from == 0 {
            return Err(PartitionError::InvalidRange {
                reason: "a range starts at 1 or above".to_owned(),
            });
        }
        if to < from {
            return Err(PartitionError::InvalidRange {
                reason: "a range's last number is below its first".to_owned(),
            });
        }
        if to == u64::MAX {
            return Err(PartitionError::InvalidRange {
                reason: "a range cannot reach the largest number".to_owned(),
            });
        }
        Ok(Self {
            series,
            from,
            to,
            node,
            lease_id,
        })
    }

    /// How many numbers the range holds.
    #[must_use]
    pub fn size(&self) -> u64 {
        self.to - self.from + 1
    }

    /// Whether `number` is inside the range.
    #[must_use]
    pub fn contains(&self, number: u64) -> bool {
        self.from <= number && number <= self.to
    }
}

/// A grant of a range, with the instant it was made, for the audit trail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RangeGrant {
    /// The granted range.
    pub lease: RangeLease,
    /// The instant the hub granted it.
    pub granted_at: OffsetDateTime,
}

/// The hub-side allocator: it grants disjoint ranges of a series to nodes.
pub trait RangeAllocator: Send {
    /// Grants `node` a block of `size` consecutive numbers of `series`.
    ///
    /// The block never overlaps one already granted, so numbers allocated under two leases
    /// never collide.
    ///
    /// # Errors
    ///
    /// [`PartitionError::InvalidArgument`] when `size` is zero, and
    /// [`PartitionError::RangeExhausted`] when the series has no numbers left.
    fn lease(
        &mut self,
        series: &SeriesKey,
        node: &NodeId,
        size: u64,
        at: OffsetDateTime,
    ) -> Result<RangeLease, PartitionError>;

    /// Every range granted so far, oldest first.
    fn grants(&self) -> Vec<RangeGrant>;
}

/// An in-memory [`RangeAllocator`]: one monotonic cursor per series, so granted ranges are
/// disjoint by construction.
#[derive(Debug, Default)]
pub struct MemoryRangeAllocator {
    next: HashMap<String, u128>,
    next_id: u64,
    grants: Vec<RangeGrant>,
}

impl MemoryRangeAllocator {
    /// An allocator that starts every series at 1.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl RangeAllocator for MemoryRangeAllocator {
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
        let cursor = self
            .next
            .entry(series.as_str().to_owned())
            .or_insert(1_u128);
        if *cursor > u128::from(u64::MAX) {
            return Err(PartitionError::RangeExhausted {
                series: series.as_str().to_owned(),
                node: node.to_string(),
            });
        }
        let from = *cursor as u64;
        let to = from
            .checked_add(size - 1)
            .ok_or_else(|| PartitionError::InvalidRange {
                reason: "the range overflows the number space".to_owned(),
            })?;
        if to == u64::MAX {
            return Err(PartitionError::InvalidRange {
                reason: "the range would reach the largest number".to_owned(),
            });
        }
        let lease_id = LeaseId::new(self.next_id);
        self.next_id =
            self.next_id
                .checked_add(1)
                .ok_or_else(|| PartitionError::InvalidArgument {
                    reason: "the allocator has issued every lease id".to_owned(),
                })?;
        *cursor = u128::from(to) + 1;
        let lease = RangeLease {
            series: series.clone(),
            from,
            to,
            node: node.clone(),
            lease_id,
        };
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
