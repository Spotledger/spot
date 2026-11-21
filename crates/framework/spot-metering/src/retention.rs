//! Retention: how long the deduplication tombstones outlive their period.
//!
//! A store keeps one tombstone per recorded `(tenant, event id)` so that a retried
//! event is a no-op. Without a bound those tombstones outlive every total they protect,
//! so the table grows with every event ever metered (a recorded security finding: a period's raw metadata
//! was dropped, but its ids were kept forever). [`RetentionPolicy`] bounds them:
//! compaction keeps a tombstone for `tombstone_horizon_periods` periods past its cut
//! and removes every tombstone older than that.
//!
//! # The invariant that makes removal safe
//!
//! Deleting a tombstone would normally let a replayed id be counted twice. It cannot
//! here, because the same cut also becomes the store's **late-arrival floor**: once a
//! tombstone is gone, every event whose period ends at or before the floor's start is
//! refused as
//! [`MeteringError::LateBeyondHorizon`](crate::error::MeteringError::LateBeyondHorizon)
//! — whether or not the store has ever seen its id, and therefore whether or not its
//! tombstone still exists. The purged set and the refused set are the same set by
//! construction, so one of these is always true for a replayed id:
//!
//! * its period is **inside** the horizon: its tombstone is still there and the replay
//!   is a no-op;
//! * its period is **beyond** the horizon: its tombstone is gone, and the replay is
//!   refused rather than counted again.
//!
//! A *first-time* event beyond the horizon is refused too. That is deliberate: the
//! horizon is the store's late-arrival window, and refusing a very late event is
//! recoverable (the producer still holds the fact) while a double count is not.
//!
//! # Sizing the horizon
//!
//! `tombstone_horizon_periods` must be **longer than the longest late-arrival window
//! the deployment promises**. Nothing in this crate's policy refuses an old instant on
//! its own, so before a retention policy exists the window is unbounded; the horizon
//! is what bounds it. Size it as:
//!
//! ```text
//! horizon  >=  the longest a producer may retry a fact          (outbox/retry window)
//!          +  the lag between a period closing and its last
//!             reconciliation run                              (reconciliation lag)
//!          +  1 period of slack for a month-boundary race
//! ```
//!
//! The default, [`RetentionPolicy::DEFAULT_HORIZON_PERIODS`] (13), keeps thirteen
//! periods of tombstones — a year plus the reconciliation month — which is more than
//! any producer retry window this platform configures. An operator running an
//! aggressive storage budget may lower it, but not below
//! [`RetentionPolicy::MIN_HORIZON_PERIODS`] (2, one period of lag plus one of slack):
//! [`MeterStore::compact`](crate::store::MeterStore::compact) refuses a shorter horizon,
//! because every event still inside a producer's retry window that the floor refuses is
//! **unbilled usage**, and no cleanup job can tell a genuine late fact from a replay. An
//! operator replaying a long backlog after an outage must raise the horizon **before**
//! compacting with the new cut, because the floor only ever moves forward: nothing can
//! lower it again without accepting the risk of counting a purged id twice.
//!
//! Compaction only ever touches periods whose totals are **final**. That statement is
//! the caller's: the `before` cut names the first period whose totals may still
//! change, so every earlier period is closed and reconciled — the same precondition
//! raw-metadata compaction already relied on. The horizon is applied *on top of* that
//! cut, never in its place, so a tombstone is removed only after
//! `before` (final) **and** `horizon` (no longer replayable) both hold.

use crate::error::MeteringError;
use crate::period::{PeriodKey, PeriodResolver};

/// How much deduplication history compaction keeps.
///
/// Time is an argument, never a clock reading: the policy is relative to the cut a
/// caller passes to [`MeterStore::compact`](crate::store::MeterStore::compact).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetentionPolicy {
    /// Tombstones for periods at least this many periods older than the compaction cut
    /// are removed. Must be at least [`RetentionPolicy::MIN_HORIZON_PERIODS`] to compact:
    /// a shorter horizon turns a producer's still-retryable events into refusals, which is
    /// unbilled usage, so it is a configuration error rather than a storage trade-off.
    pub tombstone_horizon_periods: u32,
}

impl RetentionPolicy {
    /// The default horizon: thirteen periods. See the module docs for how to size it.
    pub const DEFAULT_HORIZON_PERIODS: u32 = 13;

    /// The shortest horizon compaction accepts: two periods.
    ///
    /// One period absorbs the lag between a period closing and its last reconciliation, and
    /// one the month-boundary race; anything shorter is inside the retry window a producer
    /// is entitled to. The bound exists because a refuted late event is lost revenue, while
    /// a slightly larger tombstone table is only storage.
    pub const MIN_HORIZON_PERIODS: u32 = 2;

    /// A policy keeping tombstones for `tombstone_horizon_periods` periods.
    ///
    /// This constructor does not validate; [`RetentionPolicy::try_new`] does, and
    /// [`MeterStore::compact`](crate::store::MeterStore::compact) refuses any policy below
    /// [`RetentionPolicy::MIN_HORIZON_PERIODS`]. Prefer `try_new` for a value that comes from
    /// configuration.
    #[must_use]
    pub const fn new(tombstone_horizon_periods: u32) -> Self {
        Self {
            tombstone_horizon_periods,
        }
    }

    /// A policy keeping tombstones for `tombstone_horizon_periods` periods, or
    /// [`MeteringError::RetentionHorizonTooShort`] when that is below
    /// [`RetentionPolicy::MIN_HORIZON_PERIODS`].
    ///
    /// # Errors
    /// [`MeteringError::RetentionHorizonTooShort`] for a horizon below the minimum.
    pub fn try_new(tombstone_horizon_periods: u32) -> Result<Self, MeteringError> {
        Self::new(tombstone_horizon_periods).validated()
    }

    /// Checks the horizon is at least [`RetentionPolicy::MIN_HORIZON_PERIODS`].
    ///
    /// # Errors
    /// [`MeteringError::RetentionHorizonTooShort`] for a horizon below the minimum.
    pub fn validated(self) -> Result<Self, MeteringError> {
        if self.tombstone_horizon_periods < Self::MIN_HORIZON_PERIODS {
            return Err(MeteringError::RetentionHorizonTooShort {
                periods: self.tombstone_horizon_periods,
                min: Self::MIN_HORIZON_PERIODS,
            });
        }
        Ok(self)
    }

    /// The horizon in periods.
    #[must_use]
    pub const fn horizon_periods(&self) -> u32 {
        self.tombstone_horizon_periods
    }

    /// The first period whose tombstones survive a compaction cut at `before`, or
    /// `None` when the horizon reaches back past the representable calendar and nothing
    /// may be removed.
    ///
    /// This is the pure walk; it does not enforce
    /// [`RetentionPolicy::MIN_HORIZON_PERIODS`] (compaction does), so a caller may ask
    /// where any horizon would cut.
    ///
    /// Every period that ends at or before this period's start has its tombstones
    /// removed, and every event whose period ends at or before this period's start is
    /// refused by the store from then on. One period, one answer: the two sets cannot
    /// drift apart.
    ///
    /// The walk is exact and uses only the caller's resolver: the instant one second
    /// before a window's start falls in the previous window by construction, so
    /// stepping back is `resolve(start - 1)` — never an assumed month length, never a
    /// clock, never a float.
    ///
    /// # Errors
    /// Whatever the resolver fails with, and
    /// [`MeteringError::InvalidPeriodSpec`] when a resolver returns a window that does
    /// not end where the next one starts (the store could then never name a cut).
    pub fn tombstone_cut(
        &self,
        before: &PeriodKey,
        tenant: &str,
        resolver: &dyn PeriodResolver,
    ) -> Result<Option<PeriodKey>, MeteringError> {
        let mut cut = before.clone();
        for _ in 0..self.tombstone_horizon_periods {
            let Some(previous_instant) = cut.start.checked_sub(1) else {
                // Nothing is older than the start of the representable calendar.
                return Ok(None);
            };
            let previous = resolver.resolve(tenant, previous_instant)?;
            if previous.start >= cut.start || previous.end_exclusive != cut.start {
                return Err(MeteringError::InvalidPeriodSpec {
                    reason: format!(
                        "period {} does not end where {} starts",
                        previous.label, cut.label
                    ),
                });
            }
            cut = previous;
        }
        Ok(Some(cut))
    }
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self::new(Self::DEFAULT_HORIZON_PERIODS)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::period::{PeriodSpec, TenantPeriods, period_of};

    fn resolver() -> TenantPeriods {
        TenantPeriods::new(PeriodSpec::calendar_month("UTC").unwrap())
    }

    #[test]
    fn the_pure_walk_with_no_horizon_cuts_at_the_compaction_period_itself() {
        // `tombstone_cut` is the pure walk: compaction refuses a horizon this short, but
        // the walk itself still answers where it would cut.
        let resolver = resolver();
        let february = period_of(2_678_400, resolver.spec_for("acme")).unwrap();
        let cut = RetentionPolicy::new(0)
            .tombstone_cut(&february, "acme", &resolver)
            .unwrap()
            .unwrap();
        assert_eq!(cut, february);
    }

    #[test]
    fn a_horizon_below_the_minimum_is_refused_before_anything_is_purged() {
        assert_eq!(RetentionPolicy::MIN_HORIZON_PERIODS, 2);
        for periods in [0, 1] {
            let error = RetentionPolicy::try_new(periods).unwrap_err();
            assert_eq!(error.code(), "metering.retention_horizon_too_short");
            assert_eq!(
                error,
                MeteringError::RetentionHorizonTooShort { periods, min: 2 }
            );
        }
        assert_eq!(RetentionPolicy::try_new(2).unwrap().horizon_periods(), 2);
        assert_eq!(
            RetentionPolicy::try_new(13).unwrap(),
            RetentionPolicy::default()
        );
    }

    #[test]
    fn the_walk_back_is_exact_across_a_year_boundary_and_a_short_month() {
        let resolver = resolver();
        let march = period_of(5_097_600, resolver.spec_for("acme")).unwrap(); // 1970-03
        let cut = RetentionPolicy::new(2)
            .tombstone_cut(&march, "acme", &resolver)
            .unwrap()
            .unwrap();
        assert_eq!(cut.label, "1970-01");
        assert_eq!(cut.start, 0);
        // The same walk in a zone whose month starts earlier in UTC still lands on
        // window boundaries, because it is the resolver's own answer.
        let karachi = TenantPeriods::new(PeriodSpec::calendar_month("Asia/Karachi").unwrap());
        let march = period_of(5_097_600, karachi.spec_for("acme")).unwrap();
        let cut = RetentionPolicy::new(1)
            .tombstone_cut(&march, "acme", &karachi)
            .unwrap()
            .unwrap();
        assert_eq!(cut, period_of(2_678_400, karachi.spec_for("acme")).unwrap());
        assert_eq!(cut.end_exclusive, march.start);
    }

    #[test]
    fn the_default_keeps_a_year_and_the_reconciliation_month() {
        assert_eq!(RetentionPolicy::default().horizon_periods(), 13);
        assert_eq!(RetentionPolicy::default(), RetentionPolicy::new(13));
    }

    #[test]
    fn the_walk_crosses_the_epoch_without_a_special_case() {
        let resolver = resolver();
        let january = period_of(0, resolver.spec_for("acme")).unwrap();
        let cut = RetentionPolicy::new(1)
            .tombstone_cut(&january, "acme", &resolver)
            .unwrap()
            .unwrap();
        assert_eq!(cut.label, "1969-12");
        assert_eq!(cut.end_exclusive, 0);
    }
}
