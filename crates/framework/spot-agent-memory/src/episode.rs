//! Episodic memory: what happened, as symptoms, actions, an outcome and the audit entries
//! behind them.
//!
//! An episode names the record it is about **by type and id** (`RecordRef`) and links the audit
//! entries the actions produced. Its only free text is a [`Summary`], which only the sanitiser
//! can produce, so a record's values cannot enter an episode by construction.

use std::fmt;

use serde::{Deserialize, Serialize};
use spot_agent_tiers::Tier;
use spot_remedies::RemedyId;

use crate::embed::{Embedder, embed_one, vector_is_usable};
use crate::error::MemoryError;
use crate::id::{AuditRef, EpisodeId, RecordRef, Symptom};
use crate::meta::{EntryMeta, MemoryKind};
use crate::sanitiser::{Sanitiser, Summary};

/// How an incident ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// The symptom went away.
    Resolved,
    /// The symptom improved without going away.
    Partly,
    /// Nothing changed.
    Unresolved,
    /// The action made things worse.
    Harm,
}

impl Outcome {
    /// The stable snake_case text form, which is also the serde form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Resolved => "resolved",
            Self::Partly => "partly",
            Self::Unresolved => "unresolved",
            Self::Harm => "harm",
        }
    }

    /// Whether the episode counts as a success: only a resolved incident does.
    #[must_use]
    pub fn is_success(self) -> bool {
        matches!(self, Self::Resolved)
    }

    /// Whether the episode records harm, which forbids a remedy ever being proposed from it.
    #[must_use]
    pub fn is_harm(self) -> bool {
        matches!(self, Self::Harm)
    }
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One action taken during an incident: the audit entry it wrote, the tier it ran at, and the
/// remedy it ran when it ran one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ActionRef {
    /// The audit entry the action wrote. This is the episode's provenance.
    pub audit: AuditRef,
    /// The tier the action ran at.
    pub tier: Tier,
    /// The remedy that ran, when the action was a remedy.
    pub remedy: Option<RemedyId>,
}

impl ActionRef {
    /// An action that ran a remedy.
    #[must_use]
    pub fn remedy(audit: AuditRef, tier: Tier, remedy: RemedyId) -> Self {
        Self {
            audit,
            tier,
            remedy: Some(remedy),
        }
    }

    /// An action that was not a remedy (a read, a diagnosis, a setting read back).
    #[must_use]
    pub fn diagnosis(audit: AuditRef, tier: Tier) -> Self {
        Self {
            audit,
            tier,
            remedy: None,
        }
    }
}

/// The fields a caller supplies to record an episode.
pub struct EpisodeInput<'a> {
    /// The episode's id.
    pub id: EpisodeId,
    /// The record the incident is about, by type and id.
    pub incident: RecordRef,
    /// The structured symptom signature; at least one.
    pub symptoms: Vec<Symptom>,
    /// What was done, in order.
    pub actions: Vec<ActionRef>,
    /// How it ended.
    pub outcome: Outcome,
    /// A short summary in the agent's words. Scanned by the sanitiser.
    pub summary: &'a str,
    /// The audit entries the incident produced; at least one.
    pub audits: Vec<AuditRef>,
    /// Tier, tenant, creation instant, TTL, confidence and source.
    pub meta: EntryMeta,
}

/// One recorded incident.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Episode {
    /// The episode's id.
    pub id: EpisodeId,
    /// The record the incident is about, by type and id. Never a value from it.
    pub incident: RecordRef,
    /// The structured symptom signature recall filters on.
    pub symptoms: Vec<Symptom>,
    /// What was done, in order.
    pub actions: Vec<ActionRef>,
    /// How it ended.
    pub outcome: Outcome,
    /// The summary, after the sanitiser accepted it.
    pub summary: Summary,
    /// The audit entries behind the episode.
    pub audits: Vec<AuditRef>,
    /// Tier, tenant, creation instant, TTL, confidence and source.
    pub meta: EntryMeta,
    /// The embedding of the episode's text, from the host's provider.
    pub vector: Vec<f32>,
}

impl Episode {
    /// Records an episode: the summary is scanned, the text embedded, the structure validated.
    ///
    /// # Errors
    ///
    /// [`MemoryError::Unsanitised`] / [`MemoryError::Unstorable`] when the summary carries a
    /// record value, [`MemoryError::Embed`] when the provider fails, and
    /// [`MemoryError::EntryInvalid`] when the episode breaks a structural rule.
    pub fn record(
        sanitiser: &Sanitiser,
        embedder: &dyn Embedder,
        input: EpisodeInput<'_>,
    ) -> Result<Self, MemoryError> {
        let summary = sanitiser.check(input.summary)?;
        let symptoms = input.symptoms;
        let text = format!(
            "{} {} {}",
            symptom_text(&symptoms),
            input.outcome.as_str(),
            summary.as_str()
        );
        let vector = embed_one(embedder, &text)?;
        let episode = Self {
            id: input.id,
            incident: input.incident,
            symptoms,
            actions: input.actions,
            outcome: input.outcome,
            summary,
            audits: input.audits,
            meta: input.meta,
            vector,
        };
        episode.validate()?;
        Ok(episode)
    }

    /// The entry kind.
    #[must_use]
    pub fn kind(&self) -> MemoryKind {
        MemoryKind::Episode
    }

    /// The id, as the store keys it.
    #[must_use]
    pub fn id_str(&self) -> &str {
        self.id.as_str()
    }

    /// The text recall shows for this episode.
    #[must_use]
    pub fn text(&self) -> &str {
        self.summary.as_str()
    }

    /// Whether the episode names `remedy` in its actions.
    #[must_use]
    pub fn ran(&self, remedy: &RemedyId) -> bool {
        self.actions
            .iter()
            .any(|action| action.remedy.as_ref() == Some(remedy))
    }

    /// Checks the episode's structure.
    ///
    /// An episode has at least one symptom (or recall cannot filter it), at least one audit entry
    /// (or it has no provenance) and a usable vector.
    ///
    /// # Errors
    ///
    /// [`MemoryError::EntryInvalid`] naming the rule that failed.
    pub fn validate(&self) -> Result<(), MemoryError> {
        let refuse = |reason: &str| {
            Err(MemoryError::EntryInvalid {
                kind: MemoryKind::Episode.as_str().to_owned(),
                id: self.id.to_string(),
                reason: reason.to_owned(),
            })
        };
        if self.symptoms.is_empty() {
            return refuse("an episode names at least one symptom");
        }
        if self.audits.is_empty() {
            return refuse("an episode links at least one audit entry");
        }
        if !vector_is_usable(&self.vector) {
            return refuse("an episode carries an embedding");
        }
        if !self.meta.source.fits(MemoryKind::Episode) {
            return refuse("an episode's source is an incident, a remedy run, a person or a case");
        }
        Ok(())
    }
}

/// The symptom tags as one space-separated string, for embedding.
pub(crate) fn symptom_text(symptoms: &[Symptom]) -> String {
    symptoms
        .iter()
        .map(Symptom::as_str)
        .collect::<Vec<&str>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::{ActionRef, Episode, EpisodeInput, Outcome};
    use crate::id::{AuditRef, EpisodeId, RecordRef, Symptom};
    use crate::meta::{EntryMeta, SourceKind};
    use crate::testing::{embedder, sanitiser};
    use spot_agent_tiers::Tier;

    fn meta() -> EntryMeta {
        EntryMeta::new(
            Tier::T0,
            crate::id::TenantId::parse("tenant_a").unwrap(),
            1_000,
            SourceKind::Incident,
        )
        .unwrap()
    }

    fn input<'a>(summary: &'a str) -> EpisodeInput<'a> {
        EpisodeInput {
            id: EpisodeId::parse("episode.job_stuck.one").unwrap(),
            incident: RecordRef::new("job", "018f-0001").unwrap(),
            symptoms: vec![Symptom::parse("job.stuck_running").unwrap()],
            actions: vec![ActionRef::diagnosis(
                AuditRef::parse("audit-1").unwrap(),
                Tier::T0,
            )],
            outcome: Outcome::Resolved,
            summary,
            audits: vec![AuditRef::parse("audit-1").unwrap()],
            meta: meta(),
        }
    }

    #[test]
    fn an_episode_records_its_summary_through_the_sanitiser() {
        let episode = Episode::record(&sanitiser(), &embedder(), input("unstuck the job")).unwrap();
        assert_eq!(episode.text(), "unstuck the job");
        assert_eq!(episode.kind(), crate::meta::MemoryKind::Episode);
        assert!(!episode.vector.is_empty());
        assert_eq!(episode.validate(), Ok(()));
    }

    #[test]
    fn an_episode_refuses_a_record_value_in_its_summary() {
        let error = Episode::record(
            &sanitiser(),
            &embedder(),
            input("the invoice total was Rs 12,500"),
        )
        .unwrap_err();
        assert_eq!(error.code(), "agent_memory.unsanitised");
    }

    #[test]
    fn an_episode_without_a_symptom_or_an_audit_entry_is_refused() {
        let mut first = input("unstuck the job");
        first.symptoms.clear();
        let error = Episode::record(&sanitiser(), &embedder(), first).unwrap_err();
        assert_eq!(error.code(), "agent_memory.entry_invalid");

        let mut input = input("unstuck the job");
        input.audits.clear();
        let error = Episode::record(&sanitiser(), &embedder(), input).unwrap_err();
        assert_eq!(error.code(), "agent_memory.entry_invalid");
    }

    #[test]
    fn a_compaction_cannot_be_recorded_as_an_episode() {
        let mut input = input("unstuck the job");
        input.meta = input
            .meta
            .clone()
            .with_ttl(1_000)
            .unwrap()
            .with_confidence(crate::meta::Confidence::FULL);
        input.meta.source = SourceKind::Compaction;
        let error = Episode::record(&sanitiser(), &embedder(), input).unwrap_err();
        assert_eq!(error.code(), "agent_memory.entry_invalid");
    }
}
