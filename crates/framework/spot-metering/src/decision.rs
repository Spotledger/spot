//! Quota decisions: comparing usage to limits, and what a breach may and may not stop.
//!
//! [`evaluate`] produces one [`QuotaDecision`] per metered meter and a
//! [`QuotaReport`] that composes with the licence's own state machine
//! ([`spot_license::Status::allows`]) into a [`Decision`] for a concrete action.
//!
//! The action classes are the point of the whole module. A [`QuotaDecision::Blocked`]
//! stops **creating new users and new metered documents** — nothing else. Reading,
//! exporting one's own data, signing an existing user in, and posting a reversal or
//! cancellation are never blocked by a quota: a customer over their limit must always
//! be able to get their data out and fix their mistakes.
//!
//! A licence in its grace period behaves as the licence's own decision says
//! ([`spot_license::Status::Grace`] allows everything, with its warning); an expired
//! licence is not this crate's decision — it reports the usage and defers.

use std::collections::BTreeMap;
use std::time::Duration;

use spot_license::{Decision, Status};

use crate::meter::Meter;
use crate::period::PeriodKey;
use crate::policy::{QuotaMode, QuotaPolicy, QuotaRule};
use crate::totals::TenantPeriodUsage;

/// The stable refusal codes a quota decision carries.
pub mod reasons {
    /// Usage is past the limit and the mode blocks immediately.
    pub const QUOTA_EXCEEDED: &str = "metering.quota_exceeded";
    /// Usage is past the soft tolerance and the soft-block grace has closed.
    pub const QUOTA_SOFT_EXCEEDED: &str = "metering.quota_soft_exceeded";
}

/// The meters a `Blocked` decision may stop, by the action class it affects.
///
/// The set is deliberately short: only creating new users and new metered documents.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum QuotaAction {
    /// Creating a new user — gated by [`Meter::ActiveUsers`].
    CreateUser,
    /// Creating a new metered document — gated by [`Meter::Transactions`] and any
    /// custom counter meter the policy declares.
    CreateMeteredDocument,
    /// Reading existing data: never blocked by a quota.
    Read,
    /// Exporting one's own data: never blocked by a quota.
    ExportOwnData,
    /// Signing in an existing user: never blocked by a quota.
    SignInExistingUser,
    /// Posting a reversal: never blocked by a quota.
    PostReversal,
    /// Cancelling a document: never blocked by a quota.
    CancelDocument,
}

impl QuotaAction {
    /// Every action, for exhaustive tests.
    #[must_use]
    pub fn all() -> [QuotaAction; 7] {
        [
            QuotaAction::CreateUser,
            QuotaAction::CreateMeteredDocument,
            QuotaAction::Read,
            QuotaAction::ExportOwnData,
            QuotaAction::SignInExistingUser,
            QuotaAction::PostReversal,
            QuotaAction::CancelDocument,
        ]
    }

    /// Whether a quota breach may stop this action. Only creating new users and new
    /// metered documents.
    #[must_use]
    pub fn is_quota_gated(self) -> bool {
        matches!(self, Self::CreateUser | Self::CreateMeteredDocument)
    }

    /// The licence action this maps to, for composing with the licence's own state
    /// machine.
    fn licence_action(self) -> spot_license::Action {
        match self {
            Self::CreateUser | Self::CreateMeteredDocument => spot_license::Action::CreateDocument,
            Self::Read | Self::SignInExistingUser | Self::PostReversal | Self::CancelDocument => {
                spot_license::Action::UsePlatform
            }
            Self::ExportOwnData => spot_license::Action::ExportData,
        }
    }
    /// The licence action this maps to, for composing with the licence's own state
    /// machine. Public so the guard can compose without duplicating the mapping.
    #[must_use]
    pub fn licence_action_public(self) -> spot_license::Action {
        self.licence_action()
    }

    /// Whether `meter`'s verdict stops `action`, public for the same reason.
    #[must_use]
    pub fn gates(action: QuotaAction, meter: &Meter) -> bool {
        match action {
            Self::CreateUser => matches!(meter, Meter::ActiveUsers),
            Self::CreateMeteredDocument => {
                matches!(
                    meter,
                    Meter::Transactions | Meter::StorageBytes | Meter::AiTokens | Meter::Custom(_)
                )
            }
            Self::Read
            | Self::ExportOwnData
            | Self::SignInExistingUser
            | Self::PostReversal
            | Self::CancelDocument => false,
        }
    }
}

/// This crate's decision for one action: allowed, or refused with a stable code.
///
/// The code is either a `licence.*` code (the licence's own decision) or a
/// `metering.*` one (a quota breach); both are stable and displayable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionDecision {
    /// The action may proceed.
    Allow,
    /// Refused; `reason` is a stable code.
    Deny {
        /// The refusal code.
        reason: &'static str,
    },
}

impl ActionDecision {
    /// Whether the decision allows the action.
    #[must_use]
    pub fn is_allow(self) -> bool {
        matches!(self, Self::Allow)
    }

    /// The refusal code, if any.
    #[must_use]
    pub fn reason(self) -> Option<&'static str> {
        match self {
            Self::Allow => None,
            Self::Deny { reason } => Some(reason),
        }
    }
}

impl From<Decision> for ActionDecision {
    fn from(decision: Decision) -> Self {
        match decision {
            Decision::Allow => Self::Allow,
            Decision::Deny { reason } => Self::Deny { reason },
        }
    }
}

/// The verdict for one meter in one period.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuotaDecision {
    /// Under the warn level (or the meter is unlimited).
    Ok {
        /// The usage observed.
        used: u64,
        /// The limit, `None` when unlimited.
        limit: Option<u64>,
    },
    /// At or past the warn level, still within the limit.
    Warn {
        /// The usage observed.
        used: u64,
        /// The limit.
        limit: u64,
    },
    /// Past the limit within the soft tolerance: a `SoftBlock` opens a grace window
    /// (`grace_until`) and blocks only once it closes.
    OverSoft {
        /// The usage observed.
        used: u64,
        /// The limit.
        limit: u64,
        /// The instant the soft-block grace closes, UTC seconds; the action continues
        /// until then.
        grace_until: i64,
    },
    /// Blocked: creating new users or metered documents stops. Reading, exporting,
    /// signing in and reversals never do.
    Blocked {
        /// The usage observed.
        used: u64,
        /// The limit.
        limit: u64,
        /// The stable `metering.*` refusal code.
        reason: &'static str,
    },
}

impl QuotaDecision {
    /// The usage this decision observed.
    #[must_use]
    pub fn used(&self) -> u64 {
        match *self {
            Self::Ok { used, .. }
            | Self::Warn { used, .. }
            | Self::OverSoft { used, .. }
            | Self::Blocked { used, .. } => used,
        }
    }

    /// Whether this decision blocks new users or metered documents.
    #[must_use]
    pub fn is_blocked(&self) -> bool {
        matches!(self, Self::Blocked { .. })
    }

    /// Whether this decision is anything other than `Ok` (a warning or worse).
    #[must_use]
    pub fn is_breach(&self) -> bool {
        !matches!(self, Self::Ok { .. })
    }

    /// The refusal code, for the breached states.
    #[must_use]
    pub fn reason(&self) -> Option<&'static str> {
        match self {
            Self::Blocked { reason, .. } => Some(reason),
            Self::Ok { .. } | Self::Warn { .. } | Self::OverSoft { .. } => None,
        }
    }
}

/// Every meter's verdict for one tenant-period, with the licence state it was
/// evaluated against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuotaReport {
    /// The period the usage was read for.
    pub period: PeriodKey,
    /// One verdict per meter the policy or the usage names.
    pub decisions: BTreeMap<Meter, QuotaDecision>,
    /// The licence state the verdicts compose with.
    pub licence: Status,
    /// Extra entitlement names the licence carried that no meter maps to (ignored).
    pub unknown_limits: Vec<String>,
    /// How many events were dropped (sink overflow) since the last reconciliation;
    /// never silently lost, always surfaced here.
    pub dropped: u64,
}

impl QuotaReport {
    /// The verdict for one meter, `None` when neither the policy nor the usage names
    /// it (an untracked meter is unconstrained, not `Ok` by accident).
    #[must_use]
    pub fn decision_for(&self, meter: &Meter) -> Option<&QuotaDecision> {
        self.decisions.get(meter)
    }

    /// Whether any meter is blocked.
    #[must_use]
    pub fn any_blocked(&self) -> bool {
        self.decisions.values().any(QuotaDecision::is_blocked)
    }

    /// The decided action: the licence's own decision first (an expired licence is
    /// not this crate's decision), then the quota gate for the actions a breach may
    /// stop.
    ///
    /// Reading, exporting one's own data, signing an existing user in and reversals
    /// are never blocked by a quota; they still respect the licence's own state.
    #[must_use]
    pub fn allows(&self, action: QuotaAction) -> ActionDecision {
        // The licence's decision governs first: an expired licence denies everything
        // it denies, and this crate only reports usage.
        if let Decision::Deny { reason } = self.licence.allows(action.licence_action()) {
            return ActionDecision::Deny { reason };
        }
        if !action.is_quota_gated() {
            return ActionDecision::Allow;
        }
        for (meter, decision) in &self.decisions {
            if QuotaAction::gates(action, meter)
                && let QuotaDecision::Blocked { reason, .. } = decision
            {
                return ActionDecision::Deny { reason };
            }
        }
        ActionDecision::Allow
    }

    /// The meters whose `Blocked` verdict stops `action`; public so the guard's hot
    /// path composes without duplicating the gate table.
    #[must_use]
    pub fn gating_meters(&self, action: QuotaAction) -> Vec<Meter> {
        self.decisions
            .keys()
            .filter(|meter| QuotaAction::gates(action, meter))
            .cloned()
            .collect()
    }
}

/// The soft-block grace deadline for a breach observed at `now`.
#[must_use]
pub fn grace_until_for(now: i64, grace: Duration) -> i64 {
    now.saturating_add(i64::try_from(grace.as_secs()).unwrap_or(i64::MAX))
}

/// Whether `used` is past a rule's soft tolerance but not its hard limit.
#[must_use]
pub fn over_soft_limit(used: u64, rule: &QuotaRule) -> bool {
    match (rule.limit, rule.soft_block_level()) {
        (crate::policy::MeterLimit::Unlimited, _) | (_, None) => false,
        (crate::policy::MeterLimit::UpTo(_), Some(level)) => used > level,
    }
}

/// The verdict for one meter: usage against a rule, with `now` fixing any grace
/// deadline.
#[must_use]
pub fn verdict_for(used: u64, rule: &QuotaRule, now: i64, grace: Duration) -> QuotaDecision {
    let Some(limit) = (match rule.limit {
        crate::policy::MeterLimit::Unlimited => None,
        crate::policy::MeterLimit::UpTo(bound) => Some(bound),
    }) else {
        // `Limit::Unlimited`: no warn level and no block level; never a breach.
        return QuotaDecision::Ok { used, limit: None };
    };

    // Past the hard limit.
    if used > limit {
        return match rule.mode {
            // Warn-only and soft-block modes still tolerate over-limit usage inside
            // the tolerance window.
            QuotaMode::WarnOnly => QuotaDecision::Warn { used, limit },
            QuotaMode::SoftBlock => {
                let soft = rule.soft_block_level().unwrap_or(limit);
                if used <= soft {
                    QuotaDecision::OverSoft {
                        used,
                        limit,
                        grace_until: grace_until_for(now, grace),
                    }
                } else {
                    QuotaDecision::Blocked {
                        used,
                        limit,
                        reason: reasons::QUOTA_SOFT_EXCEEDED,
                    }
                }
            }
            QuotaMode::HardBlock => QuotaDecision::Blocked {
                used,
                limit,
                reason: reasons::QUOTA_EXCEEDED,
            },
        };
    }

    // At the limit but not over.
    if used == limit {
        return match rule.mode {
            QuotaMode::WarnOnly => QuotaDecision::Warn { used, limit },
            QuotaMode::SoftBlock => QuotaDecision::OverSoft {
                used,
                limit,
                grace_until: grace_until_for(now, grace),
            },
            QuotaMode::HardBlock => QuotaDecision::Blocked {
                used,
                limit,
                reason: reasons::QUOTA_EXCEEDED,
            },
        };
    }

    // Below the limit: warn at the warn level.
    if rule.warn_level().is_some_and(|level| used >= level) {
        return QuotaDecision::Warn { used, limit };
    }
    QuotaDecision::Ok {
        used,
        limit: Some(limit),
    }
}

/// Evaluates every metered meter against `policy`, composing with `licence`.
///
/// `usage` is the store's totals for the period; `licence` is the current
/// [`Status`] (a grace-period licence behaves as its own decision says; an expired
/// licence is reported, not judged here). `now` fixes soft-block grace deadlines.
#[must_use]
pub fn evaluate(
    policy: &QuotaPolicy,
    usage: &TenantPeriodUsage,
    licence: Status,
    period: PeriodKey,
    now: i64,
    unknown_limits: Vec<String>,
    dropped: u64,
) -> QuotaReport {
    let mut decisions: BTreeMap<Meter, QuotaDecision> = BTreeMap::new();

    // Every meter the policy names, whether or not it has usage yet.
    for meter in policy.meters() {
        let used = usage.total(&meter);
        if let Some(rule) = policy.rule_for(&meter) {
            decisions.insert(meter, verdict_for(used, rule, now, policy.grace()));
        }
    }
    // Every meter the usage names but the policy does not: unconstrained (unlimited).
    for meter in usage.by_meter.keys() {
        if decisions.contains_key(meter) {
            continue;
        }
        decisions.insert(
            meter.clone(),
            QuotaDecision::Ok {
                used: usage.total(meter),
                limit: None,
            },
        );
    }

    QuotaReport {
        period,
        decisions,
        licence,
        unknown_limits,
        dropped,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::meter::MemberDigest;
    use crate::period::{PeriodSpec, period_of};
    use crate::policy::QuotaRule;
    use crate::totals::PeriodTotals;
    use spot_license::Limit;

    fn a_period() -> PeriodKey {
        let spec = PeriodSpec::calendar_month("UTC").unwrap();
        period_of(0, &spec).unwrap()
    }

    fn usage_with(meter: Meter, total: u64) -> TenantPeriodUsage {
        let totals = match meter.kind() {
            crate::meter::MeterKind::Counter => PeriodTotals {
                sum: total,
                ..PeriodTotals::default()
            },
            crate::meter::MeterKind::Gauge => PeriodTotals {
                max: total,
                ..PeriodTotals::default()
            },
            crate::meter::MeterKind::DistinctSet => PeriodTotals {
                distinct: total,
                ..PeriodTotals::default()
            },
        };
        TenantPeriodUsage::default().with_totals(meter, totals)
    }

    fn report(used_tx: u64, rule: QuotaRule) -> QuotaReport {
        let policy = QuotaPolicy::new().with_rule(Meter::Transactions, rule);
        evaluate(
            &policy,
            &usage_with(Meter::Transactions, used_tx),
            Status::Active,
            a_period(),
            1_000_000,
            Vec::new(),
            0,
        )
    }

    #[test]
    fn ok_below_warn() {
        let report = report(50, QuotaRule::new(Limit::UpTo(100)).hard_blocking());
        assert_eq!(
            report.decision_for(&Meter::Transactions),
            Some(&QuotaDecision::Ok {
                used: 50,
                limit: Some(100)
            })
        );
        assert!(!report.any_blocked());
    }

    #[test]
    fn warn_at_the_warn_level() {
        let report = report(80, QuotaRule::new(Limit::UpTo(100)).hard_blocking());
        assert!(matches!(
            report.decision_for(&Meter::Transactions),
            Some(QuotaDecision::Warn {
                used: 80,
                limit: 100
            })
        ));
        assert!(!report.any_blocked());
    }

    #[test]
    fn hard_block_at_the_limit_stops_only_gated_actions() {
        let report = report(100, QuotaRule::new(Limit::UpTo(100)).hard_blocking());
        assert!(report.any_blocked());
        assert_eq!(
            report.allows(QuotaAction::CreateMeteredDocument),
            ActionDecision::Deny {
                reason: "metering.quota_exceeded"
            }
        );
        // Never blocked: reading, exporting, signing in, reversals, cancellations.
        for action in [
            QuotaAction::Read,
            QuotaAction::ExportOwnData,
            QuotaAction::SignInExistingUser,
            QuotaAction::PostReversal,
            QuotaAction::CancelDocument,
        ] {
            assert_eq!(report.allows(action), ActionDecision::Allow, "{action:?}");
        }
    }

    #[test]
    fn soft_block_tolerates_then_blocks() {
        // limit 100, tolerance 10% -> soft level 110.
        let rule = QuotaRule::new(Limit::UpTo(100)).soft_blocking();
        let over = report(105, rule.clone());
        assert!(matches!(
            over.decision_for(&Meter::Transactions),
            Some(QuotaDecision::OverSoft { grace_until, .. }) if *grace_until > 1_000_000
        ));
        // Over the soft tolerance: blocked.
        let blocked = report(111, rule);
        assert!(blocked.any_blocked());
        assert_eq!(
            blocked.allows(QuotaAction::CreateMeteredDocument),
            ActionDecision::Deny {
                reason: "metering.quota_soft_exceeded"
            }
        );
    }

    #[test]
    fn warn_only_never_blocks() {
        let rule = QuotaRule::new(Limit::UpTo(10)); // WarnOnly by default
        let report = report(9_999, rule);
        assert!(!report.any_blocked());
        assert_eq!(
            report.allows(QuotaAction::CreateUser),
            ActionDecision::Allow
        );
        assert_eq!(
            report.allows(QuotaAction::CreateMeteredDocument),
            ActionDecision::Allow
        );
    }

    #[test]
    fn unlimited_never_blocks() {
        let report = report(
            u64::from(u32::MAX),
            QuotaRule::new(Limit::Unlimited).hard_blocking(),
        );
        assert!(!report.any_blocked());
        assert_eq!(
            report.decision_for(&Meter::Transactions),
            Some(&QuotaDecision::Ok {
                used: u64::from(u32::MAX),
                limit: None
            })
        );
    }

    #[test]
    fn create_user_follows_the_active_users_meter() {
        let policy = QuotaPolicy::new().with_rule(
            Meter::ActiveUsers,
            QuotaRule::new(Limit::UpTo(5)).hard_blocking(),
        );
        let usage = usage_with(Meter::ActiveUsers, 5);
        let report = evaluate(
            &policy,
            &usage,
            Status::Active,
            a_period(),
            0,
            Vec::new(),
            0,
        );
        assert_eq!(
            report.allows(QuotaAction::CreateUser),
            ActionDecision::Deny {
                reason: "metering.quota_exceeded"
            }
        );
        // The documents meter is not in the policy, so document creation is free.
        assert_eq!(
            report.allows(QuotaAction::CreateMeteredDocument),
            ActionDecision::Allow
        );
    }

    #[test]
    fn a_restricted_licence_blocks_documents_via_the_licence_decision() {
        let policy = QuotaPolicy::new(); // no quota rules at all
        let report = evaluate(
            &policy,
            &TenantPeriodUsage::default(),
            Status::Restricted,
            a_period(),
            0,
            Vec::new(),
            0,
        );
        // The licence's own decision refuses new documents...
        assert_eq!(
            report.allows(QuotaAction::CreateMeteredDocument),
            ActionDecision::Deny {
                reason: "licence.restricted"
            }
        );
        // ...but export stays allowed.
        assert_eq!(
            report.allows(QuotaAction::ExportOwnData),
            ActionDecision::Allow
        );
    }

    #[test]
    fn an_expired_licence_is_the_licences_decision_not_the_quotas() {
        let policy = QuotaPolicy::new().with_rule(
            Meter::Transactions,
            QuotaRule::new(Limit::UpTo(1)).hard_blocking(),
        );
        // Usage is under the limit, but the licence denies everything.
        let report = evaluate(
            &policy,
            &usage_with(Meter::Transactions, 0),
            Status::Expired,
            a_period(),
            0,
            Vec::new(),
            0,
        );
        assert!(!report.any_blocked(), "quota itself is not the blocker");
        assert_eq!(
            report.allows(QuotaAction::Read),
            ActionDecision::Deny {
                reason: "licence.expired_trial"
            }
        );
    }

    #[test]
    fn a_meter_without_a_rule_is_unconstrained_but_reported() {
        let policy = QuotaPolicy::new();
        let usage = usage_with(Meter::StorageBytes, 42);
        let report = evaluate(
            &policy,
            &usage,
            Status::Active,
            a_period(),
            0,
            Vec::new(),
            0,
        );
        assert_eq!(
            report.decision_for(&Meter::StorageBytes),
            Some(&QuotaDecision::Ok {
                used: 42,
                limit: None
            })
        );
    }

    #[test]
    fn a_member_digest_participates_in_the_distinct_total() {
        let usage = TenantPeriodUsage::default().with_members(
            Meter::ActiveUsers,
            vec![MemberDigest::new("a"), MemberDigest::new("b")],
        );
        assert_eq!(usage.total(&Meter::ActiveUsers), 2);
    }
}
