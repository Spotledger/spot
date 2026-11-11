//! The learning loop: a repeated successful procedure becomes a **candidate** remedy, and a
//! candidate is unusable until CI has run it and promoted it.
//!
//! WS13: "when a registered deep agent or a human fixes a new kind of problem, the fix becomes a
//! candidate remedy; it passes CI like any other code before lower tiers may use it unattended."
//! This module is the model of that sentence, with the gate made structural:
//!
//! 1. [`propose_remedy`] turns a procedure with at least the policy's confirming incidents and no
//!    record of harm into a [`CandidateRemedy`] in the `spot-remedies` spec format; the bundle
//!    (spec plus a self-test scenario skeleton) and the `candidates/` queue are
//!    [`crate::candidate`]'s.
//! 2. The candidate walks [`CandidateStatus`] `Proposed → InCi → Passed | Rejected`. The
//!    transitions are crate-private: the only code that moves a candidate is a
//!    [`PromotionPort`], and it moves it to `Passed` only when the port's own CI record
//!    ([`PromotionPort::outcome`]) says that run passed.
//! 3. Only a [`PromotionPort`] promotion turns a `Passed` candidate into a [`Promoted`] one, and
//!    the promotion records **the CI run id** it is promoting.
//! 4. [`UsableRemedy`] has no public constructor, so [`PromotionPort::promote`] is the one public
//!    way to obtain one and unattended use of an unproven candidate does not compile — the gate
//!    is the type, not a check somebody remembers to call. [`can_use`] states the same rule as a
//!    boolean for a host that only has a serialised candidate.
//!
//! Code outside this crate cannot forge the CI-passed fact:
//!
//! ```compile_fail,E0624
//! use spot_agent_memory::{CandidateRemedy, CiRunId};
//! fn forge(mut candidate: CandidateRemedy) {
//!     let run = CiRunId::parse("made-up-run").unwrap();
//!     candidate.begin_ci(run.clone()).unwrap();
//! }
//! ```
//!
//! ```compile_fail,E0624
//! use spot_agent_memory::{CandidateRemedy, CiRunId};
//! fn forge(mut candidate: CandidateRemedy) {
//!     let run = CiRunId::parse("made-up-run").unwrap();
//!     candidate.pass_ci(&run).unwrap();
//! }
//! ```
//!
//! ```compile_fail,E0624
//! use spot_agent_memory::{CandidateRemedy, CiRunId, Promoted};
//! fn forge(candidate: CandidateRemedy) {
//!     let run = CiRunId::parse("made-up-run").unwrap();
//!     let _ = Promoted::new(candidate, run);
//! }
//! ```
//!
//! ```compile_fail,E0624
//! use spot_agent_memory::{Promoted, UsableRemedy};
//! fn forge(promoted: Promoted) -> UsableRemedy {
//!     UsableRemedy::from_promoted(promoted)
//! }
//! ```
//!
//! No CI system, no clock and no model is called here: the crate decides what a candidate is and
//! what promotion requires, and the host supplies the run id.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use spot_remedies::{ParamSchema, ParamType, RemedyId, RemedyMeta, Risk, UndoKind};

use crate::error::MemoryError;
use crate::id::{CandidateId, CiRunId, Symptom};
use crate::procedure::{ParamValue, Procedure, ProcedureStep};

/// The default number of confirming incidents before a procedure may become a candidate.
pub const DEFAULT_MIN_CONFIRMATIONS: u32 = 3;

/// How much evidence a procedure needs before it may be proposed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LearningPolicy {
    /// How many resolved incidents the procedure needs.
    pub min_confirmations: u32,
    /// How confident the procedure must be.
    pub min_confidence: crate::meta::Confidence,
    /// The risk the candidate declares until its author says otherwise.
    pub risk: Risk,
}

impl Default for LearningPolicy {
    fn default() -> Self {
        Self {
            min_confirmations: DEFAULT_MIN_CONFIRMATIONS,
            min_confidence: crate::meta::Confidence::DEFAULT_MINIMUM,
            risk: Risk::Medium,
        }
    }
}

/// Where a candidate is in the CI pipeline.
///
/// The order is the pipeline's: a candidate is proposed, CI starts, CI passes or rejects, and only
/// a passing candidate is promoted.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateStatus {
    /// Written by the learning loop; nobody may use it.
    Proposed,
    /// CI is running it.
    InCi,
    /// CI ran it and every self-test passed.
    Passed,
    /// CI ran it and something failed.
    Rejected,
    /// Promoted after a passing CI run: the one status that makes a remedy usable.
    Promoted,
}

impl CandidateStatus {
    /// The stable snake_case text form, which is also the serde form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Proposed => "proposed",
            Self::InCi => "in_ci",
            Self::Passed => "passed",
            Self::Rejected => "rejected",
            Self::Promoted => "promoted",
        }
    }

    /// Whether a candidate may go from this status to `next`: the same status, or one step along
    /// the pipeline (`Proposed → InCi → Passed | Rejected`, `Passed → Promoted`). Nothing moves
    /// back, and a `Rejected` or `Promoted` candidate stays where it is.
    #[must_use]
    pub fn can_move_to(self, next: Self) -> bool {
        self == next
            || matches!(
                (self, next),
                (Self::Proposed, Self::InCi)
                    | (Self::InCi, Self::Passed | Self::Rejected)
                    | (Self::Passed, Self::Promoted)
            )
    }

    /// Whether a remedy in this status may be used at all.
    #[must_use]
    pub fn is_usable(self) -> bool {
        matches!(self, Self::Promoted)
    }
}

impl fmt::Display for CandidateStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A candidate remedy: a procedure's repeated fix, in the remedy catalogue's own format, waiting
/// for CI.
///
/// Its fields are private and every transition is a method, so a candidate cannot be moved to
/// `Promoted` by construction: only [`Promoted::new`] does that, and it requires a passing CI run
/// id.
#[derive(Clone, Debug, PartialEq)]
pub struct CandidateRemedy {
    id: CandidateId,
    remedy: RemedyId,
    meta: RemedyMeta,
    steps: Vec<ProcedureStep>,
    procedure: crate::id::ProcedureId,
    symptoms: Vec<Symptom>,
    confirmations: u32,
    status: CandidateStatus,
    ci_run: Option<CiRunId>,
    rejection: Option<String>,
}

impl CandidateRemedy {
    /// The candidate's id (its directory name in the `candidates/` queue).
    #[must_use]
    pub fn id(&self) -> &CandidateId {
        &self.id
    }

    /// The remedy id the promoted remedy will be registered under.
    #[must_use]
    pub fn remedy(&self) -> &RemedyId {
        &self.remedy
    }

    /// The remedy spec, in `spot-remedies` format.
    #[must_use]
    pub fn meta(&self) -> &RemedyMeta {
        &self.meta
    }

    /// The steps the remedy runs, in order.
    #[must_use]
    pub fn steps(&self) -> &[ProcedureStep] {
        &self.steps
    }

    /// The procedure it was learned from.
    #[must_use]
    pub fn procedure(&self) -> &crate::id::ProcedureId {
        &self.procedure
    }

    /// The symptom signature it fixes.
    #[must_use]
    pub fn symptoms(&self) -> &[Symptom] {
        &self.symptoms
    }

    /// How many resolved incidents back it.
    #[must_use]
    pub fn confirmations(&self) -> u32 {
        self.confirmations
    }

    /// Where it is in the pipeline.
    #[must_use]
    pub fn status(&self) -> CandidateStatus {
        self.status
    }

    /// The CI run that is running or has run it, when one has.
    #[must_use]
    pub fn ci_run(&self) -> Option<&CiRunId> {
        self.ci_run.as_ref()
    }

    /// Why CI rejected it, when it did.
    #[must_use]
    pub fn rejection(&self) -> Option<&str> {
        self.rejection.as_deref()
    }

    /// Starts CI on a proposed candidate.
    ///
    /// # Errors
    ///
    /// [`MemoryError::Status`] when the candidate is not `Proposed`.
    pub(crate) fn begin_ci(&mut self, run: CiRunId) -> Result<(), MemoryError> {
        self.require(CandidateStatus::Proposed, "start CI")?;
        self.status = CandidateStatus::InCi;
        self.ci_run = Some(run);
        Ok(())
    }

    /// Records that the CI run passed.
    ///
    /// # Errors
    ///
    /// [`MemoryError::Status`] when the candidate is not `InCi`, and [`MemoryError::CiRun`] when
    /// the run is not the one that started.
    pub(crate) fn pass_ci(&mut self, run: &CiRunId) -> Result<(), MemoryError> {
        self.require(CandidateStatus::InCi, "record a passing run")?;
        self.match_run(run)?;
        self.status = CandidateStatus::Passed;
        Ok(())
    }

    /// Records that the CI run failed, with the reason CI gave.
    ///
    /// # Errors
    ///
    /// [`MemoryError::Status`] when the candidate is not `InCi`, and [`MemoryError::CiRun`] when
    /// the run is not the one that started.
    pub(crate) fn reject_ci(&mut self, run: &CiRunId, reason: &str) -> Result<(), MemoryError> {
        self.require(CandidateStatus::InCi, "record a failing run")?;
        self.match_run(run)?;
        self.status = CandidateStatus::Rejected;
        self.rejection = Some(reason.to_owned());
        Ok(())
    }

    fn require(&self, required: CandidateStatus, action: &str) -> Result<(), MemoryError> {
        if self.status == required {
            Ok(())
        } else {
            Err(MemoryError::Status {
                candidate: self.id.to_string(),
                status: self.status.as_str().to_owned(),
                required: required.as_str().to_owned(),
                action: action.to_owned(),
            })
        }
    }

    fn match_run(&self, run: &CiRunId) -> Result<(), MemoryError> {
        match &self.ci_run {
            Some(started) if started == run => Ok(()),
            Some(started) => Err(MemoryError::CiRun {
                candidate: self.id.to_string(),
                reason: format!("the run `{run}` is not the run `{started}` that started it"),
            }),
            None => Err(MemoryError::CiRun {
                candidate: self.id.to_string(),
                reason: "no CI run has started for it".to_owned(),
            }),
        }
    }
}

/// A candidate CI has passed. The only route to a [`UsableRemedy`].
#[derive(Clone, Debug, PartialEq)]
pub struct Promoted {
    candidate: CandidateRemedy,
    ci_run: CiRunId,
}

impl Promoted {
    /// Promotes a **passed** candidate, recording the CI run that passed.
    ///
    /// # Errors
    ///
    /// [`MemoryError::Status`] when the candidate has not passed, and [`MemoryError::CiRun`] when
    /// the run is not the one that passed.
    pub(crate) fn new(
        mut candidate: CandidateRemedy,
        ci_run: CiRunId,
    ) -> Result<Self, MemoryError> {
        candidate.require(CandidateStatus::Passed, "promote")?;
        candidate.match_run(&ci_run)?;
        candidate.status = CandidateStatus::Promoted;
        Ok(Self { candidate, ci_run })
    }

    /// The promoted candidate.
    #[must_use]
    pub fn candidate(&self) -> &CandidateRemedy {
        &self.candidate
    }

    /// The CI run that promoted it.
    #[must_use]
    pub fn ci_run(&self) -> &CiRunId {
        &self.ci_run
    }
}

/// A remedy an unattended agent may run: a promoted candidate and nothing else.
///
/// There is no other constructor, no public field and no `Deserialize`, so an unproven candidate
/// cannot reach an unattended run.
#[derive(Clone, Debug, PartialEq)]
pub struct UsableRemedy(Promoted);

impl UsableRemedy {
    /// The one way to obtain a usable remedy, reachable only from [`PromotionPort::promote`].
    #[must_use]
    pub(crate) fn from_promoted(promoted: Promoted) -> Self {
        Self(promoted)
    }

    /// The remedy spec.
    #[must_use]
    pub fn meta(&self) -> &RemedyMeta {
        self.0.candidate.meta()
    }

    /// The CI run that proved it.
    #[must_use]
    pub fn ci_run(&self) -> &CiRunId {
        self.0.ci_run()
    }

    /// The candidate it came from.
    #[must_use]
    pub fn candidate(&self) -> &CandidateRemedy {
        self.0.candidate()
    }
}

/// What the CI system recorded for one run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CiOutcome {
    /// Every self-test passed.
    Passed,
    /// Something failed, for the reason CI gave.
    Failed(String),
}

/// The promotion seam: the CI system's own record of its runs, and the only code allowed to move
/// a candidate through CI.
///
/// A host implements [`outcome`](Self::outcome) over its CI system. The provided methods are the
/// whole pipeline: they start a run, settle a candidate **only** from what `outcome` reports for
/// its run, and promote a passed candidate, recording the run id. The candidate's transitions and
/// [`UsableRemedy`]'s constructor are crate-private, so an implementor can report what CI saw but
/// cannot make a candidate pass, or a remedy usable, any other way.
pub trait PromotionPort: Send + Sync {
    /// What CI recorded for `run`; `None` while it is unknown or still running.
    fn outcome(&self, run: &CiRunId) -> Option<CiOutcome>;

    /// Hands a proposed candidate to CI as `run`.
    ///
    /// # Errors
    ///
    /// [`MemoryError::Status`] when the candidate is not `Proposed`.
    fn start(&self, candidate: &mut CandidateRemedy, run: CiRunId) -> Result<(), MemoryError> {
        candidate.begin_ci(run)
    }

    /// Moves an `InCi` candidate to `Passed` or `Rejected` from CI's record of its run.
    ///
    /// # Errors
    ///
    /// [`MemoryError::Status`] when the candidate is not `InCi`, and [`MemoryError::CiRun`] when
    /// CI has no finished record for its run.
    fn settle(&self, candidate: &mut CandidateRemedy) -> Result<(), MemoryError> {
        candidate.require(CandidateStatus::InCi, "settle")?;
        let Some(run) = candidate.ci_run().cloned() else {
            return Err(MemoryError::CiRun {
                candidate: candidate.id().to_string(),
                reason: "no CI run has started for it".to_owned(),
            });
        };
        match self.outcome(&run) {
            Some(CiOutcome::Passed) => candidate.pass_ci(&run),
            Some(CiOutcome::Failed(reason)) => candidate.reject_ci(&run, &reason),
            None => Err(MemoryError::CiRun {
                candidate: candidate.id().to_string(),
                reason: format!("CI has no finished record of the run `{run}`"),
            }),
        }
    }

    /// Promotes a passed candidate, recording `run`, and returns the usable remedy.
    ///
    /// # Errors
    ///
    /// [`MemoryError::Status`] when the candidate has not passed, and [`MemoryError::CiRun`] when
    /// `run` is not the run that passed or CI does not record it as passed.
    fn promote(
        &self,
        candidate: CandidateRemedy,
        run: CiRunId,
    ) -> Result<UsableRemedy, MemoryError> {
        candidate.require(CandidateStatus::Passed, "promote")?;
        candidate.match_run(&run)?;
        if self.outcome(&run) != Some(CiOutcome::Passed) {
            return Err(MemoryError::CiRun {
                candidate: candidate.id().to_string(),
                reason: format!("CI does not record the run `{run}` as passed"),
            });
        }
        Ok(UsableRemedy::from_promoted(Promoted::new(candidate, run)?))
    }
}

/// The port this crate ships: CI's results are fed in with [`report`](Self::report) by the
/// host's CI integration, and promotion trusts only what was reported.
#[derive(Debug, Default)]
pub struct CiPromotion {
    runs: Mutex<BTreeMap<CiRunId, CiOutcome>>,
}

impl CiPromotion {
    /// A port that has been told nothing yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records what CI saw for `run`. A later report for the same run replaces the earlier.
    pub fn report(&self, run: CiRunId, outcome: CiOutcome) {
        self.runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(run, outcome);
    }
}

impl PromotionPort for CiPromotion {
    fn outcome(&self, run: &CiRunId) -> Option<CiOutcome> {
        self.runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(run)
            .cloned()
    }
}

/// Whether a candidate may be used. Only a promoted one may.
#[must_use]
pub fn can_use(candidate: &CandidateRemedy) -> bool {
    candidate.status().is_usable()
}

/// Turns a repeated successful procedure into a candidate remedy.
///
/// # Errors
///
/// [`MemoryError::Harmful`] when any recorded incident ended in harm,
/// [`MemoryError::Unproven`] when there are fewer confirming incidents than the policy requires,
/// and [`MemoryError::Candidate`] when the procedure cannot become a remedy (no steps, a
/// confidence below the policy, parameters that do not fit one schema).
pub fn propose_remedy(
    procedure: &Procedure,
    policy: &LearningPolicy,
) -> Result<CandidateRemedy, MemoryError> {
    if procedure.harms() > 0 {
        return Err(MemoryError::Harmful {
            procedure: procedure.id.to_string(),
            incidents: usize::try_from(procedure.harms()).unwrap_or(usize::MAX),
        });
    }
    let successes = procedure.successes();
    if successes < policy.min_confirmations {
        return Err(MemoryError::Unproven {
            procedure: procedure.id.to_string(),
            confirmations: successes,
            required: policy.min_confirmations,
        });
    }
    if !procedure.meta.confidence.is_at_least(policy.min_confidence) {
        return Err(MemoryError::Candidate {
            candidate: procedure.id.to_string(),
            reason: format!(
                "its confidence is {} basis points and the policy needs {}",
                procedure.meta.confidence.get(),
                policy.min_confidence.get()
            ),
        });
    }
    if procedure.steps.is_empty() {
        return Err(MemoryError::Candidate {
            candidate: procedure.id.to_string(),
            reason: "a remedy has at least one step".to_owned(),
        });
    }
    let params = infer_schema(&procedure.steps).map_err(|reason| MemoryError::Candidate {
        candidate: procedure.id.to_string(),
        reason,
    })?;
    let slug = remedy_slug(procedure);
    let remedy =
        RemedyId::parse(&format!("learned.{slug}")).map_err(|error| MemoryError::Candidate {
            candidate: procedure.id.to_string(),
            reason: error.to_string(),
        })?;
    let id = CandidateId::parse(&format!("candidate.{slug}")).map_err(|error| {
        MemoryError::Candidate {
            candidate: procedure.id.to_string(),
            reason: error.to_string(),
        }
    })?;
    let preconditions = format!(
        "the incident shows {} and this procedure resolved it {successes} time(s)",
        symptoms_text(procedure)
    );
    Ok(CandidateRemedy {
        id,
        remedy: remedy.clone(),
        meta: RemedyMeta {
            title: procedure.title.as_str().to_owned(),
            tier: procedure.meta.tier,
            risk: policy.risk,
            affects: Vec::new(),
            params,
            preconditions,
            undo: UndoKind::reversible("the remedy's undo restores what its steps changed"),
        },
        steps: procedure.steps.clone(),
        procedure: procedure.id.clone(),
        symptoms: procedure.symptoms.clone(),
        confirmations: successes,
        status: CandidateStatus::Proposed,
        ci_run: None,
        rejection: None,
    })
}

/// The symptom tags as one text, for the precondition sentence.
fn symptoms_text(procedure: &Procedure) -> String {
    procedure
        .symptoms
        .iter()
        .map(Symptom::as_str)
        .collect::<Vec<&str>>()
        .join(", ")
}

/// The remedy's short name: the procedure id without its `procedure.` prefix, dots to underscores.
fn remedy_slug(procedure: &Procedure) -> String {
    let id = procedure.id.as_str();
    let tail = id.strip_prefix("procedure.").unwrap_or(id);
    tail.replace('.', "_")
}

/// The parameter schema the steps imply.
///
/// A name present in every step is required; a name whose type differs between steps is an error
/// rather than a guess, because a remedy's parameters must be one shape.
fn infer_schema(steps: &[ProcedureStep]) -> Result<ParamSchema, String> {
    let mut kinds: BTreeMap<&str, ParamType> = BTreeMap::new();
    for step in steps {
        for (name, value) in &step.params {
            let kind = match value {
                ParamValue::Text(_) => ParamType::String,
                ParamValue::Integer(_) => ParamType::Integer,
                ParamValue::Bool(_) => ParamType::Boolean,
            };
            match kinds.get(name.as_str()) {
                Some(existing) if *existing != kind => {
                    return Err(format!(
                        "the parameter `{name}` is a {} in one step and a {} in another",
                        existing.json_name(),
                        kind.json_name()
                    ));
                }
                Some(_) => {}
                None => {
                    kinds.insert(name.as_str(), kind);
                }
            }
        }
    }
    let mut params = ParamSchema {
        properties: BTreeMap::new(),
        required: Vec::new(),
        additional: false,
    };
    for (name, kind) in kinds {
        params.properties.insert(name.to_owned(), kind);
        if steps.iter().all(|step| step.params.contains_key(name)) {
            params.required.push(name.to_owned());
        }
    }
    params.required.sort();
    for step in steps {
        let object: serde_json::Map<String, serde_json::Value> = step
            .params
            .iter()
            .map(|(name, value)| (name.clone(), value.to_json()))
            .collect();
        if let Err(violation) = params.validate(&serde_json::Value::Object(object)) {
            return Err(format!(
                "the parameter `{}` does not fit the schema: {}",
                violation.path, violation.detail
            ));
        }
    }
    Ok(params)
}
