//! `spot-leased-sequences` — the offline [`Sequences`] a branch runs over hub-leased ranges
//! (WP-EDG-03.T3; ADR-062 section 7).
//!
//! A branch that is offline cannot use the hub's transactional allocator, so
//! [`spot_partition`] hands it a **leased range** of a naming series ([`BranchSeries`]). This
//! crate wraps a set of those branch series — keyed by [`SeriesKey`] — in the storage
//! [`Sequences`] trait, so the same [`spot_naming::Namer`] that names a document on the hub
//! names it on the branch: `INV-.YYYY-.#####` becomes `INV-2026-00001` from the first number of
//! the leased range, prefix, date parts and zero padding included. Two branches with disjoint
//! leases of the same series therefore render disjoint names, and the first sync does not have
//! to reconcile two numbering schemes.
//!
//! # Allocation never reaches the hub
//!
//! [`LeasedSequences::allocate`] hands out consecutive numbers from the lease alone. It never
//! asks the hub for a number and never falls back to a local counter: a series with no lease, or
//! a lease with fewer numbers left than the caller asked for, fails with
//! [`LeasedSequencesError::LeaseExhausted`], which converts to
//! [`spot_storage_contract::ErrorKind::Unavailable`] — the kind a caller already treats as
//! "retry after renewing the lease". The node must block document creation of that series until
//! the sync layer installs another range.
//!
//! # Rollback: a rented number is spent, not returned
//!
//! The hub's transactional allocator is gap-less: a number allocated inside a transaction that
//! rolls back returns to the series, so committed numbers have no gaps. **A leased allocator
//! cannot do that**, and this difference is deliberate:
//!
//! * The number left the hub's allocator when the range was leased. Nobody else will be given
//!   it, so reusing it would not corrupt this lease.
//! * But the branch cannot prove to the hub that the number was never written. After a crash,
//!   a commit whose outcome the branch does not know, or a sync that lands before the rollback
//!   is known, a reused number could collide with a document another branch already holds.
//!
//! So a number allocated in a transaction that rolls back is **lost**: it leaves a gap and is
//! never handed out again, and [`BranchSeries`] persists the high-water mark before the number
//! is returned so even a restart cannot reissue it. Callers that require gap-less numbers
//! must name documents on the hub; a branch tolerates gaps in exchange for never colliding.
//! Ranges of different branches never overlap because the hub grants disjoint ranges and this
//! crate allocates only inside the range it was given.
//!
//! # Seeing exhaustion coming
//!
//! [`LeasedSequences::remaining`] and [`LeasedSequences::running_low`] expose the state of a
//! lease — the same low-water signal [`BranchSeries::running_low`] already computes — so the
//! sync layer can ask the hub for a new range before the current one runs dry. A series with no
//! lease at all reports nothing remaining and is always running low.
//!
//! # Time is an argument
//!
//! Nothing here reads the wall clock. Lease grants and date parts take the instants the caller
//! passes in (ADR-077); this crate only carries numbers.
//!
//! # Example
//!
//! ```
//! use spot_leased_sequences::{LeasedSequences, LeasedSequencesError};
//! use spot_storage_contract::SeriesKey;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let key = SeriesKey::new("INV-2026-")?;
//! // Until the hub grants a range, the allocator fails closed instead of inventing a number.
//! let mut seq = LeasedSequences::new();
//! assert_eq!(seq.remaining(&key), 0);
//! assert!(matches!(
//!     seq.allocate_block(&key, 1),
//!     Err(LeasedSequencesError::LeaseExhausted { .. })
//! ));
//!
//! // A real caller installs the `BranchSeries` it leased from
//! // `spot_partition::RangeAllocator::lease` with `LeasedSequences::with_lease`, or reopens it
//! // with `BranchSeries::reopen` after a restart.
//! # Ok(())
//! # }
//! ```

#![warn(missing_docs)]

use std::collections::BTreeMap;
use std::future::{Future, ready};
use std::sync::Arc;

use spot_partition::{BranchSeries, HighWaterStore, LowWater, PartitionError, RangeLease};
use spot_storage_contract::{Sequences, SeriesKey, StorageError, StorageResult};

mod error;

pub use error::{LeaseRefusal, LeasedSequencesError};

/// The storage [`Sequences`] a branch runs over hub-leased ranges, one [`BranchSeries`] per
/// series key.
///
/// Install a series when the hub grants a lease ([`LeasedSequences::with_lease`],
/// [`LeasedSequences::install`]); the naming layer then allocates from it exactly as it does on
/// the hub. Unleased series fail rather than fall back, and [`Sequences::set_next`] is refused:
/// a branch never moves its own cursor.
///
/// Deliberately not `Clone`: two copies would each hand out the same next number from the same
/// lease. There is one allocator per branch and series.
#[derive(Debug, Default)]
pub struct LeasedSequences {
    series: BTreeMap<SeriesKey, BranchSeries>,
}

impl LeasedSequences {
    /// An allocator with no leases. Every allocation fails until a lease is installed.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The allocator with `branch` installed (builder form).
    ///
    /// # Errors
    ///
    /// As [`LeasedSequences::install`]; a refused lease leaves nothing installed.
    pub fn with_lease(mut self, branch: BranchSeries) -> Result<Self, LeasedSequencesError> {
        self.install(branch)?;
        Ok(self)
    }

    /// Installs or renews a branch series, returning the series it replaced, if any.
    ///
    /// Renewal fails closed. A number must never be issued twice, and replacing a held
    /// [`BranchSeries`] resets the saved high-water mark to the new range's first number, so a
    /// stale, overlapping, lower or repeated lease would reissue numbers. When a lease is
    /// already held for the series the new one is therefore refused unless it is for the same
    /// node **and** its range lies strictly above the held range
    /// (`new.from > held.to`). A refused lease leaves the held one untouched. Resuming a held
    /// range after a restart is [`BranchSeries::reopen`] followed by `install` into an
    /// allocator that holds nothing yet.
    ///
    /// `BranchSeries::install` has already written its starting mark to the store by the time
    /// it is passed here, so prefer [`LeasedSequences::renew`], which checks first.
    ///
    /// # Errors
    ///
    /// [`LeasedSequencesError::LeaseRefused`] as described above.
    pub fn install(
        &mut self,
        branch: BranchSeries,
    ) -> Result<Option<BranchSeries>, LeasedSequencesError> {
        if let Some(held) = self.series.get(branch.series()) {
            check_renewal(held.lease(), branch.lease())?;
        }
        Ok(self.series.insert(branch.series().clone(), branch))
    }

    /// Renews (or first installs) the lease of its series, checking **before** anything is
    /// written to `store`.
    ///
    /// The offered `lease` is checked against the lease held here and against the state the
    /// `store` already keeps for the series and node (a restart that has not reopened it yet),
    /// and refused unless it lies strictly above both. Only then is the high-water mark written
    /// ([`BranchSeries::install`]). This is the entry point for a lease just granted by the hub.
    ///
    /// # Errors
    ///
    /// [`LeasedSequencesError::LeaseRefused`] when the lease is not strictly above, or is for
    /// another node, and [`LeasedSequencesError::Store`] when the high-water mark cannot be
    /// written (the held lease is then untouched).
    pub fn renew(
        &mut self,
        lease: RangeLease,
        low: LowWater,
        store: Arc<dyn HighWaterStore>,
    ) -> Result<Option<BranchSeries>, LeasedSequencesError> {
        if let Some(held) = self.series.get(&lease.series) {
            check_renewal(held.lease(), &lease)?;
        }
        if let Some(stored) = store.load(&lease.series, &lease.node) {
            check_renewal(&stored.lease, &lease)?;
        }
        let branch = BranchSeries::install(lease, low, store).map_err(|error| {
            LeasedSequencesError::Store {
                reason: error.to_string(),
            }
        })?;
        self.install(branch)
    }

    /// Removes the lease of `series`, for example when the sync layer starts over from a fresh
    /// snapshot.
    pub fn remove(&mut self, series: &SeriesKey) -> Option<BranchSeries> {
        self.series.remove(series)
    }

    /// The branch series installed for `series`, if any.
    #[must_use]
    pub fn branch(&self, series: &SeriesKey) -> Option<&BranchSeries> {
        self.series.get(series)
    }

    /// How many numbers the lease of `series` still holds; zero when it holds no lease.
    #[must_use]
    pub fn remaining(&self, series: &SeriesKey) -> u64 {
        self.series.get(series).map_or(0, BranchSeries::remaining)
    }

    /// Whether `series` is running low — or holds no lease at all, which is as low as it gets.
    ///
    /// The threshold is the [`LowWater`](spot_partition::LowWater) the branch series was
    /// installed with; the comparison is exact integer arithmetic (ADR-12).
    #[must_use]
    pub fn running_low(&self, series: &SeriesKey) -> bool {
        self.series
            .get(series)
            .is_none_or(BranchSeries::running_low)
    }

    /// How many series hold a lease.
    #[must_use]
    pub fn len(&self) -> usize {
        self.series.len()
    }

    /// Whether no series holds a lease.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.series.is_empty()
    }

    /// The series keys that hold a lease, in key order.
    pub fn series_keys(&self) -> impl Iterator<Item = &SeriesKey> {
        self.series.keys()
    }

    /// Allocates `count` consecutive numbers of `series` from its lease, returning the first.
    ///
    /// This is the synchronous allocation the [`Sequences`] implementation forwards to, for
    /// callers that do not need the async trait.
    ///
    /// # Errors
    ///
    /// * [`LeasedSequencesError::InvalidCount`] when `count` is zero.
    /// * [`LeasedSequencesError::LeaseExhausted`] when the series holds no lease or fewer than
    ///   `count` numbers are left. Nothing is consumed in that case.
    /// * [`LeasedSequencesError::Store`] when the high-water mark cannot be written. Numbers
    ///   already handed out from the block in that round are lost (a gap), never reused.
    pub fn allocate_block(
        &mut self,
        series: &SeriesKey,
        count: u32,
    ) -> Result<u64, LeasedSequencesError> {
        if count == 0 {
            return Err(LeasedSequencesError::InvalidCount { count });
        }
        let Some(branch) = self.series.get_mut(series) else {
            return Err(LeasedSequencesError::LeaseExhausted {
                series: series.as_str().to_owned(),
                requested: count,
                remaining: 0,
            });
        };
        let remaining = branch.remaining();
        if remaining < u64::from(count) {
            return Err(LeasedSequencesError::LeaseExhausted {
                series: series.as_str().to_owned(),
                requested: count,
                remaining,
            });
        }
        let first = branch
            .allocate()
            .map_err(|error| exhausted_or_store(error, series, count))?;
        for _ in 1..count {
            branch
                .allocate()
                .map_err(|error| exhausted_or_store(error, series, count))?;
        }
        Ok(first)
    }
}

/// Refuses `offered` unless it may follow `held`: the same node, and strictly above.
fn check_renewal(held: &RangeLease, offered: &RangeLease) -> Result<(), LeasedSequencesError> {
    let refuse = |reason| LeasedSequencesError::LeaseRefused {
        series: offered.series.as_str().to_owned(),
        reason,
    };
    if held.node != offered.node {
        return Err(refuse(LeaseRefusal::OtherNode));
    }
    if offered.from <= held.to {
        return Err(refuse(LeaseRefusal::NotAbove));
    }
    Ok(())
}

/// Maps a [`PartitionError`] from the branch series onto this crate's error vocabulary.
fn exhausted_or_store(
    error: PartitionError,
    series: &SeriesKey,
    requested: u32,
) -> LeasedSequencesError {
    match error {
        PartitionError::RangeExhausted { .. } => LeasedSequencesError::LeaseExhausted {
            series: series.as_str().to_owned(),
            requested,
            remaining: 0,
        },
        // No other variant is produced by `BranchSeries::allocate`; a store failure is the one
        // real case, and it must fail the allocation rather than hand out an unrecorded number.
        other => LeasedSequencesError::Store {
            reason: other.to_string(),
        },
    }
}

impl Sequences for LeasedSequences {
    /// Hands out `count` consecutive numbers from the series' lease.
    ///
    /// Unlike the hub's transactional allocator, the result does not depend on the caller's
    /// transaction: a rollback does not return the numbers (see the crate docs). The future is
    /// ready immediately — there is no I/O and no lock held across an await.
    ///
    /// # Errors
    ///
    /// As [`LeasedSequences::allocate_block`], converted to a [`StorageError`].
    fn allocate(
        &mut self,
        series: &SeriesKey,
        count: u32,
    ) -> impl Future<Output = StorageResult<u64>> + Send {
        let result = self
            .allocate_block(series, count)
            .map_err(StorageError::from);
        ready(result)
    }

    /// Refused: leases, and the next number they start at, are issued by the hub.
    ///
    /// A branch that could set the cursor could hand out a number the hub will also give to
    /// another branch once this one syncs. The failure is
    /// [`spot_storage_contract::ErrorKind::Unsupported`], so a caller that only retries
    /// transport failures does not loop.
    ///
    /// # Errors
    ///
    /// Always [`LeasedSequencesError::SetNextRefused`], converted to a [`StorageError`].
    fn set_next(
        &mut self,
        series: &SeriesKey,
        next: u64,
    ) -> impl Future<Output = StorageResult<()>> + Send {
        let error = LeasedSequencesError::SetNextRefused {
            series: series.as_str().to_owned(),
            next,
        };
        ready(Err(StorageError::from(error)))
    }
}
