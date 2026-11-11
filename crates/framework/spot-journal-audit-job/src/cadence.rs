//! How often a tenant is audited, and whether it is due.

use spot_journal_store::TenantId;

/// One day in milliseconds.
pub const DAY_MILLIS: i64 = 24 * 60 * 60 * 1000;

/// How often a tenant's journal is audited (ADR-061 s8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cadence {
    /// Every seven days: the default.
    Weekly,
    /// Every day: tenants whose migration replay policy is `always`. The directory says which
    /// tenants these are; the job does not read the policy itself.
    Daily,
}

impl Cadence {
    /// The interval between two audits, in milliseconds.
    #[must_use]
    pub fn interval_millis(self) -> i64 {
        match self {
            Self::Weekly => 7 * DAY_MILLIS,
            Self::Daily => DAY_MILLIS,
        }
    }
}

/// What the directory knows about one tenant's audit schedule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TenantAuditPlan {
    /// The tenant.
    pub tenant: TenantId,
    /// How often it is audited.
    pub cadence: Cadence,
    /// When the last audit of the tenant started, in unix milliseconds; `None` when it was never
    /// audited. The result sink records the start of the run (see
    /// [`crate::TenantResult::started_millis`]), so a daily schedule that fires at the same
    /// minute every day is never one run short because a run took a few minutes.
    pub last_run_millis: Option<i64>,
}

/// Whether the tenant is due at `now_millis`: never audited, or the interval has elapsed.
///
/// A clock that went backwards is never due (the elapsed time saturates at zero).
#[must_use]
pub fn due(plan: &TenantAuditPlan, now_millis: i64) -> bool {
    match plan.last_run_millis {
        None => true,
        Some(last) => now_millis.saturating_sub(last) >= plan.cadence.interval_millis(),
    }
}
