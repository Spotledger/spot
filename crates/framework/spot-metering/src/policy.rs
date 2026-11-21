//! Quota policies: which limits apply to which meters, and how a breach behaves.
//!
//! A [`QuotaPolicy`] is built from a licence's [`Entitlements`] plus a tenant's own
//! policy (an app may tighten a limit the licence left unlimited, or add a meter the
//! licence never mentioned). Fractions are integer basis points — no floats anywhere
//! (workspace lint) — and the defaults are the acceptance ones: warn at 80%, tolerate
//! 10% over, and `WarnOnly` unless the tenant says otherwise.

use std::collections::BTreeMap;

use spot_license::model::ExtraEntitlement;
use spot_license::{Entitlements, Limit};

use crate::error::MeteringError;
use crate::meter::Meter;

/// The default warn fraction in basis points: 80%.
pub const QUOTA_WARN_FRACTION_BPS: u64 = 8_000;

/// The default soft over-limit tolerance in basis points: 10%.
pub const QUOTA_SOFT_TOLERANCE_BPS: u64 = 1_000;

/// What happens when a meter is over its limit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum QuotaMode {
    /// Report the breach, block nothing (the default).
    #[default]
    WarnOnly,
    /// Past the soft tolerance, block creating new users or metered documents until
    /// the grace window closes.
    SoftBlock,
    /// At the limit, block creating new users or metered documents immediately.
    HardBlock,
}

/// Quota bounds retain the full u64 width of licence extra limits (storage bytes
/// routinely exceed the typed users limit's u32 range).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MeterLimit {
    /// Inclusive usage ceiling.
    UpTo(u64),
    /// No bound.
    Unlimited,
}
impl From<Limit> for MeterLimit {
    fn from(limit: Limit) -> Self {
        match limit {
            Limit::UpTo(n) => Self::UpTo(u64::from(n)),
            Limit::Unlimited => Self::Unlimited,
        }
    }
}

/// One meter's rule: a limit, a warn fraction, a soft tolerance and a mode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuotaRule {
    /// The licensed limit, [`Limit::Unlimited`] when the licence set none.
    pub limit: MeterLimit,
    /// The fraction of `limit` at which usage warns, in basis points (0..=10,000).
    pub warn_at_bps: u64,
    /// How far over `limit` a `SoftBlock` tolerates before it blocks, in basis points.
    pub soft_tolerance_bps: u64,
    /// What the breach does.
    pub mode: QuotaMode,
}

impl QuotaRule {
    /// A rule with the acceptance defaults: warn at 80%, tolerate 10%, `WarnOnly`.
    #[must_use]
    pub fn new(limit: impl Into<MeterLimit>) -> Self {
        Self {
            limit: limit.into(),
            warn_at_bps: QUOTA_WARN_FRACTION_BPS,
            soft_tolerance_bps: QUOTA_SOFT_TOLERANCE_BPS,
            mode: QuotaMode::WarnOnly,
        }
    }

    /// A rule that blocks new documents once the soft tolerance is passed; builders
    /// keep chaining.
    #[must_use]
    pub fn soft_blocking(mut self) -> Self {
        self.mode = QuotaMode::SoftBlock;
        self
    }

    /// A rule that blocks new documents at the limit; builders keep chaining.
    #[must_use]
    pub fn hard_blocking(mut self) -> Self {
        self.mode = QuotaMode::HardBlock;
        self
    }

    /// Overrides the warn fraction; builders keep chaining.
    #[must_use]
    pub fn warn_at(mut self, bps: u64) -> Self {
        self.warn_at_bps = bps;
        self
    }

    /// Overrides the soft tolerance; builders keep chaining.
    #[must_use]
    pub fn over_limit_tolerance(mut self, bps: u64) -> Self {
        self.soft_tolerance_bps = bps;
        self
    }

    /// The usage at which this rule warns: `limit * warn_at_bps / 10_000`, in exact
    /// integer arithmetic (rounding down, so the warning never fires early).
    #[must_use]
    pub fn warn_level(&self) -> Option<u64> {
        match self.limit {
            MeterLimit::Unlimited => None,
            MeterLimit::UpTo(bound) => Some(
                ((u128::from(bound) * u128::from(self.warn_at_bps.min(10_000))) / 10_000) as u64,
            ),
        }
    }

    /// The usage at which a `SoftBlock` blocks: `limit * (10_000 + tolerance) / 10_000`,
    /// exact integer arithmetic.
    #[must_use]
    pub fn soft_block_level(&self) -> Option<u64> {
        match self.limit {
            MeterLimit::Unlimited => None,
            MeterLimit::UpTo(bound) => {
                let factor = 10_000u64.saturating_add(self.soft_tolerance_bps);
                Some(
                    u64::try_from(u128::from(bound) * u128::from(factor) / 10_000)
                        .unwrap_or(u64::MAX),
                )
            }
        }
    }
}

/// The extra-entitlement names this crate maps to meters, and their meter names.
///
/// A licence can carry extra limits an app declared (`max_documents`-style names).
/// The declared table here is the whole mapping; a name not in it is **ignored with a
/// warning** rather than guessed at, because an unrecognised limit has no meter whose
/// usage could be compared to it.
pub const EXTRA_LIMIT_TABLE: &[(&str, &str)] = &[
    ("max_documents", "transactions"),
    ("max_transactions", "transactions"),
    ("max_storage_bytes", "storage_bytes"),
    ("max_ai_tokens", "ai_tokens"),
    ("max_active_users", "active_users"),
    ("max_users", "active_users"),
];

/// A tenant's quota policy: one [`QuotaRule`] per meter, plus the deadline a
/// `SoftBlock` gives the tenant to act.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuotaPolicy {
    rules: BTreeMap<Meter, QuotaRule>,
    grace: std::time::Duration,
}

impl QuotaPolicy {
    /// An empty policy: every meter unlimited, `WarnOnly`, with a 24-hour soft-block
    /// grace deadline.
    #[must_use]
    pub fn new() -> Self {
        Self {
            rules: BTreeMap::new(),
            grace: std::time::Duration::from_secs(24 * 60 * 60),
        }
    }

    /// Builds a policy from a licence's entitlements.
    ///
    /// The typed `users` limit becomes the [`Meter::ActiveUsers`] rule; the built-in
    /// meters the licence names nothing for stay unlimited; every extra entitlement
    /// whose name is in [`EXTRA_LIMIT_TABLE`] becomes a rule for the meter it names.
    /// Unknown extra entitlement names are collected and returned so the caller can
    /// log them — the crate itself never guesses.
    #[must_use]
    pub fn from_entitlements(entitlements: &Entitlements) -> (Self, Vec<String>) {
        let mut policy = Self::new();
        policy
            .rules
            .insert(Meter::ActiveUsers, QuotaRule::new(entitlements.users()));
        let mut unknown = Vec::new();
        for (name, value) in &entitlements.extra {
            match EXTRA_LIMIT_TABLE.iter().find(|(key, _)| key == name) {
                Some((_, meter)) => {
                    let limit = extra_to_limit(value);
                    let key = meter_from_name(meter);
                    policy
                        .rules
                        .entry(key)
                        .and_modify(|rule| {
                            rule.limit = tighter(rule.limit, limit);
                        })
                        .or_insert_with(|| QuotaRule::new(limit));
                }
                None => unknown.push(name.clone()),
            }
        }
        (policy, unknown)
    }

    /// Combines licence limits with tenant rules. Tenant fractions and modes apply,
    /// but a tenant cannot raise a licensed bound. Unknown extras remain visible.
    #[must_use]
    pub fn from_entitlements_with_policy(
        entitlements: &Entitlements,
        tenant: &Self,
    ) -> (Self, Vec<String>) {
        let (mut combined, unknown) = Self::from_entitlements(entitlements);
        for (meter, rule) in &tenant.rules {
            let mut rule = rule.clone();
            if let Some(licensed) = combined.rules.get(meter) {
                rule.limit = tighter(rule.limit, licensed.limit);
            }
            combined.rules.insert(meter.clone(), rule);
        }
        combined.grace = tenant.grace;
        (combined, unknown)
    }

    /// Sets one meter's rule; builders keep chaining.
    #[must_use]
    pub fn with_rule(mut self, meter: Meter, rule: QuotaRule) -> Self {
        self.rules.insert(meter, rule);
        self
    }

    /// Sets the soft-block grace window; builders keep chaining.
    #[must_use]
    pub fn with_grace(mut self, grace: std::time::Duration) -> Self {
        self.grace = grace;
        self
    }

    /// The grace window a `SoftBlock` grants.
    #[must_use]
    pub fn grace(&self) -> std::time::Duration {
        self.grace
    }

    /// The rule for one meter, if the policy has one (an absent rule means unlimited).
    #[must_use]
    pub fn rule_for(&self, meter: &Meter) -> Option<&QuotaRule> {
        self.rules.get(meter)
    }

    /// Every meter the policy names a rule for, sorted.
    #[must_use]
    pub fn meters(&self) -> Vec<Meter> {
        self.rules.keys().cloned().collect()
    }

    /// Validates every fraction is in 0..=10,000 bps.
    ///
    /// # Errors
    /// [`MeteringError::BasisPointsOutOfRange`] for the first offending rule.
    pub fn validate(&self) -> Result<(), MeteringError> {
        for rule in self.rules.values() {
            for bps in [rule.warn_at_bps, rule.soft_tolerance_bps] {
                if bps > 10_000 {
                    return Err(MeteringError::BasisPointsOutOfRange { bps });
                }
            }
        }
        Ok(())
    }
}

impl Default for QuotaPolicy {
    fn default() -> Self {
        Self::new()
    }
}

/// The [`Limit`] an extra entitlement expresses: a `Number` is `UpTo` (0 means the
/// issuer granted nothing, so the meter blocks at once), `Unlimited` otherwise.
///
/// A `Flag`/`Name` extra that landed in the table would be a table bug; the mapping
/// treats anything non-numeric as unlimited rather than inventing a bound.
fn tighter(a: MeterLimit, b: MeterLimit) -> MeterLimit {
    match (a, b) {
        (MeterLimit::UpTo(a), MeterLimit::UpTo(b)) => MeterLimit::UpTo(a.min(b)),
        (MeterLimit::Unlimited, b) => b,
        (a, MeterLimit::Unlimited) => a,
    }
}

fn extra_to_limit(value: &ExtraEntitlement) -> MeterLimit {
    match value {
        ExtraEntitlement::Number(count) => MeterLimit::UpTo(*count),
        ExtraEntitlement::Name(name) if name == "unlimited" => MeterLimit::Unlimited,
        _ => MeterLimit::Unlimited,
    }
}

fn meter_from_name(name: &str) -> Meter {
    match name {
        "active_users" => Meter::ActiveUsers,
        "transactions" => Meter::Transactions,
        "storage_bytes" => Meter::StorageBytes,
        "ai_tokens" => Meter::AiTokens,
        other => Meter::Custom(other.to_owned()),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_the_acceptance_ones() {
        let rule = QuotaRule::new(Limit::UpTo(100));
        assert_eq!(rule.warn_at_bps, 8_000);
        assert_eq!(rule.soft_tolerance_bps, 1_000);
        assert_eq!(rule.mode, QuotaMode::WarnOnly);
        assert_eq!(rule.warn_level(), Some(80));
        assert_eq!(rule.soft_block_level(), Some(110));
    }

    #[test]
    fn unlimited_never_warns_or_blocks() {
        let rule = QuotaRule::new(Limit::Unlimited).hard_blocking();
        assert_eq!(rule.warn_level(), None);
        assert_eq!(rule.soft_block_level(), None);
    }

    #[test]
    fn entitlements_map_users_and_declared_extras() {
        let mut entitlements = Entitlements::from_lists(Some(Limit::UpTo(10)), vec![], vec![]);
        entitlements
            .extra
            .insert("max_documents".to_owned(), ExtraEntitlement::Number(500));
        entitlements.extra.insert(
            "max_storage_bytes".to_owned(),
            ExtraEntitlement::Number(1_000_000),
        );
        entitlements
            .extra
            .insert("unknown_thing".to_owned(), ExtraEntitlement::Number(7));

        let (policy, unknown) = QuotaPolicy::from_entitlements(&entitlements);
        assert_eq!(
            policy.rule_for(&Meter::ActiveUsers).unwrap().limit,
            MeterLimit::UpTo(10)
        );
        assert_eq!(
            policy.rule_for(&Meter::Transactions).unwrap().limit,
            MeterLimit::UpTo(500)
        );
        assert_eq!(
            policy.rule_for(&Meter::StorageBytes).unwrap().limit,
            MeterLimit::UpTo(1_000_000)
        );
        // An unknown extra limit is ignored, not guessed at.
        assert_eq!(unknown, vec!["unknown_thing".to_owned()]);
        assert!(policy.validate().is_ok());
    }

    #[test]
    fn out_of_range_fractions_are_refused() {
        let policy = QuotaPolicy::new().with_rule(
            Meter::Transactions,
            QuotaRule::new(Limit::UpTo(1)).warn_at(20_000),
        );
        assert_eq!(
            policy.validate().unwrap_err().code(),
            "metering.basis_points_out_of_range"
        );
    }
}
