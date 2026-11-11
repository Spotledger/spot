//! Factual memory: what this installation is, keyed by the deployment model's epoch.
//!
//! A fact is about the deployment, not about a record. It is stamped with the
//! `spot-deployment-model` epoch that was current when it was read, so a stale fact is visible as
//! stale: recall reports the epoch, and a fact from an older epoch than the model's is exactly
//! the "knowledge lags the deployment" failure WS13 forbids.

use std::fmt;

use serde::Serialize;
use spot_deployment_model::DeploymentModel;

use crate::embed::{Embedder, embed_one, vector_is_usable};
use crate::episode::Outcome;
use crate::error::MemoryError;
use crate::id::{FactId, FactKey, Symptom};
use crate::meta::{EntryMeta, MemoryKind, SourceKind};
use crate::sanitiser::{Sanitiser, Summary};

/// The deployment model's epoch, as a fact records it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct Epoch(u64);

impl Epoch {
    /// The epoch numbered `value`.
    #[must_use]
    pub fn new(value: u64) -> Self {
        Self(value)
    }

    /// The epoch a deployment model is at, so a fact never claims an epoch the model never had.
    #[must_use]
    pub fn of(model: &DeploymentModel) -> Self {
        Self(model.epoch)
    }

    /// The epoch number.
    #[must_use]
    pub fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for Epoch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The aggregate of the episodes that compaction folded into one fact.
///
/// Every field is an integer or an enum: the aggregate is computed deterministically by this
/// crate, with no model call, so the same episodes always produce the same figures.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct EpisodeAggregate {
    /// The symptom the folded episodes shared.
    pub symptom: Symptom,
    /// The outcome the folded episodes shared.
    pub outcome: Outcome,
    /// How many episodes were folded.
    pub count: u64,
    /// The earliest folded episode's creation instant, in Unix milliseconds.
    pub first_ms: i64,
    /// The latest folded episode's creation instant, in Unix milliseconds.
    pub last_ms: i64,
}

impl EpisodeAggregate {
    /// An aggregate of one folded episode.
    #[must_use]
    pub fn first(symptom: Symptom, outcome: Outcome, at_ms: i64) -> Self {
        Self {
            symptom,
            outcome,
            count: 1,
            first_ms: at_ms,
            last_ms: at_ms,
        }
    }

    /// The same aggregate with one more episode folded in.
    #[must_use]
    pub fn merge(self, other: Self) -> Self {
        Self {
            symptom: self.symptom,
            outcome: self.outcome,
            count: self.count.saturating_add(other.count),
            first_ms: self.first_ms.min(other.first_ms),
            last_ms: self.last_ms.max(other.last_ms),
        }
    }
}

/// What a fact says. No variant can hold a document payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum FactValue {
    /// A short text value (`16.4`, `standalone`).
    Text(Summary),
    /// A signed integer (a version part, a count, a slot).
    Integer(i64),
    /// A boolean.
    Boolean(bool),
    /// An instant, in Unix milliseconds.
    Instant(i64),
    /// The aggregate compaction produced from old episodes.
    Stats(EpisodeAggregate),
}

impl FactValue {
    /// A text fact value, scanned by the sanitiser before it can be stored.
    ///
    /// # Errors
    ///
    /// [`MemoryError::Unsanitised`] when the text carries a record value or a secret.
    pub fn text(sanitiser: &Sanitiser, text: &str) -> Result<Self, MemoryError> {
        Ok(Self::Text(sanitiser.check(text)?))
    }

    /// The value as text, for embedding and for the recall snippet.
    #[must_use]
    pub fn as_text(&self) -> String {
        match self {
            Self::Text(summary) => summary.as_str().to_owned(),
            Self::Integer(value) => value.to_string(),
            Self::Boolean(value) => value.to_string(),
            Self::Instant(value) => value.to_string(),
            Self::Stats(aggregate) => format!(
                "{} {} {} {} {}",
                aggregate.symptom,
                aggregate.outcome,
                aggregate.count,
                aggregate.first_ms,
                aggregate.last_ms
            ),
        }
    }
}

/// The fields a caller supplies to record a fact.
pub struct FactInput<'a> {
    /// The fact's id.
    pub id: FactId,
    /// The stable key it is read by (`backend.postgres_version`).
    pub key: FactKey,
    /// What it says.
    pub value: FactValue,
    /// The deployment epoch the value was read at.
    pub epoch: Epoch,
    /// Tier, tenant, creation instant, TTL, confidence and source.
    pub meta: EntryMeta,
    /// The text to embed. Defaults to the key and the value when `None`.
    pub text: Option<&'a str>,
}

/// One deployment fact.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Fact {
    /// The fact's id.
    pub id: FactId,
    /// The stable key it is read by.
    pub key: FactKey,
    /// What it says.
    pub value: FactValue,
    /// The deployment epoch the value was read at.
    pub epoch: Epoch,
    /// Tier, tenant, creation instant, TTL, confidence and source.
    pub meta: EntryMeta,
    /// The embedding of the fact's text, from the host's provider.
    pub vector: Vec<f32>,
}

impl Fact {
    /// Records a fact: any text is scanned, the text embedded, the structure validated.
    ///
    /// # Errors
    ///
    /// [`MemoryError::Unsanitised`] / [`MemoryError::Unstorable`] for the free text,
    /// [`MemoryError::Embed`] when the provider fails, and [`MemoryError::EntryInvalid`] when the
    /// fact breaks a structural rule.
    pub fn record(
        sanitiser: &Sanitiser,
        embedder: &dyn Embedder,
        input: FactInput<'_>,
    ) -> Result<Self, MemoryError> {
        if let Some(text) = input.text {
            // The caller's embedding text is a free-text field like any other: scan it even
            // though only its vector is kept, so no document value can shape memory.
            sanitiser.check(text)?;
        }
        let text = input.text.map_or_else(
            || format!("{} {}", input.key, input.value.as_text()),
            str::to_owned,
        );
        let vector = embed_one(embedder, &text)?;
        let fact = Self {
            id: input.id,
            key: input.key,
            value: input.value,
            epoch: input.epoch,
            meta: input.meta,
            vector,
        };
        fact.validate()?;
        Ok(fact)
    }

    /// The aggregate fact compaction writes: no free text, no sanitiser, a deterministic vector
    /// over the rendered figures.
    pub(crate) fn aggregate(
        embedder: &dyn Embedder,
        id: FactId,
        key: FactKey,
        aggregate: EpisodeAggregate,
        epoch: Epoch,
        meta: EntryMeta,
    ) -> Result<Self, MemoryError> {
        let value = FactValue::Stats(aggregate);
        let text = format!("{key} {}", value.as_text());
        let vector = embed_one(embedder, &text)?;
        let fact = Self {
            id,
            key,
            value,
            epoch,
            meta,
            vector,
        };
        fact.validate()?;
        Ok(fact)
    }

    /// The entry kind.
    #[must_use]
    pub fn kind(&self) -> MemoryKind {
        MemoryKind::Fact
    }

    /// The id, as the store keys it.
    #[must_use]
    pub fn id_str(&self) -> &str {
        self.id.as_str()
    }

    /// The text recall shows for this fact.
    #[must_use]
    pub fn text(&self) -> String {
        format!("{}: {}", self.key, self.value.as_text())
    }

    /// Whether the fact was read at an older epoch than `current`: its knowledge lags the
    /// deployment.
    #[must_use]
    pub fn is_stale_at(&self, current: Epoch) -> bool {
        self.epoch < current
    }

    /// Checks the fact's structure.
    ///
    /// # Errors
    ///
    /// [`MemoryError::EntryInvalid`] naming the rule that failed.
    pub fn validate(&self) -> Result<(), MemoryError> {
        let refuse = |reason: &str| {
            Err(MemoryError::EntryInvalid {
                kind: MemoryKind::Fact.as_str().to_owned(),
                id: self.id.to_string(),
                reason: reason.to_owned(),
            })
        };
        if !vector_is_usable(&self.vector) {
            return refuse("a fact carries an embedding");
        }
        if !self.meta.source.fits(MemoryKind::Fact) {
            return refuse("a fact's source is the deployment model, a person or a compaction");
        }
        let stats = matches!(self.value, FactValue::Stats(_));
        if stats != (self.meta.source == SourceKind::Compaction) {
            return refuse(
                "only a compaction produces aggregate statistics, and it produces nothing else",
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{EpisodeAggregate, Epoch, Fact, FactInput, FactValue};
    use crate::episode::Outcome;
    use crate::id::{FactId, FactKey, Symptom, TenantId};
    use crate::meta::{EntryMeta, SourceKind};
    use crate::testing::{embedder, sanitiser};
    use spot_agent_tiers::Tier;

    fn meta(source: SourceKind) -> EntryMeta {
        EntryMeta::new(
            Tier::T1,
            TenantId::parse("tenant_a").unwrap(),
            1_000,
            source,
        )
        .unwrap()
    }

    #[test]
    fn a_fact_records_its_key_value_and_epoch() {
        let sanitiser = sanitiser();
        let fact = Fact::record(
            &sanitiser,
            &embedder(),
            FactInput {
                id: FactId::parse("fact.backend.postgres").unwrap(),
                key: FactKey::parse("backend.postgres_version").unwrap(),
                value: FactValue::text(&sanitiser, "16.4").unwrap(),
                epoch: Epoch::new(7),
                meta: meta(SourceKind::Deployment),
                text: None,
            },
        )
        .unwrap();
        assert_eq!(fact.epoch.get(), 7);
        assert_eq!(fact.text(), "backend.postgres_version: 16.4");
        assert!(!fact.is_stale_at(Epoch::new(7)));
        assert!(fact.is_stale_at(Epoch::new(8)));
    }

    #[test]
    fn a_fact_refuses_a_record_value_in_its_embedding_text() {
        let error = Fact::record(
            &sanitiser(),
            &embedder(),
            FactInput {
                id: FactId::parse("fact.backend.postgres").unwrap(),
                key: FactKey::parse("backend.postgres_version").unwrap(),
                value: FactValue::Integer(16),
                epoch: Epoch::new(7),
                meta: meta(SourceKind::Deployment),
                text: Some("the invoice total was Rs 12,500"),
            },
        )
        .unwrap_err();
        assert_eq!(error.code(), "agent_memory.unsanitised");
    }

    #[test]
    fn statistics_come_only_from_compaction_and_compaction_produces_only_statistics() {
        let aggregate = EpisodeAggregate::first(
            Symptom::parse("job.stuck_running").unwrap(),
            Outcome::Resolved,
            1_000,
        )
        .merge(EpisodeAggregate::first(
            Symptom::parse("job.stuck_running").unwrap(),
            Outcome::Resolved,
            5_000,
        ));
        assert_eq!(aggregate.count, 2);
        assert_eq!(aggregate.first_ms, 1_000);
        assert_eq!(aggregate.last_ms, 5_000);

        let fact = Fact::aggregate(
            &embedder(),
            FactId::parse("fact.episode.job_stuck_running.resolved").unwrap(),
            FactKey::parse("episode.aggregate.job_stuck_running").unwrap(),
            aggregate.clone(),
            Epoch::new(3),
            meta(SourceKind::Compaction),
        )
        .unwrap();
        assert!(fact.text().contains("2"));

        // A deployment-sourced fact may not carry statistics.
        let wrong = Fact::aggregate(
            &embedder(),
            FactId::parse("fact.episode.job_stuck_running.resolved").unwrap(),
            FactKey::parse("episode.aggregate.job_stuck_running").unwrap(),
            aggregate,
            Epoch::new(3),
            meta(SourceKind::Deployment),
        );
        assert_eq!(
            wrong
                .map(|fact| fact.id_str().to_owned())
                .unwrap_err()
                .code(),
            "agent_memory.entry_invalid"
        );
    }
}
