//! The metadata every memory entry carries: kind, tier, tenant, creation instant, TTL,
//! confidence and what produced it.
//!
//! Nothing here is a float: a confidence is integer basis points (0 to 10,000) and a time is
//! Unix milliseconds supplied by the caller. No function in the crate reads a clock.

use serde::{Deserialize, Serialize};
use spot_agent_tiers::Tier;

use crate::error::MemoryError;
use crate::id::TenantId;

/// A confidence as integer basis points: `5000` is 50%, `10000` is certainty.
///
/// There is no float form (ADR-12's rule reaches further than money on this platform: a ratio
/// that decides whether a remedy may be proposed must be exact and orderable).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct Confidence(u16);

impl Confidence {
    /// Certainty: 10,000 basis points.
    pub const FULL: Self = Self(10_000);
    /// The floor a learned procedure must clear before it may become a candidate remedy when
    /// the policy takes the default.
    pub const DEFAULT_MINIMUM: Self = Self(5_000);

    /// A confidence in basis points, refusing more than certainty.
    ///
    /// # Errors
    ///
    /// [`MemoryError::EntryInvalid`] when the value is above 10,000.
    pub fn new(basis_points: u16) -> Result<Self, MemoryError> {
        if basis_points > 10_000 {
            return Err(MemoryError::EntryInvalid {
                kind: "value".to_owned(),
                id: "confidence".to_owned(),
                reason: "a confidence is 0 to 10,000 basis points".to_owned(),
            });
        }
        Ok(Self(basis_points))
    }

    /// The value in basis points.
    #[must_use]
    pub fn get(self) -> u16 {
        self.0
    }

    /// The value in whole percent, truncated (`6_667` is 66).
    #[must_use]
    pub fn percent(self) -> u16 {
        self.0 / 100
    }

    /// Whether this confidence is at least `floor`.
    #[must_use]
    pub fn is_at_least(self, floor: Self) -> bool {
        self.0 >= floor.0
    }
}

impl Default for Confidence {
    fn default() -> Self {
        Self::DEFAULT_MINIMUM
    }
}

/// Which of the three memory kinds an entry is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    /// What happened: an incident, its symptoms, actions and outcome.
    Episode,
    /// How to fix it: an ordered list of remedy steps.
    Procedure,
    /// What the installation is: a deployment fact at an epoch.
    Fact,
}

impl MemoryKind {
    /// Every kind, in canonical order.
    #[must_use]
    pub fn all() -> [Self; 3] {
        [Self::Episode, Self::Procedure, Self::Fact]
    }

    /// The stable snake_case text form, which is also the serde form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Episode => "episode",
            Self::Procedure => "procedure",
            Self::Fact => "fact",
        }
    }

    /// The DocType name a legal hold names this kind by (`agent_memory_episode`), so a hold on
    /// the memory namespace is a hold like any other (`spot-data-policy`).
    #[must_use]
    pub fn hold_doctype(self) -> &'static str {
        match self {
            Self::Episode => "agent_memory_episode",
            Self::Procedure => "agent_memory_procedure",
            Self::Fact => "agent_memory_fact",
        }
    }
}

/// A stand-in kind name for errors raised about a value rather than an entry.
impl std::fmt::Display for MemoryKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What produced a memory entry. There is deliberately no variant for a document: an entry
/// records what an agent did or learned, never a record's content.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    /// An incident an agent diagnosed.
    Incident,
    /// A remedy run (`spot-remedies`), with its audit id.
    RemedyRun,
    /// A person fixed the problem and the fix was recorded.
    Operator,
    /// A support case closed with a fix.
    SupportCase,
    /// A deployment fact read from the deployment model at an epoch.
    Deployment,
    /// An aggregate this crate computed while compacting old episodes. Deterministic, no model.
    Compaction,
}

impl SourceKind {
    /// The stable snake_case text form, which is also the serde form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Incident => "incident",
            Self::RemedyRun => "remedy_run",
            Self::Operator => "operator",
            Self::SupportCase => "support_case",
            Self::Deployment => "deployment",
            Self::Compaction => "compaction",
        }
    }

    /// Whether this source can produce an entry of `kind`.
    ///
    /// A learned procedure comes from something that happened (an incident, a remedy run, a
    /// person, a case); a fact comes from the deployment model, from a person or from this
    /// crate's own compaction — never from an incident alone, and never a document.
    #[must_use]
    pub fn fits(self, kind: MemoryKind) -> bool {
        match kind {
            MemoryKind::Episode | MemoryKind::Procedure => matches!(
                self,
                Self::Incident | Self::RemedyRun | Self::Operator | Self::SupportCase
            ),
            MemoryKind::Fact => {
                matches!(self, Self::Deployment | Self::Compaction | Self::Operator)
            }
        }
    }
}

impl std::fmt::Display for SourceKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The metadata every entry carries.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct EntryMeta {
    /// The tier that produced the entry, and the tier a reader needs for a procedure.
    pub tier: Tier,
    /// The tenant whose namespace holds the entry. Episodic memory never crosses it.
    pub tenant: TenantId,
    /// When the entry was recorded, in Unix milliseconds (supplied by the caller).
    pub created_ms: i64,
    /// How long the entry lives, in milliseconds, from `created_ms`; `None` keeps it forever.
    pub ttl_ms: Option<i64>,
    /// How sure the writer was of the entry.
    pub confidence: Confidence,
    /// What produced the entry.
    pub source: SourceKind,
}

impl EntryMeta {
    /// Metadata for a new entry: no TTL, the default confidence, `created_ms` from the caller.
    ///
    /// # Errors
    ///
    /// [`MemoryError::EntryInvalid`] when `created_ms` is before the Unix epoch.
    pub fn new(
        tier: Tier,
        tenant: TenantId,
        created_ms: i64,
        source: SourceKind,
    ) -> Result<Self, MemoryError> {
        if created_ms < 0 {
            return Err(MemoryError::EntryInvalid {
                kind: source.as_str().to_owned(),
                id: "created_ms".to_owned(),
                reason: "a creation instant is at or after the Unix epoch".to_owned(),
            });
        }
        Ok(Self {
            tier,
            tenant,
            created_ms,
            ttl_ms: None,
            confidence: Confidence::default(),
            source,
        })
    }

    /// The same metadata with a time to live.
    ///
    /// # Errors
    ///
    /// [`MemoryError::EntryInvalid`] when `ttl_ms` is negative.
    pub fn with_ttl(mut self, ttl_ms: i64) -> Result<Self, MemoryError> {
        if ttl_ms < 0 {
            return Err(MemoryError::EntryInvalid {
                kind: self.source.as_str().to_owned(),
                id: "ttl_ms".to_owned(),
                reason: "a time to live is zero or more milliseconds".to_owned(),
            });
        }
        self.ttl_ms = Some(ttl_ms);
        Ok(self)
    }

    /// The same metadata with a confidence.
    #[must_use]
    pub fn with_confidence(mut self, confidence: Confidence) -> Self {
        self.confidence = confidence;
        self
    }

    /// When the entry stops being readable, if it does. Saturates instead of overflowing.
    #[must_use]
    pub fn expires_at(&self) -> Option<i64> {
        self.ttl_ms.map(|ttl| self.created_ms.saturating_add(ttl))
    }

    /// Whether the entry has expired at `now_ms`. An entry with no TTL never expires.
    #[must_use]
    pub fn is_expired(&self, now_ms: i64) -> bool {
        self.expires_at().is_some_and(|at| now_ms >= at)
    }

    /// How old the entry is at `now_ms`, in milliseconds (never negative).
    #[must_use]
    pub fn age_ms(&self, now_ms: i64) -> i64 {
        now_ms.saturating_sub(self.created_ms).max(0)
    }
}

#[cfg(test)]
mod tests {
    use super::{Confidence, EntryMeta, MemoryKind, SourceKind};
    use crate::id::TenantId;
    use spot_agent_tiers::Tier;

    fn tenant() -> TenantId {
        TenantId::parse("tenant_a").unwrap()
    }

    #[test]
    fn confidence_is_integer_basis_points() {
        assert_eq!(Confidence::new(6_667).unwrap().percent(), 66);
        assert!(Confidence::new(10_000).is_ok());
        assert!(Confidence::new(10_001).is_err());
        assert!(Confidence::FULL.is_at_least(Confidence::DEFAULT_MINIMUM));
        assert_eq!(Confidence::default().get(), 5_000);
    }

    #[test]
    fn a_ttl_expires_at_its_creation_plus_its_life() {
        let meta = EntryMeta::new(Tier::T2, tenant(), 1_000, SourceKind::Incident)
            .and_then(|meta| meta.with_ttl(500))
            .unwrap();
        assert_eq!(meta.expires_at(), Some(1_500));
        assert!(!meta.is_expired(1_499));
        assert!(meta.is_expired(1_500));
        assert_eq!(meta.age_ms(1_200), 200);
        assert_eq!(meta.age_ms(900), 0);
    }

    #[test]
    fn an_entry_without_a_ttl_never_expires_and_a_negative_instant_is_refused() {
        let meta = EntryMeta::new(Tier::T0, tenant(), 1_000, SourceKind::Deployment).unwrap();
        assert_eq!(meta.expires_at(), None);
        assert!(!meta.is_expired(i64::MAX));
        assert!(EntryMeta::new(Tier::T0, tenant(), -1, SourceKind::Deployment).is_err());
        assert!(meta.with_ttl(-1).is_err());
    }

    #[test]
    fn every_kind_has_a_hold_doctype_and_a_stable_name() {
        for kind in MemoryKind::all() {
            assert!(kind.hold_doctype().starts_with("agent_memory_"));
            assert_eq!(kind.to_string(), kind.as_str());
        }
        assert_eq!(MemoryKind::all().len(), 3);
    }
}
