//! The hot path: a lock-free, store-free allow/deny backed by a swapped snapshot.
//!
//! [`QuotaGuard::allow`] is what a document-creation or sign-up path calls. It reads
//! an `Arc`-swapped [`QuotaSnapshot`] — no lock, no store call, no allocation on the
//! read — and answers with [`ActionDecision`]. [`QuotaGuard::refresh`] rebuilds the
//! snapshot from a store; a flusher calls it after draining the sink.
//!
//! # Stale snapshots
//!
//! A snapshot older than the configured bound (`max_age`) is *stale*, and staleness
//! is handled by mode, never by guessing:
//!
//! - **warn-only meters fail open** — the action is allowed, with the staleness
//!   reason available, because blocking a customer over a metrics-collection lag
//!   would be a worse fault than a late warning;
//! - **`HardBlock` meters fail closed** — the action is refused with
//!   [`reasons::QUOTA_SNAPSHOT_STALE`], because a hard limit is a contract.
//!
//! # A metering outage
//!
//! When the store is down, [`QuotaGuard::refresh`] returns
//! [`MeteringError::StoreUnavailable`](crate::error::MeteringError::StoreUnavailable) and
//! leaves the previous snapshot in place; nothing is guessed. The snapshot then ages into the
//! stale rules above, so the outage rule is the staleness rule: a `HardBlock` meter fails
//! closed once the snapshot is stale (an over-quota action is never silently allowed for a
//! hard quota), every other mode fails open, and read-only paths (reading, export, sign-in of
//! an existing user, reversals, cancellations) are never blocked.
//!
//! Either way the guard says which happened: [`QuotaGuard::staleness`] reports the
//! age and whether it is stale, so a caller can log it rather than discover it in
//! production.

use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;

use crate::decision::{ActionDecision, QuotaAction, QuotaDecision, QuotaReport};
use crate::meter::Meter;
use crate::period::{PeriodKey, PeriodResolver};
use crate::policy::QuotaMode;
use crate::store::MeterStore;

/// The stable refusal code a stale `HardBlock` snapshot carries.
pub mod reasons {
    /// A tenant attempted to use another tenant's cached verdict.
    pub const TENANT_MISMATCH: &str = "metering.tenant_mismatch";
    /// A `HardBlock` meter's snapshot is older than the guard's bound.
    pub const QUOTA_SNAPSHOT_STALE: &str = "metering.quota_snapshot_stale";
}

/// An immutable, cheap-to-clone view of every meter's verdict, plus the licence state.
///
/// The guard swaps a whole `Arc<QuotaSnapshot>` at once, so a reader either sees the
/// previous evaluation or the next one, never a torn mix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuotaSnapshot {
    /// The evaluated report.
    pub report: QuotaReport,
    /// When the snapshot was built, UTC seconds (from the caller's clock, not read
    /// here).
    pub built_at: i64,
    /// The modes in force per meter, so staleness can be resolved without the policy.
    pub modes: std::collections::BTreeMap<Meter, QuotaMode>,
}

impl QuotaSnapshot {
    /// A snapshot from a report and the modes it was built under.
    #[must_use]
    pub fn new(
        report: QuotaReport,
        built_at: i64,
        modes: std::collections::BTreeMap<Meter, QuotaMode>,
    ) -> Self {
        Self {
            report,
            built_at,
            modes,
        }
    }

    /// An empty snapshot for a period, used before the first refresh.
    #[must_use]
    pub fn empty(period: PeriodKey, licence: spot_license::Status, now: i64) -> Self {
        Self {
            report: QuotaReport {
                period,
                decisions: std::collections::BTreeMap::new(),
                licence,
                unknown_limits: Vec::new(),
                dropped: 0,
            },
            built_at: now,
            modes: std::collections::BTreeMap::new(),
        }
    }

    /// The metrics a meter's verdict is gated by, or `None` if it has no `Blocked`
    /// verdict and no declared mode.
    #[must_use]
    fn mode_of(&self, meter: &Meter) -> Option<QuotaMode> {
        self.modes.get(meter).copied()
    }
}

/// The staleness of the current snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Staleness {
    /// The snapshot's age at the moment asked, in seconds (never negative).
    pub age: i64,
    /// Whether the age exceeds the guard's bound.
    pub is_stale: bool,
    /// The bound in force, in seconds.
    pub max_age: i64,
}

/// Inputs for refreshing one tenant's immutable quota snapshot. Time is explicit.
pub struct RefreshContext<'a> {
    /// Tenant whose totals are read.
    pub tenant: &'a str,
    /// Period to evaluate.
    pub period: &'a PeriodKey,
    /// Tenant time-zone resolver.
    pub resolver: &'a dyn PeriodResolver,
    /// Entitlements combined with the tenant policy.
    pub policy: &'a crate::policy::QuotaPolicy,
    /// Already evaluated licence status.
    pub licence: spot_license::Status,
    /// Unmapped licence limits to report to the host.
    pub unknown_limits: Vec<String>,
    /// Cumulative sink loss count.
    pub dropped: u64,
    /// Caller-supplied UTC seconds.
    pub now: i64,
}

/// The caller-facing guard: swaps a snapshot and answers the hot path from it.
#[derive(Debug)]
pub struct QuotaGuard {
    snapshot: ArcSwap<QuotaSnapshot>,
    tenant: String,
    max_age: Duration,
}

impl QuotaGuard {
    /// A guard holding `snapshot`, treating it as stale after `max_age`.
    #[must_use]
    pub fn new(tenant: impl Into<String>, snapshot: QuotaSnapshot, max_age: Duration) -> Arc<Self> {
        Arc::new(Self {
            snapshot: ArcSwap::from_pointee(snapshot),
            tenant: tenant.into(),
            max_age,
        })
    }

    /// The current snapshot, cheap (one atomic load, no lock).
    #[must_use]
    pub fn snapshot(&self) -> Arc<QuotaSnapshot> {
        self.snapshot.load_full()
    }

    /// The staleness of the current snapshot evaluated at `now`.
    #[must_use]
    pub fn staleness(&self, now: i64) -> Staleness {
        let built_at = self.snapshot.load().built_at;
        let age = now.saturating_sub(built_at).max(0);
        let max_age = i64::try_from(self.max_age.as_secs()).unwrap_or(i64::MAX);
        Staleness {
            age,
            is_stale: age > max_age,
            max_age,
        }
    }

    /// The hot path: `Allow`, or `Deny` with a stable reason.
    ///
    /// Reads the snapshot without a lock. If the snapshot is stale, a `HardBlock`
    /// meter fails closed ([`reasons::QUOTA_SNAPSHOT_STALE`]) and every other mode
    /// fails open, exactly as the module docs describe.
    #[must_use]
    pub fn allow(&self, action: QuotaAction, tenant: &str, now: i64) -> ActionDecision {
        let snapshot = self.snapshot.load();
        if tenant != self.tenant {
            return ActionDecision::Deny {
                reason: reasons::TENANT_MISMATCH,
            };
        }
        let age = now.saturating_sub(snapshot.built_at);
        let stale = age > i64::try_from(self.max_age.as_secs()).unwrap_or(i64::MAX)
            || now < snapshot.built_at
            || !snapshot.report.period.contains(now);

        // The licence's own decision governs first, at every mode and staleness.
        if let spot_license::Decision::Deny { reason } = snapshot
            .report
            .licence
            .allows(action.licence_action_public())
        {
            return ActionDecision::Deny { reason };
        }
        if !action.is_quota_gated() {
            return ActionDecision::Allow;
        }

        for (meter, decision) in &snapshot.report.decisions {
            if !QuotaAction::gates(action, meter) {
                continue;
            }
            if stale {
                // Unlimited meters never fail closed, regardless of their configured mode.
                if snapshot.mode_of(meter) == Some(QuotaMode::HardBlock)
                    && !matches!(decision, QuotaDecision::Ok { limit: None, .. })
                {
                    return ActionDecision::Deny {
                        reason: reasons::QUOTA_SNAPSHOT_STALE,
                    };
                }
                continue;
            }
            match decision {
                QuotaDecision::Blocked { reason, .. } => return ActionDecision::Deny { reason },
                QuotaDecision::OverSoft { grace_until, .. } if now >= *grace_until => {
                    return ActionDecision::Deny {
                        reason: crate::decision::reasons::QUOTA_SOFT_EXCEEDED,
                    };
                }
                _ => {}
            }
        }

        ActionDecision::Allow
    }

    /// Rebuilds the snapshot from `store` for `period`, swapping it in atomically.
    ///
    /// # Errors
    /// Any store error.
    pub async fn refresh(
        &self,
        store: &dyn MeterStore,
        context: RefreshContext<'_>,
    ) -> Result<(), crate::error::MeteringError> {
        let RefreshContext {
            tenant,
            period,
            resolver,
            policy,
            licence,
            unknown_limits,
            dropped,
            now,
        } = context;
        if tenant != self.tenant {
            return Err(crate::error::MeteringError::TenantMismatch);
        }
        policy.validate()?;
        let usage = store.period_totals(tenant, period, resolver).await?;
        let report = crate::decision::evaluate(
            policy,
            &usage,
            licence,
            period.clone(),
            now,
            unknown_limits,
            dropped,
        );
        let mut modes = std::collections::BTreeMap::new();
        for meter in policy.meters() {
            if let Some(rule) = policy.rule_for(&meter) {
                modes.insert(meter, rule.mode);
            }
        }
        self.snapshot.rcu(|previous| {
            if previous.built_at > now {
                return previous.clone();
            }
            let mut report = report.clone();
            if previous.report.period == *period {
                for (meter, decision) in &mut report.decisions {
                    if let QuotaDecision::OverSoft {
                        grace_until,
                        used,
                        limit,
                    } = decision
                    {
                        match previous.report.decisions.get(meter) {
                            Some(QuotaDecision::OverSoft {
                                grace_until: old, ..
                            }) => *grace_until = *old,
                            Some(QuotaDecision::Blocked { reason, .. })
                                if *reason == crate::decision::reasons::QUOTA_SOFT_EXCEEDED =>
                            {
                                *grace_until = now
                            }
                            _ => {}
                        }
                        if now >= *grace_until {
                            *decision = QuotaDecision::Blocked {
                                used: *used,
                                limit: *limit,
                                reason: crate::decision::reasons::QUOTA_SOFT_EXCEEDED,
                            };
                        }
                    }
                }
            }
            Arc::new(QuotaSnapshot::new(report, now, modes.clone()))
        });
        Ok(())
    }

    /// The instant of the last refresh, UTC seconds.
    #[must_use]
    pub fn last_refresh(&self) -> i64 {
        self.snapshot.load().built_at
    }
}

/// Re-exported refusal code for callers that only name the stale case.
pub use crate::decision::reasons as quota_reasons;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::period::{PeriodSpec, period_of};
    use crate::policy::{QuotaPolicy, QuotaRule};
    use crate::totals::{PeriodTotals, TenantPeriodUsage};
    use spot_license::{Limit, Status};

    fn a_period() -> PeriodKey {
        let spec = PeriodSpec::calendar_month("UTC").unwrap();
        period_of(0, &spec).unwrap()
    }

    fn guard_with(mode: QuotaMode, built_at: i64, max_age: Duration) -> Arc<QuotaGuard> {
        let rule = QuotaRule {
            limit: Limit::UpTo(0).into(),
            warn_at_bps: 0,
            soft_tolerance_bps: 0,
            mode,
        };
        let policy = QuotaPolicy::new().with_rule(Meter::Transactions, rule);
        let usage = TenantPeriodUsage::default().with_totals(
            Meter::Transactions,
            PeriodTotals {
                sum: 5,
                ..PeriodTotals::default()
            },
        );
        // limit 0, used 5 -> blocked under any blocking mode.
        let report = crate::decision::evaluate(
            &policy,
            &usage,
            Status::Active,
            a_period(),
            built_at,
            Vec::new(),
            0,
        );
        let mut modes = std::collections::BTreeMap::new();
        modes.insert(Meter::Transactions, mode);
        QuotaGuard::new("acme", QuotaSnapshot::new(report, built_at, modes), max_age)
    }

    #[test]
    fn a_fresh_snapshot_denies_a_blocked_meter() {
        let guard = guard_with(QuotaMode::HardBlock, 1_000, Duration::from_secs(60));
        assert_eq!(
            guard.allow(QuotaAction::CreateMeteredDocument, "acme", 1_010),
            ActionDecision::Deny {
                reason: "metering.quota_exceeded"
            }
        );
        assert!(!guard.staleness(1_010).is_stale);
    }

    #[test]
    fn a_stale_hard_block_fails_closed_even_without_a_breach() {
        // A hard-block meter with no breach in the snapshot: staleness alone denies.
        let policy = QuotaPolicy::new().with_rule(
            Meter::Transactions,
            QuotaRule::new(Limit::UpTo(1_000_000)).hard_blocking(),
        );
        let report = crate::decision::evaluate(
            &policy,
            &TenantPeriodUsage::default(),
            Status::Active,
            a_period(),
            1_000,
            Vec::new(),
            0,
        );
        let mut modes = std::collections::BTreeMap::new();
        modes.insert(Meter::Transactions, QuotaMode::HardBlock);
        let guard = QuotaGuard::new(
            "acme",
            QuotaSnapshot::new(report, 1_000, modes),
            Duration::from_secs(60),
        );
        let decision = guard.allow(QuotaAction::CreateMeteredDocument, "acme", 1_000_000);
        assert_eq!(
            decision,
            ActionDecision::Deny {
                reason: "metering.quota_snapshot_stale"
            }
        );
        assert!(guard.staleness(1_000_000).is_stale);
    }

    #[test]
    fn a_stale_warn_only_snapshot_fails_open() {
        // A blocked-looking warn-only meter: staleness makes it advisory, not binding.
        let guard = guard_with(QuotaMode::WarnOnly, 1_000, Duration::from_secs(60));
        assert_eq!(
            guard.allow(QuotaAction::CreateMeteredDocument, "acme", 1_000_000),
            ActionDecision::Allow
        );
    }

    #[test]
    fn a_stale_snapshot_never_blocks_reading_or_export() {
        let guard = guard_with(QuotaMode::HardBlock, 1_000, Duration::from_secs(1));
        for action in [
            QuotaAction::Read,
            QuotaAction::ExportOwnData,
            QuotaAction::SignInExistingUser,
            QuotaAction::PostReversal,
            QuotaAction::CancelDocument,
        ] {
            assert_eq!(
                guard.allow(action, "acme", 1_000_000),
                ActionDecision::Allow,
                "{action:?} must never be blocked by a quota, stale or not"
            );
        }
    }

    #[test]
    fn the_licence_denies_before_the_quota_at_any_staleness() {
        let policy = QuotaPolicy::new();
        let report = crate::decision::evaluate(
            &policy,
            &TenantPeriodUsage::default(),
            Status::Expired,
            a_period(),
            1_000,
            Vec::new(),
            0,
        );
        let guard = QuotaGuard::new(
            "acme",
            QuotaSnapshot::new(report, 1_000, std::collections::BTreeMap::new()),
            Duration::from_secs(1),
        );
        assert_eq!(
            guard.allow(QuotaAction::Read, "acme", 1_000_000),
            ActionDecision::Deny {
                reason: "licence.expired_trial"
            }
        );
    }

    /// A store that is always down, as a failed database would be.
    struct DownStore;

    impl MeterStore for DownStore {
        fn tenant(&self) -> &str {
            "acme"
        }

        fn add_events<'a>(
            &'a self,
            _: Vec<crate::event::MeterEvent>,
            _: &'a dyn PeriodResolver,
        ) -> crate::store::BoxFuture<'a, Result<usize, crate::error::MeteringError>> {
            Box::pin(async { Err(down()) })
        }
        fn period_totals<'a>(
            &'a self,
            _: &'a str,
            _: &'a PeriodKey,
            _: &'a dyn PeriodResolver,
        ) -> crate::store::BoxFuture<'a, Result<TenantPeriodUsage, crate::error::MeteringError>>
        {
            Box::pin(async { Err(down()) })
        }
        fn distinct_members<'a>(
            &'a self,
            _: &'a str,
            _: &'a PeriodKey,
            _: &'a Meter,
            _: &'a dyn PeriodResolver,
        ) -> crate::store::BoxFuture<
            'a,
            Result<Vec<crate::meter::MemberDigest>, crate::error::MeteringError>,
        > {
            Box::pin(async { Err(down()) })
        }
        fn tombstone_count<'a>(
            &'a self,
        ) -> crate::store::BoxFuture<'a, Result<u64, crate::error::MeteringError>> {
            Box::pin(async { Err(down()) })
        }
        fn compact<'a>(
            &'a self,
            _: &'a PeriodKey,
            _: &'a crate::retention::RetentionPolicy,
            _: &'a dyn PeriodResolver,
        ) -> crate::store::BoxFuture<
            'a,
            Result<crate::store::CompactionReport, crate::error::MeteringError>,
        > {
            Box::pin(async { Err(down()) })
        }
    }

    fn down() -> crate::error::MeteringError {
        crate::error::MeteringError::StoreUnavailable {
            cause: "connection_lost",
        }
    }

    #[tokio::test]
    async fn a_store_outage_keeps_the_snapshot_and_follows_the_staleness_rule() {
        let policy = QuotaPolicy::new().with_rule(
            Meter::Transactions,
            QuotaRule::new(Limit::UpTo(1_000_000)).hard_blocking(),
        );
        let report = crate::decision::evaluate(
            &policy,
            &TenantPeriodUsage::default(),
            Status::Active,
            a_period(),
            1_000,
            Vec::new(),
            0,
        );
        let mut modes = std::collections::BTreeMap::new();
        modes.insert(Meter::Transactions, QuotaMode::HardBlock);
        let guard = QuotaGuard::new(
            "acme",
            QuotaSnapshot::new(report, 1_000, modes),
            Duration::from_secs(60),
        );
        let resolver =
            crate::period::TenantPeriods::new(PeriodSpec::calendar_month("UTC").unwrap());
        let period = a_period();
        let error = guard
            .refresh(
                &DownStore,
                RefreshContext {
                    tenant: "acme",
                    period: &period,
                    resolver: &resolver,
                    policy: &policy,
                    licence: Status::Active,
                    unknown_limits: Vec::new(),
                    dropped: 0,
                    now: 1_030,
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            crate::error::MeteringError::StoreUnavailable { .. }
        ));
        // The failed refresh swapped nothing in.
        assert_eq!(guard.last_refresh(), 1_000);
        // Fresh: still served from the old snapshot. Stale: a hard quota fails closed.
        assert_eq!(
            guard.allow(QuotaAction::CreateMeteredDocument, "acme", 1_030),
            ActionDecision::Allow
        );
        assert_eq!(
            guard.allow(QuotaAction::CreateMeteredDocument, "acme", 1_000_000),
            ActionDecision::Deny {
                reason: "metering.quota_snapshot_stale"
            }
        );
        // Read-only paths are never blocked, outage or not.
        assert_eq!(
            guard.allow(QuotaAction::Read, "acme", 1_000_000),
            ActionDecision::Allow
        );
    }
}
