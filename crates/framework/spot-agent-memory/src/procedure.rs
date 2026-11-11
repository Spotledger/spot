//! Procedural memory: a learned how-to, as an ordered list of remedy steps with their
//! parameters, plus the incidents that confirmed it.
//!
//! A procedure is what becomes a candidate remedy (see [`crate::learning`]). It carries the tier
//! that learned it, and only an agent at that tier or above may read it back — a T2 data repair
//! is not shown to a T0 agent that could not run it.

use serde::Serialize;
use serde_json::Value as Json;
use spot_remedies::RemedyId;
use std::collections::BTreeMap;

use crate::embed::{Embedder, embed_one, vector_is_usable};
use crate::episode::{Outcome, symptom_text};
use crate::error::MemoryError;
use crate::id::{EpisodeId, ProcedureId, Symptom};
use crate::meta::{EntryMeta, MemoryKind};
use crate::sanitiser::{Sanitiser, Summary};

/// One parameter value a remedy step runs with.
///
/// There is no float variant: money, quantities and rates are integers on this platform
/// (ADR-12), and text is a [`Summary`], which only the sanitiser produces.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum ParamValue {
    /// A short text value: a status, a state name, a reference.
    Text(Summary),
    /// A signed integer: a retry count, a version, a slot.
    Integer(i64),
    /// A boolean.
    Bool(bool),
}

impl ParamValue {
    /// A text parameter, scanned by the sanitiser before it can be used.
    ///
    /// # Errors
    ///
    /// [`MemoryError::Unsanitised`] when the text carries a record value or a secret.
    pub fn text(sanitiser: &Sanitiser, text: &str) -> Result<Self, MemoryError> {
        Ok(Self::Text(sanitiser.check(text)?))
    }

    /// The value as JSON, for the remedy's parameter schema.
    #[must_use]
    pub fn to_json(&self) -> Json {
        match self {
            Self::Text(summary) => Json::String(summary.as_str().to_owned()),
            Self::Integer(value) => Json::from(*value),
            Self::Bool(value) => Json::Bool(*value),
        }
    }

    /// The JSON type name (`string`, `integer`, `boolean`), for schema inference.
    #[must_use]
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Text(_) => "string",
            Self::Integer(_) => "integer",
            Self::Bool(_) => "boolean",
        }
    }
}

/// One step of a learned procedure: a remedy and the parameters it ran with.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ProcedureStep {
    /// The remedy the step ran (`spot-remedies`).
    pub remedy: RemedyId,
    /// Its parameters, by name.
    pub params: BTreeMap<String, ParamValue>,
}

impl ProcedureStep {
    /// A step with no parameters.
    #[must_use]
    pub fn new(remedy: RemedyId) -> Self {
        Self {
            remedy,
            params: BTreeMap::new(),
        }
    }

    /// The same step with one parameter added.
    #[must_use]
    pub fn with(mut self, name: &str, value: ParamValue) -> Self {
        self.params.insert(name.to_owned(), value);
        self
    }
}

/// One incident that confirmed (or harmed) a procedure.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Confirmation {
    /// The episode that recorded the run.
    pub episode: EpisodeId,
    /// How that run ended.
    pub outcome: Outcome,
    /// When it ended, in Unix milliseconds (supplied by the caller).
    pub at_ms: i64,
}

impl Confirmation {
    /// A confirmation of one incident's outcome.
    ///
    /// # Errors
    ///
    /// [`MemoryError::EntryInvalid`] when `at_ms` is before the Unix epoch.
    pub fn new(episode: EpisodeId, outcome: Outcome, at_ms: i64) -> Result<Self, MemoryError> {
        if at_ms < 0 {
            return Err(MemoryError::EntryInvalid {
                kind: MemoryKind::Procedure.as_str().to_owned(),
                id: episode.to_string(),
                reason: "a confirmation instant is at or after the Unix epoch".to_owned(),
            });
        }
        Ok(Self {
            episode,
            outcome,
            at_ms,
        })
    }
}

/// The fields a caller supplies to record a procedure.
pub struct ProcedureInput<'a> {
    /// The procedure's id.
    pub id: ProcedureId,
    /// A one-line title in the learner's words. Scanned by the sanitiser.
    pub title: &'a str,
    /// The structured symptom signature it fixes; at least one.
    pub symptoms: Vec<Symptom>,
    /// The remedy steps, in order; at least one.
    pub steps: Vec<ProcedureStep>,
    /// The incidents that ran it.
    pub confirmations: Vec<Confirmation>,
    /// Tier, tenant, creation instant, TTL, confidence and source.
    pub meta: EntryMeta,
}

/// A learned how-to.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Procedure {
    /// The procedure's id.
    pub id: ProcedureId,
    /// A one-line title, after the sanitiser accepted it.
    pub title: Summary,
    /// The symptom signature it fixes.
    pub symptoms: Vec<Symptom>,
    /// The remedy steps, in order.
    pub steps: Vec<ProcedureStep>,
    /// The incidents that ran it, with their outcomes.
    pub confirmations: Vec<Confirmation>,
    /// Tier, tenant, creation instant, TTL, confidence and source.
    pub meta: EntryMeta,
    /// The embedding of the procedure's text, from the host's provider.
    pub vector: Vec<f32>,
}

impl Procedure {
    /// Records a procedure: the title is scanned, the text embedded, the structure validated.
    ///
    /// # Errors
    ///
    /// [`MemoryError::Unsanitised`] / [`MemoryError::Unstorable`] for the title,
    /// [`MemoryError::Embed`] when the provider fails, and [`MemoryError::EntryInvalid`] when the
    /// procedure breaks a structural rule.
    pub fn record(
        sanitiser: &Sanitiser,
        embedder: &dyn Embedder,
        input: ProcedureInput<'_>,
    ) -> Result<Self, MemoryError> {
        let title = sanitiser.check(input.title)?;
        let text = format!(
            "{} {} {}",
            symptom_text(&input.symptoms),
            title.as_str(),
            remedy_text(&input.steps)
        );
        let vector = embed_one(embedder, &text)?;
        let procedure = Self {
            id: input.id,
            title,
            symptoms: input.symptoms,
            steps: input.steps,
            confirmations: input.confirmations,
            meta: input.meta,
            vector,
        };
        procedure.validate()?;
        Ok(procedure)
    }

    /// The entry kind.
    #[must_use]
    pub fn kind(&self) -> MemoryKind {
        MemoryKind::Procedure
    }

    /// The id, as the store keys it.
    #[must_use]
    pub fn id_str(&self) -> &str {
        self.id.as_str()
    }

    /// The text recall shows for this procedure.
    #[must_use]
    pub fn text(&self) -> &str {
        self.title.as_str()
    }

    /// The remedies the procedure runs, in order.
    #[must_use]
    pub fn remedies(&self) -> Vec<&RemedyId> {
        self.steps.iter().map(|step| &step.remedy).collect()
    }

    /// Whether the procedure runs `remedy`.
    #[must_use]
    pub fn runs(&self, remedy: &RemedyId) -> bool {
        self.steps.iter().any(|step| &step.remedy == remedy)
    }

    /// How many incident runs ended in the symptom being resolved.
    #[must_use]
    pub fn successes(&self) -> u32 {
        self.confirmations
            .iter()
            .filter(|confirmation| confirmation.outcome.is_success())
            .count()
            .try_into()
            .unwrap_or(u32::MAX)
    }

    /// How many incident runs made things worse.
    #[must_use]
    pub fn harms(&self) -> u32 {
        self.confirmations
            .iter()
            .filter(|confirmation| confirmation.outcome.is_harm())
            .count()
            .try_into()
            .unwrap_or(u32::MAX)
    }

    /// Checks the procedure's structure.
    ///
    /// # Errors
    ///
    /// [`MemoryError::EntryInvalid`] naming the rule that failed.
    pub fn validate(&self) -> Result<(), MemoryError> {
        let refuse = |reason: &str| {
            Err(MemoryError::EntryInvalid {
                kind: MemoryKind::Procedure.as_str().to_owned(),
                id: self.id.to_string(),
                reason: reason.to_owned(),
            })
        };
        if self.symptoms.is_empty() {
            return refuse("a procedure names at least one symptom");
        }
        if self.steps.is_empty() {
            return refuse("a procedure has at least one remedy step");
        }
        if !vector_is_usable(&self.vector) {
            return refuse("a procedure carries an embedding");
        }
        if !self.meta.source.fits(MemoryKind::Procedure) {
            return refuse("a procedure's source is an incident, a remedy run, a person or a case");
        }
        if self.steps.iter().any(|step| {
            step.params.values().any(
                |value| matches!(value, ParamValue::Text(summary) if summary.as_str().is_empty()),
            )
        }) {
            return refuse("a procedure step's text parameter is not empty");
        }
        Ok(())
    }

    /// Whether an agent at `agent_tier` may read this procedure.
    ///
    /// A procedure is readable at the tier that learned it or above: the reader must be able to
    /// run every step it names. Nothing above the reader's tier is ever returned (WS13).
    #[must_use]
    pub fn readable_by(&self, agent_tier: spot_agent_tiers::Tier) -> bool {
        agent_tier >= self.meta.tier
    }
}

/// The remedies and their parameter names as text, for embedding.
fn remedy_text(steps: &[ProcedureStep]) -> String {
    steps
        .iter()
        .map(|step| {
            let names: Vec<&str> = step.params.keys().map(String::as_str).collect();
            if names.is_empty() {
                step.remedy.to_string()
            } else {
                format!("{} {}", step.remedy, names.join(" "))
            }
        })
        .collect::<Vec<String>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::{Confirmation, ParamValue, Procedure, ProcedureInput, ProcedureStep};
    use crate::episode::Outcome;
    use crate::id::{EpisodeId, ProcedureId, Symptom, TenantId};
    use crate::meta::{EntryMeta, SourceKind};
    use crate::testing::{embedder, sanitiser};
    use spot_agent_tiers::Tier;
    use spot_remedies::RemedyId;

    fn episode_id(n: u32) -> EpisodeId {
        EpisodeId::parse(&format!("episode.job_stuck.e{n:04}")).unwrap()
    }

    fn procedure_input<'a>(title: &'a str) -> ProcedureInput<'a> {
        let meta = EntryMeta::new(
            Tier::T2,
            TenantId::parse("tenant_a").unwrap(),
            1_000,
            SourceKind::RemedyRun,
        )
        .unwrap();
        ProcedureInput {
            id: ProcedureId::parse("procedure.job_stuck").unwrap(),
            title,
            symptoms: vec![Symptom::parse("job.stuck_running").unwrap()],
            steps: vec![
                ProcedureStep::new(RemedyId::new("job.retry_failed"))
                    .with("attempts", ParamValue::Integer(3)),
            ],
            confirmations: vec![
                Confirmation::new(episode_id(1), Outcome::Resolved, 1_000).unwrap(),
                Confirmation::new(episode_id(2), Outcome::Resolved, 2_000).unwrap(),
            ],
            meta,
        }
    }

    #[test]
    fn a_procedure_records_steps_confirmations_and_a_title() {
        let procedure =
            Procedure::record(&sanitiser(), &embedder(), procedure_input("retry the job")).unwrap();
        assert_eq!(procedure.text(), "retry the job");
        assert_eq!(procedure.successes(), 2);
        assert_eq!(procedure.harms(), 0);
        assert!(procedure.runs(&RemedyId::new("job.retry_failed")));
        assert_eq!(procedure.validate(), Ok(()));
    }

    #[test]
    fn a_procedure_without_steps_is_refused() {
        let mut input = procedure_input("retry the job");
        input.steps.clear();
        let error = Procedure::record(&sanitiser(), &embedder(), input).unwrap_err();
        assert_eq!(error.code(), "agent_memory.entry_invalid");
    }

    #[test]
    fn a_text_parameter_goes_through_the_sanitiser() {
        let sanitiser = sanitiser();
        assert!(ParamValue::text(&sanitiser, "running").is_ok());
        assert!(ParamValue::text(&sanitiser, "amount: 1500").is_err());
        assert_eq!(ParamValue::Integer(3).to_json().to_string(), "3");
        assert_eq!(ParamValue::Bool(true).type_name(), "boolean");
    }

    #[test]
    fn only_the_learning_tier_or_above_reads_a_procedure() {
        let procedure =
            Procedure::record(&sanitiser(), &embedder(), procedure_input("retry the job")).unwrap();
        assert!(!procedure.readable_by(Tier::T0));
        assert!(!procedure.readable_by(Tier::T1));
        assert!(procedure.readable_by(Tier::T2));
        assert!(procedure.readable_by(Tier::T3));
    }
}
