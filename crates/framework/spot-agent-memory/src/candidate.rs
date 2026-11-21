//! The `candidates/` queue: the files a candidate ships and the queue a host reads them from.
//!
//! A candidate is a file bundle — `candidates/<candidate id>/spec.toml` (the remedy in
//! `spot-remedies` format) and `candidates/<candidate id>/scenario.toml` (the self-test scenario
//! skeleton its author fills in). Both are rendered deterministically from the candidate, so the
//! same evidence always produces the same files.
//!
//! The skeleton is a skeleton on purpose: it names the world to seed, the steps to run and the
//! invariants the test **must** prove. This crate never writes a test that passes by itself; the
//! invariants are statements for a person or a later task to implement.

use std::collections::BTreeMap;
use std::sync::Mutex;

use serde::Serialize;
use spot_remedies::UndoKind;

use crate::error::MemoryError;
use crate::id::CandidateId;
use crate::learning::{CandidateRemedy, CandidateStatus};
use crate::procedure::ParamValue;

/// What a remedy's self-test expects undo to do. Mirrors `spot-remedies`' vocabulary in a form a
/// skeleton file can be written from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UndoExpectation {
    /// Undo restores the pre-apply state.
    Restored,
    /// The remedy is irreversible and undo must refuse.
    Refused,
}

impl UndoExpectation {
    /// The stable snake_case text form, which is also the serde form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Restored => "restored",
            Self::Refused => "refused",
        }
    }
}

/// One step of a scenario skeleton: the remedy and its parameter values as text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SkeletonStep {
    /// The remedy to run.
    pub remedy: String,
    /// Its parameters, rendered.
    pub params: BTreeMap<String, String>,
}

/// The self-test scenario skeleton a candidate ships.
///
/// A skeleton, not a scenario: it names the world to seed, the steps to run and the invariants the
/// test **must** prove. A person (or a later task) fills in the fixture and the assertions; the
/// crate never invents a passing test.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ScenarioSkeleton {
    /// The remedy under test.
    pub remedy: String,
    /// Its title.
    pub title: String,
    /// The symptom signature it must fix.
    pub symptoms: Vec<String>,
    /// The invariants the self-test must assert.
    pub must_prove: Vec<String>,
    /// What undo must do.
    pub undo: UndoExpectation,
    /// The steps to run, in order. Last, so the TOML form writes its table after the values.
    pub steps: Vec<SkeletonStep>,
}

impl ScenarioSkeleton {
    /// The skeleton a candidate ships: the world it needs, the steps it runs and the invariants
    /// its self-test must prove.
    #[must_use]
    pub fn of(candidate: &CandidateRemedy) -> Self {
        let mut must_prove = vec![
            "the dry run reports the changes the apply will make".to_owned(),
            "the apply leaves the seeded incident's symptom resolved".to_owned(),
            "no submitted financial record is edited: the hard floor holds".to_owned(),
        ];
        for symptom in candidate.symptoms() {
            must_prove.push(format!("the symptom `{symptom}` is gone after the apply"));
        }
        must_prove.push(format!(
            "undo {}",
            match candidate.meta().undo {
                UndoKind::Reversible { .. } => "restores the state the apply changed",
                UndoKind::Irreversible { .. } => "is refused and the change stands",
            }
        ));
        Self {
            remedy: candidate.remedy().to_string(),
            title: candidate.meta().title.clone(),
            symptoms: candidate
                .symptoms()
                .iter()
                .map(|symptom| symptom.as_str().to_owned())
                .collect(),
            steps: candidate
                .steps()
                .iter()
                .map(|step| SkeletonStep {
                    remedy: step.remedy.to_string(),
                    params: step
                        .params
                        .iter()
                        .map(|(name, value)| (name.clone(), render_param(value)))
                        .collect(),
                })
                .collect(),
            must_prove,
            undo: UndoExpectation::Restored,
        }
    }
}

/// A parameter value rendered as text, for the scenario skeleton's TOML.
fn render_param(value: &ParamValue) -> String {
    match value {
        ParamValue::Text(summary) => summary.as_str().to_owned(),
        ParamValue::Integer(number) => number.to_string(),
        ParamValue::Bool(flag) => flag.to_string(),
    }
}

/// The files a candidate ships: its spec and its self-test scenario skeleton.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CandidateBundle {
    /// The queue directory (`candidates/<candidate id>`).
    pub dir: String,
    /// The remedy spec, in TOML.
    pub spec_toml: String,
    /// The self-test scenario skeleton, in TOML.
    pub scenario_toml: String,
}

impl CandidateBundle {
    /// The bundle's files as `(path, contents)`, spec first.
    #[must_use]
    pub fn files(&self) -> Vec<(String, &str)> {
        vec![
            (format!("{}/spec.toml", self.dir), self.spec_toml.as_str()),
            (
                format!("{}/scenario.toml", self.dir),
                self.scenario_toml.as_str(),
            ),
        ]
    }
}

/// A queue of candidates: what a host lists, reads and hands to CI.
pub trait CandidateQueue: Send + Sync {
    /// Adds a `Proposed` candidate and returns the bundle that was written for it.
    ///
    /// # Errors
    ///
    /// [`MemoryError::Status`] when the candidate is not `Proposed`, and
    /// [`MemoryError::Candidate`] when its id is already queued.
    fn enqueue(&self, candidate: CandidateRemedy) -> Result<CandidateBundle, MemoryError>;

    /// Replaces a queued candidate (after a CI transition), keeping its bundle.
    ///
    /// # Errors
    ///
    /// [`MemoryError::Candidate`] when no candidate of that id is queued, or when the new
    /// status is not the queued one or a step forward from it (a promoted candidate can never be overwritten by an earlier-status value).
    fn replace(&self, candidate: CandidateRemedy) -> Result<(), MemoryError>;

    /// The candidate with `id`, if it is queued.
    fn get(&self, id: &CandidateId) -> Option<CandidateRemedy>;

    /// Every queued candidate, in id order.
    fn list(&self) -> Vec<CandidateRemedy>;

    /// The bundle written for `id`, if it is queued.
    fn bundle(&self, id: &CandidateId) -> Option<CandidateBundle>;
}

/// An in-memory `candidates/` queue: deterministic, no filesystem, no clock.
#[derive(Debug, Default)]
pub struct InMemoryCandidateQueue {
    entries: Mutex<BTreeMap<String, (CandidateRemedy, CandidateBundle)>>,
}

impl InMemoryCandidateQueue {
    /// An empty queue.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many candidates are queued.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether the queue is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    fn lock(
        &self,
    ) -> std::sync::MutexGuard<'_, BTreeMap<String, (CandidateRemedy, CandidateBundle)>> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl CandidateQueue for InMemoryCandidateQueue {
    fn enqueue(&self, candidate: CandidateRemedy) -> Result<CandidateBundle, MemoryError> {
        if candidate.status() != CandidateStatus::Proposed {
            return Err(MemoryError::Status {
                candidate: candidate.id().to_string(),
                status: candidate.status().as_str().to_owned(),
                required: CandidateStatus::Proposed.as_str().to_owned(),
                action: "queue".to_owned(),
            });
        }
        let key = candidate.id().as_str().to_owned();
        let mut entries = self.lock();
        if entries.contains_key(&key) {
            return Err(MemoryError::Candidate {
                candidate: key,
                reason: "a candidate of that id is already queued".to_owned(),
            });
        }
        let bundle = render_bundle(&candidate)?;
        entries.insert(key, (candidate, bundle.clone()));
        Ok(bundle)
    }

    fn replace(&self, candidate: CandidateRemedy) -> Result<(), MemoryError> {
        let key = candidate.id().as_str().to_owned();
        let mut entries = self.lock();
        match entries.get_mut(&key) {
            Some(slot) => {
                if !slot.0.status().can_move_to(candidate.status()) {
                    return Err(MemoryError::Candidate {
                        reason: format!(
                            "its status cannot go from {} to {}",
                            slot.0.status(),
                            candidate.status()
                        ),
                        candidate: key,
                    });
                }
                slot.0 = candidate;
                Ok(())
            }
            None => Err(MemoryError::Candidate {
                candidate: key,
                reason: "no candidate of that id is queued".to_owned(),
            }),
        }
    }

    fn get(&self, id: &CandidateId) -> Option<CandidateRemedy> {
        self.lock()
            .get(id.as_str())
            .map(|(candidate, _)| candidate.clone())
    }

    fn list(&self) -> Vec<CandidateRemedy> {
        self.lock()
            .values()
            .map(|(candidate, _)| candidate.clone())
            .collect()
    }

    fn bundle(&self, id: &CandidateId) -> Option<CandidateBundle> {
        self.lock()
            .get(id.as_str())
            .map(|(_, bundle)| bundle.clone())
    }
}

/// Renders a candidate's bundle: the spec and the scenario skeleton, both deterministic TOML.
fn render_bundle(candidate: &CandidateRemedy) -> Result<CandidateBundle, MemoryError> {
    let dir = format!("candidates/{}", candidate.id());
    let refuse = |error: toml::ser::Error| MemoryError::Bundle {
        candidate: candidate.id().to_string(),
        detail: error.to_string(),
    };
    let spec = SpecFile {
        candidate: candidate.id().to_string(),
        procedure: candidate.procedure().to_string(),
        confirmations: candidate.confirmations(),
        status: candidate.status().as_str().to_owned(),
        remedy: candidate.remedy().to_string(),
        title: candidate.meta().title.clone(),
        tier: candidate.meta().tier.as_str().to_owned(),
        risk: candidate.meta().risk.as_str().to_owned(),
        affects: candidate
            .meta()
            .affects
            .iter()
            .map(|doctype| doctype.as_str().to_owned())
            .collect(),
        params: candidate
            .meta()
            .params
            .properties
            .iter()
            .map(|(name, kind)| (name.clone(), kind.json_name().to_owned()))
            .collect(),
        required: candidate.meta().params.required.clone(),
        preconditions: candidate.meta().preconditions.clone(),
        undo: UndoFile::of(&candidate.meta().undo),
    };
    let skeleton = ScenarioSkeleton::of(candidate);
    Ok(CandidateBundle {
        dir,
        spec_toml: toml::to_string(&spec).map_err(refuse)?,
        scenario_toml: toml::to_string(&skeleton).map_err(refuse)?,
    })
}

/// The spec file as TOML: the remedy's metadata in `spot-remedies` terms.
#[derive(Serialize)]
struct SpecFile {
    candidate: String,
    procedure: String,
    confirmations: u32,
    status: String,
    remedy: String,
    title: String,
    tier: String,
    risk: String,
    affects: Vec<String>,
    required: Vec<String>,
    preconditions: String,
    /// A table, so it stays after the values above.
    params: BTreeMap<String, String>,
    /// A table, so it stays last.
    undo: UndoFile,
}

/// How undo works, written as a plain table so a hand-edited file reads naturally.
#[derive(Serialize)]
struct UndoFile {
    kind: String,
    detail: String,
}

impl UndoFile {
    fn of(undo: &UndoKind) -> Self {
        match undo {
            UndoKind::Reversible { detail } => Self {
                kind: "reversible".to_owned(),
                detail: detail.clone(),
            },
            UndoKind::Irreversible { reason } => Self {
                kind: "irreversible".to_owned(),
                detail: reason.clone(),
            },
        }
    }
}
