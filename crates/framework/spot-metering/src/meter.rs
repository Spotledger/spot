//! What is being measured, and how a period total must combine the observations in it.
//!
//! The [`MeterKind`] is the whole reason the meter is an enum rather than a string:
//! "10,000 transactions" is a sum, "storage usage" is the largest reading in the
//! period, and "40 active users" means forty distinct people however often they signed
//! in. A store that summed a gauge or counted a distinct-set's events would report
//! nonsense, so the kind travels with every [`Meter`] and every store total.

use std::fmt;

/// One thing a tenant consumes, as the licence and the quotas talk about it.
///
/// The four built-ins are the acceptance set; [`Meter::Custom`] carries a name an app
/// declares (a later task maps them through the quota table) without this crate
/// needing a release.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Meter {
    /// Distinct users seen active in the period ([`MeterKind::DistinctSet`]).
    ActiveUsers,
    /// Posted documents of metered DocTypes — not reads ([`MeterKind::Counter`]).
    Transactions,
    /// Storage bytes: the largest reading observed in the period
    /// ([`MeterKind::Gauge`]), never the sum of readings.
    StorageBytes,
    /// AI tokens settled in the period, summed ([`MeterKind::Counter`]).
    AiTokens,
    /// An app-declared named counter.
    Custom(String),
}

impl Meter {
    /// The built-in meters, in a stable order, for exhaustive tables and tests.
    #[must_use]
    pub fn builtins() -> [Meter; 4] {
        [
            Meter::ActiveUsers,
            Meter::Transactions,
            Meter::StorageBytes,
            Meter::AiTokens,
        ]
    }

    /// How a period total for this meter combines its events.
    #[must_use]
    pub fn kind(&self) -> MeterKind {
        match self {
            Meter::ActiveUsers => MeterKind::DistinctSet,
            Meter::Transactions | Meter::AiTokens => MeterKind::Counter,
            Meter::StorageBytes => MeterKind::Gauge,
            Meter::Custom(_) => MeterKind::Counter,
        }
    }

    /// The stable name used in reports, store rows and error text.
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Meter::ActiveUsers => "active_users",
            Meter::Transactions => "transactions",
            Meter::StorageBytes => "storage_bytes",
            Meter::AiTokens => "ai_tokens",
            Meter::Custom(name) => name.as_str(),
        }
    }

    /// Whether `name` is a built-in meter's name.
    #[must_use]
    pub fn is_builtin_name(name: &str) -> bool {
        Self::builtins().iter().any(|meter| meter.name() == name)
    }
}

impl fmt::Display for Meter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// How the events of one meter combine into a period total.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MeterKind {
    /// Summed over the period: transactions, AI tokens, storage writes.
    Counter,
    /// The maximum observed in the period: storage bytes, seat high-water marks.
    Gauge,
    /// Distinct members over the period: a user who signs in 40 times counts once.
    DistinctSet,
}

impl MeterKind {
    /// The stable name, for reports.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            MeterKind::Counter => "counter",
            MeterKind::Gauge => "gauge",
            MeterKind::DistinctSet => "distinct_set",
        }
    }

    /// Whether this kind keeps members (a digest per distinct id) rather than only a
    /// running number; a store may use it to decide whether it needs a member table.
    #[must_use]
    pub fn keeps_members(self) -> bool {
        matches!(self, MeterKind::DistinctSet)
    }
}

impl fmt::Display for MeterKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A member of a distinct-set meter: the digest of the id, never the id itself.
///
/// The brief forbids storing user names or raw member ids: a distinct set only ever needs to tell
/// two members apart, so it holds a stable digest. The digest is the caller's
/// (a [`MeterEvent`](crate::MeterEvent) carries it pre-hashed) because the hashing
/// policy — which salt, which tenant scoping — is a privacy decision that belongs to
/// the producer, not to the arithmetic here.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MemberDigest(pub String);

impl MemberDigest {
    /// A digest from an already-computed hex or opaque string.
    #[must_use]
    pub fn new(digest: impl Into<String>) -> Self {
        Self(digest.into())
    }

    /// The digest as written.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Display for MemberDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_match_the_acceptance_table() {
        assert_eq!(Meter::ActiveUsers.kind(), MeterKind::DistinctSet);
        assert_eq!(Meter::Transactions.kind(), MeterKind::Counter);
        assert_eq!(Meter::StorageBytes.kind(), MeterKind::Gauge);
        assert_eq!(Meter::AiTokens.kind(), MeterKind::Counter);
    }

    #[test]
    fn builtin_names_are_stable_and_unique() {
        let meters = Meter::builtins();
        let names: Vec<&str> = meters.iter().map(Meter::name).collect();
        assert_eq!(
            names,
            ["active_users", "transactions", "storage_bytes", "ai_tokens"]
        );
        for name in names {
            assert!(Meter::is_builtin_name(name), "{name} should be built-in");
        }
        assert!(!Meter::is_builtin_name("documents"));
    }

    #[test]
    fn custom_meter_defaults_to_a_counter_and_keeps_its_name() {
        let meter = Meter::Custom("api_calls".to_owned());
        assert_eq!(meter.kind(), MeterKind::Counter);
        assert_eq!(meter.name(), "api_calls");
        assert_eq!(meter.to_string(), "api_calls");
    }

    #[test]
    fn only_distinct_sets_keep_members() {
        assert!(MeterKind::DistinctSet.keeps_members());
        assert!(!MeterKind::Counter.keeps_members());
        assert!(!MeterKind::Gauge.keeps_members());
    }
}
