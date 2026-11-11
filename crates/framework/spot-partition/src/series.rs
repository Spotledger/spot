//! Per-branch naming series: allocate numbers from a leased range, offline, without collisions.
//!
//! [`BranchSeries`] is the local allocator a branch node runs. It holds one
//! [`RangeLease`](crate::RangeLease) granted by the hub, hands out the numbers in order, and
//! records the **high-water mark** in a [`HighWaterStore`] before each number is used, so a
//! crash or a reboot never reissues a number (WP-EDG-03, ADR-062 section 7).
//!
//! # Running low
//!
//! When the fraction of the range still unallocated falls to a configurable threshold,
//! [`BranchSeries::running_low`] reports it, so the hub can grant another range before the
//! current one is exhausted. The threshold is an integer fraction ([`LowWater`]); no float is
//! involved (ADR-12).
//!
//! # Offline exhaustion
//!
//! If the range runs out while the node is offline, [`BranchSeries::allocate`] fails with
//! `partition.range_exhausted`. The node must **block** document creation of that series until
//! it receives another range from the hub; it must never fall back to a local counter or reuse
//! a number, because that is exactly how two branches collide. An operator-issued emergency
//! range is a deliberate, audited act: it is installed through [`BranchSeries::install`] like
//! any other lease, so the same high-water discipline applies. (The emergency-range workflow
//! itself is a policy decision for a later ADR; see the crate's open questions.)
//!
//! # A lease never moves the mark down
//!
//! [`BranchSeries::install`] asks the store for a compare-and-set
//! ([`HighWaterStore::save_if_above`]), so a lease whose range is not above the stored one is
//! refused with `partition.lease_not_above_stored` instead of resetting the persisted mark and
//! reissuing numbers. Re-installing the lease that is already stored is a resume: it continues
//! at the stored mark, never at the lease's first number.
//!
//! # Extending the naming API
//!
//! The series identity is the naming layer's [`SeriesKey`](spot_storage_contract::SeriesKey),
//! and [`BranchSeries::allocate_name`] wraps an allocated number as a
//! [`RecordName`](spot_naming::RecordName). This is the lease-based counterpart of the
//! storage-transaction `Sequences` allocator: a branch uses it where the hub's transactional
//! allocator is unavailable. Rendering a full naming pattern (date parts, zero padding) stays
//! with [`spot_naming::Namer`]; wiring a leased number into the plan is a follow-up (see open
//! questions).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use spot_naming::RecordName;
use spot_storage_contract::SeriesKey;
use spot_sync_protocol::NodeId;

use crate::error::PartitionError;
use crate::lease::RangeLease;

/// The fraction of a range at or below which a branch series is "running low".
///
/// Stored as a numerator and a denominator (`low` when `remaining / total <= numerator /
/// denominator`), so the comparison is exact integer arithmetic. `numerator` is at most
/// `denominator` and `denominator` is at least 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LowWater {
    numerator: u64,
    denominator: u64,
}

impl LowWater {
    /// A threshold fraction, `numerator / denominator`.
    ///
    /// # Errors
    ///
    /// [`PartitionError::InvalidArgument`] when `denominator` is zero or `numerator` is above
    /// `denominator`.
    pub fn new(numerator: u64, denominator: u64) -> Result<Self, PartitionError> {
        if denominator == 0 {
            return Err(PartitionError::InvalidArgument {
                reason: "the running-low denominator is not zero".to_owned(),
            });
        }
        if numerator > denominator {
            return Err(PartitionError::InvalidArgument {
                reason: "the running-low numerator is at most its denominator".to_owned(),
            });
        }
        Ok(Self {
            numerator,
            denominator,
        })
    }

    /// A threshold of one half.
    #[must_use]
    pub fn half() -> Self {
        Self {
            numerator: 1,
            denominator: 2,
        }
    }

    /// Whether `remaining` of `total` numbers is at or below the threshold.
    #[must_use]
    pub fn is_low(&self, remaining: u64, total: u64) -> bool {
        u128::from(remaining) * u128::from(self.denominator)
            <= u128::from(total) * u128::from(self.numerator)
    }
}

/// What a [`HighWaterStore`] keeps for one branch series: its lease and its next number.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredSeries {
    /// The lease the branch holds.
    pub lease: RangeLease,
    /// The next number to hand out; one past the last number handed out.
    pub next: u64,
}

impl StoredSeries {
    /// What installing `lease` over this stored state is allowed to do.
    ///
    /// `Some` when `lease` is the lease already stored — a **resume**, which keeps this state's
    /// high-water mark — or lies strictly above it: its first number is greater than this
    /// state's lease's last number **and** greater than `next - 1`, the last number already
    /// handed out. `None` otherwise, because installing such a lease would reissue a number.
    ///
    /// A lease id is unique within an allocator, so the same id under a different range is not
    /// a resume either: it is refused, its id is not trustworthy.
    #[must_use]
    pub fn install_decision(&self, lease: &RangeLease) -> Option<InstallDecision> {
        if self.lease.lease_id == lease.lease_id {
            return if self.lease == *lease {
                Some(InstallDecision::Resume(self.next))
            } else {
                None
            };
        }
        let last_handed_out = self.next.saturating_sub(1);
        if lease.from > self.lease.to && lease.from > last_handed_out {
            Some(InstallDecision::Advance(lease.from))
        } else {
            None
        }
    }
}

/// How an install relates to the state a [`HighWaterStore`] already keeps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallDecision {
    /// The same lease is already stored: the series resumes at the stored high-water mark, so a
    /// restart continues where it stopped instead of at the lease's first number.
    Resume(u64),
    /// The lease is strictly above the stored one: it replaces the stored state and the series
    /// starts at the lease's first number.
    Advance(u64),
}

/// Persists a branch series' high-water mark, so a number is never reissued after a reboot.
///
/// [`BranchSeries::allocate`] writes the new high-water mark here **before** it returns the
/// number. A store that cannot write fails the allocation, so the number is not handed out and
/// no gap-less guarantee is broken.
pub trait HighWaterStore: Send + Sync {
    /// The stored state for `series` on `node`, if any.
    fn load(&self, series: &SeriesKey, node: &NodeId) -> Option<StoredSeries>;

    /// Writes the state for `series` on `node`.
    ///
    /// # Errors
    ///
    /// [`PartitionError::Store`] when the write fails.
    fn save(
        &self,
        series: &SeriesKey,
        node: &NodeId,
        state: StoredSeries,
    ) -> Result<(), PartitionError>;

    /// Installs `state` only when it is above what is stored, as one step.
    ///
    /// This is the compare-and-set [`BranchSeries::install`] uses: the check against the stored
    /// value and the write happen together, so two callers installing overlapping leases cannot
    /// both win. A store shared by several processes overrides this with a real
    /// compare-and-set; the default body loads, decides and then writes, which is atomic only
    /// within one process and only for a store that serialises its own writers.
    ///
    /// Returns the high-water mark the series must continue from: `state.next` when the install
    /// advanced, or the **stored** mark when the same lease id is re-installed to resume.
    ///
    /// # Errors
    ///
    /// [`PartitionError::LeaseNotAboveStored`] when `state.lease` is neither the stored lease nor
    /// strictly above it — nothing is written then — and [`PartitionError::Store`] when the
    /// write fails.
    fn save_if_above(
        &self,
        series: &SeriesKey,
        node: &NodeId,
        state: StoredSeries,
    ) -> Result<u64, PartitionError> {
        let decision = match self.load(series, node) {
            Some(stored) => stored.install_decision(&state.lease),
            None => Some(InstallDecision::Advance(state.next)),
        };
        match decision {
            Some(InstallDecision::Resume(next)) => Ok(next),
            Some(InstallDecision::Advance(next)) => {
                self.save(series, node, state)?;
                Ok(next)
            }
            None => Err(PartitionError::LeaseNotAboveStored {
                series: series.as_str().to_owned(),
                node: node.to_string(),
            }),
        }
    }
}

/// An in-memory [`HighWaterStore`] for one node and for tests.
#[derive(Debug, Default)]
pub struct MemorySeriesStore {
    inner: Mutex<HashMap<(String, String), StoredSeries>>,
}

impl MemorySeriesStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl HighWaterStore for MemorySeriesStore {
    fn load(&self, series: &SeriesKey, node: &NodeId) -> Option<StoredSeries> {
        lock(&self.inner)
            .get(&(series.as_str().to_owned(), node.to_string()))
            .cloned()
    }

    fn save(
        &self,
        series: &SeriesKey,
        node: &NodeId,
        state: StoredSeries,
    ) -> Result<(), PartitionError> {
        lock(&self.inner).insert((series.as_str().to_owned(), node.to_string()), state);
        Ok(())
    }

    /// The compare-and-set under one lock: the load, the decision and the insert happen while
    /// the guard is held, so two threads cannot both install an overlapping lease.
    fn save_if_above(
        &self,
        series: &SeriesKey,
        node: &NodeId,
        state: StoredSeries,
    ) -> Result<u64, PartitionError> {
        let key = (series.as_str().to_owned(), node.to_string());
        let mut inner = lock(&self.inner);
        let decision = match inner.get(&key) {
            Some(stored) => stored.install_decision(&state.lease),
            None => Some(InstallDecision::Advance(state.next)),
        };
        match decision {
            Some(InstallDecision::Resume(next)) => Ok(next),
            Some(InstallDecision::Advance(next)) => {
                inner.insert(key, state);
                Ok(next)
            }
            None => Err(PartitionError::LeaseNotAboveStored {
                series: series.as_str().to_owned(),
                node: node.to_string(),
            }),
        }
    }
}

/// A branch node's local allocator for one naming series.
///
/// Build one with [`BranchSeries::install`] when the hub grants a lease, or with
/// [`BranchSeries::reopen`] after a restart; then call [`BranchSeries::allocate`] for each
/// number. The high-water mark lives in the [`HighWaterStore`], so a restart resumes where it
/// stopped.
///
/// There is one owner per series and node. The series is deliberately **not** `Clone`: two
/// copies would each hand out the same next number from their own cursor and both write the
/// store, which is how two documents end up with one name. A second install of the
/// same range is refused by the store for the same reason.
///
/// ```compile_fail
/// # use std::sync::Arc;
/// # use spot_partition::{BranchSeries, HighWaterStore, LeaseId, LowWater, RangeLease};
/// # use spot_storage_contract::SeriesKey;
/// # use spot_sync_protocol::NodeId;
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let store: Arc<dyn HighWaterStore> = Arc::new(spot_partition::MemorySeriesStore::new());
/// let lease = RangeLease::new(
///     SeriesKey::new("INV-")?,
///     1,
///     10,
///     NodeId::new("edge-khi#branch_07")?,
///     LeaseId::new(1),
/// )?;
/// let series = BranchSeries::install(lease, LowWater::half(), store)?;
/// let copy = series.clone();
/// # let _ = copy;
/// # Ok(())
/// # }
/// ```
///
/// The example above does not compile, on purpose: `BranchSeries` has no `clone`.
pub struct BranchSeries {
    series: SeriesKey,
    node: NodeId,
    lease: RangeLease,
    next: u64,
    low: LowWater,
    store: Arc<dyn HighWaterStore>,
}

impl std::fmt::Debug for BranchSeries {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BranchSeries")
            .field("series", &self.series)
            .field("node", &self.node)
            .field("lease", &self.lease)
            .field("next", &self.next)
            .field("low", &self.low)
            .finish_non_exhaustive()
    }
}

impl BranchSeries {
    /// Installs a freshly granted lease and persists its starting high-water mark.
    ///
    /// The store decides, in one atomic step, whether the lease may replace what it holds:
    ///
    /// * nothing stored yet — a **first install**, which starts at `lease.from`;
    /// * the same lease id already stored — a **resume**, which continues at the stored
    ///   high-water mark, never at `lease.from`, so a restart reissues no number;
    /// * `lease.from` strictly above both the stored lease's last number and the last number
    ///   handed out — the lease replaces the stored one and starts at `lease.from`.
    ///
    /// Anything else is refused and the stored state is left exactly as it was.
    ///
    /// # Errors
    ///
    /// [`PartitionError::LeaseNotAboveStored`] when the offered lease is not above the stored
    /// one, and [`PartitionError::Store`] when the high-water mark cannot be written.
    pub fn install(
        lease: RangeLease,
        low: LowWater,
        store: Arc<dyn HighWaterStore>,
    ) -> Result<Self, PartitionError> {
        let series = lease.series.clone();
        let node = lease.node.clone();
        let next = store.save_if_above(
            &series,
            &node,
            StoredSeries {
                lease: lease.clone(),
                next: lease.from,
            },
        )?;
        Ok(Self {
            series,
            node,
            next,
            lease,
            low,
            store,
        })
    }

    /// Reopens the series for `node` from the store after a restart.
    ///
    /// # Errors
    ///
    /// [`PartitionError::NoLease`] when the store holds no state for the series and node, and
    /// [`PartitionError::InvalidRange`] when the stored high-water mark is outside the stored
    /// lease.
    pub fn reopen(
        series: &SeriesKey,
        node: &NodeId,
        low: LowWater,
        store: Arc<dyn HighWaterStore>,
    ) -> Result<Self, PartitionError> {
        let state = store
            .load(series, node)
            .ok_or_else(|| PartitionError::NoLease {
                series: series.as_str().to_owned(),
                node: node.to_string(),
            })?;
        if &state.lease.series != series || &state.lease.node != node {
            return Err(PartitionError::InvalidRange {
                reason: "the stored lease is for another series or node".to_owned(),
            });
        }
        if state.next < state.lease.from || state.next > state.lease.to + 1 {
            return Err(PartitionError::InvalidRange {
                reason: "the stored high-water mark is outside the stored lease".to_owned(),
            });
        }
        Ok(Self {
            series: series.clone(),
            node: node.clone(),
            lease: state.lease,
            next: state.next,
            low,
            store,
        })
    }

    /// Allocates the next number, persisting the high-water mark before it is used.
    ///
    /// # Errors
    ///
    /// [`PartitionError::RangeExhausted`] when the lease has no numbers left, and
    /// [`PartitionError::Store`] when the high-water mark cannot be written.
    pub fn allocate(&mut self) -> Result<u64, PartitionError> {
        if self.next > self.lease.to {
            return Err(PartitionError::RangeExhausted {
                series: self.series.as_str().to_owned(),
                node: self.node.to_string(),
            });
        }
        let number = self.next;
        let next = number + 1;
        self.store.save(
            &self.series,
            &self.node,
            StoredSeries {
                lease: self.lease.clone(),
                next,
            },
        )?;
        self.next = next;
        Ok(number)
    }

    /// Allocates the next number and renders it as a record name with a plain `prefix`.
    ///
    /// This is for a simple prefix-and-number series. A naming pattern with date parts or a
    /// zero-padded counter is rendered by [`spot_naming::Namer`] from the same number.
    ///
    /// # Errors
    ///
    /// As [`BranchSeries::allocate`], and [`PartitionError::InvalidArgument`] when
    /// `prefix` plus the number is not a legal record name.
    pub fn allocate_name(&mut self, prefix: &str) -> Result<RecordName, PartitionError> {
        let number = self.allocate()?;
        let name = format!("{prefix}{number}");
        RecordName::new(&name).map_err(|e| PartitionError::InvalidArgument {
            reason: format!("the allocated name is invalid: {e}"),
        })
    }

    /// How many numbers remain in the lease.
    #[must_use]
    pub fn remaining(&self) -> u64 {
        if self.next > self.lease.to {
            0
        } else {
            self.lease.to - self.next + 1
        }
    }

    /// Whether the remaining fraction of the lease is at or below the configured threshold.
    #[must_use]
    pub fn running_low(&self) -> bool {
        self.low.is_low(self.remaining(), self.lease.size())
    }

    /// The naming series.
    #[must_use]
    pub fn series(&self) -> &SeriesKey {
        &self.series
    }

    /// The node the series belongs to.
    #[must_use]
    pub fn node(&self) -> &NodeId {
        &self.node
    }

    /// The lease in force.
    #[must_use]
    pub fn lease(&self) -> &RangeLease {
        &self.lease
    }
}
