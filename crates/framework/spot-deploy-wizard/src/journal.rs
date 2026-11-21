//! The progress journal: one entry per step attempt, written **before** the step runs and
//! again when it finishes, so a deployment survives a closed laptop, a dead console or a
//! failed step.
//!
//! [`Journal`] is the seam the console implements over its own durable store (SQLite, the
//! management database, a file — the console's choice). The contract is the important part:
//! [`Journal::append`] must have persisted the entry when it returns, because the wizard
//! relies on a `Started` entry surviving a crash to know which step was in flight.
//! [`MemoryJournal`] is the in-memory implementation tests and a single-screen preview use.
//!
//! [`resume`] reads a journal back into a [`ResumePoint`]: which steps are done, which step
//! to retry, and whether a non-idempotent step was interrupted and needs its clean-up first.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::error::WizardError;
use crate::plan::{DeployPlan, PlanId};

/// How far one step attempt got.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepState {
    /// The step was written down and its command is about to run, or is running.
    Started,
    /// The step's command was accepted and the step is done.
    Succeeded,
    /// The step's command was refused or failed. An idempotent step can be retried at once;
    /// a non-idempotent one needs its clean-up recorded (`RolledBack`) first.
    Failed,
    /// The step was undone during a rollback.
    RolledBack,
}

impl StepState {
    /// The stable snake-case name the console stores.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::RolledBack => "rolled_back",
        }
    }

    /// Whether the state is terminal for one attempt.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Started)
    }
}

/// One journal line: the plan, the step, the attempt, how far it got and when.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalEntry {
    /// The plan the entry belongs to.
    pub plan: PlanId,
    /// The step's index in the plan.
    pub step: usize,
    /// Which attempt at the step this is, from one.
    pub attempt: u16,
    /// How far the attempt got.
    pub state: StepState,
    /// When the entry was written, in UTC seconds since the Unix epoch. Supplied by the
    /// caller: the crate never reads a clock.
    pub at: i64,
    /// A short, secret-free note from the executor or the agent.
    #[serde(default)]
    pub detail: Option<String>,
}

impl JournalEntry {
    /// A journal line for `plan`, step `step`, attempt `attempt`, in `state` at `at`.
    #[must_use]
    pub fn new(plan: PlanId, step: usize, attempt: u16, state: StepState, at: i64) -> Self {
        Self {
            plan,
            step,
            attempt,
            state,
            at,
            detail: None,
        }
    }

    /// The same entry with a note. The note is cleaned with [`sanitize_detail`]: control
    /// characters are dropped and the text is capped, so executor output cannot flood or
    /// forge the journal.
    #[must_use]
    pub fn with_detail(mut self, detail: impl AsRef<str>) -> Self {
        self.detail = Some(sanitize_detail(detail.as_ref()));
        self
    }
}

/// The longest note, in characters, a journal entry or a failure report keeps.
pub const MAX_DETAIL_CHARS: usize = 240;

/// Cleans free text from an executor or an agent before it is stored or shown: control
/// characters (newlines, escapes, NUL) become spaces, runs of spaces collapse, and the text
/// is cut at [`MAX_DETAIL_CHARS`] characters. An executor must never put a credential in a
/// note; this bounds the damage if one does echo a long parameter, it does not detect it.
#[must_use]
pub fn sanitize_detail(text: &str) -> String {
    let mut out = String::new();
    let mut last_space = true;
    for ch in text.chars() {
        let ch = if ch.is_control() { ' ' } else { ch };
        if ch == ' ' {
            if last_space {
                continue;
            }
            last_space = true;
        } else {
            last_space = false;
        }
        out.push(ch);
        if out.chars().count() >= MAX_DETAIL_CHARS {
            break;
        }
    }
    out.trim_end().to_owned()
}

/// The durable store of journal entries.
///
/// An implementation must persist an entry before [`Journal::append`] returns, and must
/// return entries in the order they were appended.
pub trait Journal {
    /// Appends `entry`, durably.
    ///
    /// # Errors
    /// [`WizardError::JournalMismatch`] when the entry belongs to another plan;
    /// implementations may report their store's failure in the same variant's detail.
    fn append(&mut self, entry: JournalEntry) -> Result<(), WizardError>;

    /// Every entry, in append order.
    fn entries(&self) -> &[JournalEntry];
}

/// An in-memory journal, for tests and a single-screen preview.
///
/// It refuses an entry for another plan, so a misuse that would lose data in a real store
/// fails loudly here too.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryJournal {
    plan: PlanId,
    entries: Vec<JournalEntry>,
}

impl MemoryJournal {
    /// An empty journal for `plan`.
    #[must_use]
    pub fn new(plan: PlanId) -> Self {
        Self {
            plan,
            entries: Vec::new(),
        }
    }

    /// The plan the journal belongs to.
    #[must_use]
    pub fn plan(&self) -> &PlanId {
        &self.plan
    }

    /// Appends a `Started` entry for `step` at `at`, attempt `attempt`.
    ///
    /// # Errors
    /// As [`Journal::append`].
    pub fn started(&mut self, step: usize, at: i64) -> Result<u16, WizardError> {
        let attempt = self.attempts(step).saturating_add(1);
        self.append(JournalEntry::new(
            self.plan.clone(),
            step,
            attempt,
            StepState::Started,
            at,
        ))?;
        Ok(attempt)
    }

    /// Appends a `Succeeded` entry for `step` at `at`.
    ///
    /// # Errors
    /// As [`Journal::append`].
    pub fn succeeded(&mut self, step: usize, at: i64) -> Result<(), WizardError> {
        let attempt = self.attempts(step).max(1);
        self.append(JournalEntry::new(
            self.plan.clone(),
            step,
            attempt,
            StepState::Succeeded,
            at,
        ))
    }

    /// Appends a `Failed` entry for `step` at `at`.
    ///
    /// # Errors
    /// As [`Journal::append`].
    pub fn failed(&mut self, step: usize, at: i64, detail: &str) -> Result<(), WizardError> {
        let attempt = self.attempts(step).max(1);
        self.append(
            JournalEntry::new(self.plan.clone(), step, attempt, StepState::Failed, at)
                .with_detail(detail),
        )
    }

    /// Appends a `RolledBack` entry for `step` at `at`, recording that its clean-up note was
    /// carried out so the step may be retried.
    ///
    /// # Errors
    /// As [`Journal::append`].
    pub fn rolled_back(&mut self, step: usize, at: i64) -> Result<(), WizardError> {
        let attempt = self.attempts(step).max(1);
        self.append(JournalEntry::new(
            self.plan.clone(),
            step,
            attempt,
            StepState::RolledBack,
            at,
        ))
    }

    /// How many attempts the journal has for `step`.
    #[must_use]
    pub fn attempts(&self, step: usize) -> u16 {
        self.entries
            .iter()
            .filter(|entry| entry.step == step)
            .map(|entry| entry.attempt)
            .max()
            .unwrap_or(0)
    }
}

impl Journal for MemoryJournal {
    fn append(&mut self, entry: JournalEntry) -> Result<(), WizardError> {
        if entry.plan != self.plan {
            return Err(WizardError::JournalMismatch {
                detail: "the entry belongs to another plan".to_owned(),
            });
        }
        self.entries.push(entry);
        Ok(())
    }

    fn entries(&self) -> &[JournalEntry] {
        &self.entries
    }
}

/// The steps that have succeeded, checking every entry against `plan`.
///
/// # Errors
/// [`WizardError::JournalMismatch`] when an entry is for another plan or for a step the
/// plan does not have.
pub fn completed_steps(
    plan: &DeployPlan,
    journal: &dyn Journal,
) -> Result<BTreeSet<usize>, WizardError> {
    let mut latest: BTreeMap<usize, StepState> = BTreeMap::new();
    for entry in journal.entries() {
        if &entry.plan != plan.id() {
            return Err(WizardError::JournalMismatch {
                detail: "the journal belongs to another deployment plan".to_owned(),
            });
        }
        let Some(step) = plan.step(entry.step) else {
            return Err(WizardError::JournalMismatch {
                detail: format!(
                    "the journal names step {} but the plan has {} steps",
                    entry.step,
                    plan.len()
                ),
            });
        };
        if entry.state == StepState::Succeeded
            && let Some(missing) = step
                .depends_on
                .iter()
                .find(|dependency| latest.get(dependency) != Some(&StepState::Succeeded))
        {
            return Err(WizardError::JournalMismatch {
                detail: format!(
                    "the journal marks step {} done before step {missing}, which it depends on; refusing an inconsistent journal",
                    entry.step
                ),
            });
        }
        let slot = latest.entry(entry.step).or_insert(entry.state);
        *slot = entry.state;
    }
    let completed: BTreeSet<usize> = latest
        .into_iter()
        .filter(|(_, state)| *state == StepState::Succeeded)
        .map(|(step, _)| step)
        .collect();
    // A dependency undone after its dependant succeeded leaves the dependant standing on
    // something that is no longer there.
    for index in &completed {
        if let Some(missing) = plan.step(*index).and_then(|step| {
            step.depends_on
                .iter()
                .find(|dependency| !completed.contains(dependency))
        }) {
            return Err(WizardError::JournalMismatch {
                detail: format!(
                    "step {index} is recorded done but step {missing}, which it depends on, is not; refusing an inconsistent journal"
                ),
            });
        }
    }
    Ok(completed)
}

/// Where a resumed deployment picks up.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumePoint {
    /// The steps that already succeeded, in ascending order.
    pub completed: Vec<usize>,
    /// The step to run next; `None` when the plan is finished.
    pub next: Option<usize>,
    /// The step whose last attempt never finished (`None` when every attempt is accounted
    /// for). This is the step a crash interrupted or an executor refused.
    pub interrupted: Option<usize>,
    /// Whether the interrupted step is not idempotent, so its clean-up note must be carried
    /// out before it runs again.
    pub cleanup_required: bool,
}

impl ResumePoint {
    /// Whether the deployment has nothing left to do.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.next.is_none()
    }
}

/// Reads `journal` against `plan` and says where to resume.
///
/// Rules:
/// - an entry for another plan, or for a step the plan does not have, is refused;
/// - the latest attempt of a step decides whether it is done;
/// - a step whose latest attempt is `Started` (a crash) or `Failed` (a refusal) is the one
///   to retry, even when later steps somehow ran;
/// - if that step is not idempotent and was left `Started` or `Failed`, a half-applied
///   change may be on the node, so its clean-up must run (a `RolledBack` entry) first;
/// - a step recorded done while a dependency is not, or before it, makes the journal
///   inconsistent and is refused.
///
/// # Errors
/// [`WizardError::JournalMismatch`] as [`completed_steps`] reports.
pub fn resume(plan: &DeployPlan, journal: &dyn Journal) -> Result<ResumePoint, WizardError> {
    let completed = completed_steps(plan, journal)?;
    let mut latest: BTreeMap<usize, StepState> = BTreeMap::new();
    for entry in journal.entries() {
        let slot = latest.entry(entry.step).or_insert(entry.state);
        *slot = entry.state;
    }
    let interrupted = latest
        .iter()
        .filter(|(_, state)| matches!(state, StepState::Started | StepState::Failed))
        .map(|(step, _)| *step)
        .max();
    let next = interrupted.or_else(|| {
        plan.steps()
            .iter()
            .find(|step| {
                !completed.contains(&step.index)
                    && step
                        .depends_on
                        .iter()
                        .all(|dependency| completed.contains(dependency))
            })
            .map(|step| step.index)
    });
    let cleanup_required = match interrupted {
        Some(step) => {
            let state = latest.get(&step);
            let non_idempotent = plan.step(step).is_some_and(|step| !step.idempotent);
            non_idempotent && matches!(state, Some(StepState::Started | StepState::Failed))
        }
        None => false,
    };
    Ok(ResumePoint {
        completed: completed.into_iter().collect(),
        next,
        interrupted,
        cleanup_required,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::topology::{NodeSpec, Profile, Role, Topology};

    fn node(host: &str, roles: &[Role]) -> NodeSpec {
        NodeSpec::new(host, roles).unwrap()
    }

    fn reference() -> Topology {
        Topology::new(
            vec![
                node("app1.example.com", &[Role::Web, Role::Jobs, Role::Realtime]),
                node("db1.example.com", &[Role::Database]),
                node("db2.example.com", &[Role::Database]),
            ],
            Profile::ClusterBasic,
        )
        .with_web_address("erp.example.com")
    }

    #[test]
    fn a_fresh_journal_starts_at_the_first_step() {
        let plan = crate::plan::plan(&reference()).unwrap();
        let journal = MemoryJournal::new(plan.id().clone());
        let point = resume(&plan, &journal).unwrap();
        assert_eq!(point.next, Some(0));
        assert!(point.completed.is_empty());
        assert!(!point.cleanup_required);
        assert!(!point.is_finished());
    }

    #[test]
    fn a_finished_journal_has_nothing_left() {
        let plan = crate::plan::plan(&reference()).unwrap();
        let mut journal = MemoryJournal::new(plan.id().clone());
        for step in plan.steps() {
            journal.started(step.index, 1_000).unwrap();
            journal.succeeded(step.index, 1_010).unwrap();
        }
        let point = resume(&plan, &journal).unwrap();
        assert!(point.is_finished());
        assert_eq!(point.completed.len(), plan.len());
        assert_eq!(point.interrupted, None);
    }

    #[test]
    fn a_started_non_idempotent_step_needs_cleanup_before_it_retries() {
        let plan = crate::plan::plan(&reference()).unwrap();
        let mut journal = MemoryJournal::new(plan.id().clone());
        let index = plan
            .steps()
            .iter()
            .position(|step| !step.idempotent)
            .unwrap();
        journal.started(index, 1_000).unwrap();
        let point = resume(&plan, &journal).unwrap();
        assert_eq!(point.interrupted, Some(index));
        assert_eq!(point.next, Some(index));
        assert!(point.cleanup_required);
    }

    #[test]
    fn a_failed_idempotent_step_retries_without_cleanup() {
        let plan = crate::plan::plan(&reference()).unwrap();
        let mut journal = MemoryJournal::new(plan.id().clone());
        let index = plan
            .steps()
            .iter()
            .position(|step| step.idempotent)
            .unwrap();
        journal.started(index, 1_000).unwrap();
        journal.failed(index, 1_010, "service refused").unwrap();
        let point = resume(&plan, &journal).unwrap();
        assert_eq!(point.next, Some(index));
        assert_eq!(point.interrupted, Some(index));
        assert!(!point.cleanup_required);
    }

    #[test]
    fn a_foreign_journal_is_refused() {
        let plan = crate::plan::plan(&reference()).unwrap();
        let other = crate::plan::plan(
            &crate::topology::Topology::new(
                vec![
                    node("app9.example.com", &[Role::Web, Role::Jobs, Role::Realtime]),
                    node("db9.example.com", &[Role::Database]),
                ],
                Profile::ClusterBasic,
            )
            .with_web_address("erp.example.com"),
        )
        .unwrap();
        let mut journal = MemoryJournal::new(other.id().clone());
        journal.started(0, 1_000).unwrap();
        let error = resume(&plan, &journal).unwrap_err();
        assert_eq!(error.code(), crate::error::JOURNAL_MISMATCH);
    }

    #[test]
    fn an_entry_beyond_the_plan_is_refused() {
        let plan = crate::plan::plan(&reference()).unwrap();
        let mut journal = MemoryJournal::new(plan.id().clone());
        journal
            .append(JournalEntry::new(
                plan.id().clone(),
                999,
                1,
                StepState::Started,
                1_000,
            ))
            .unwrap();
        assert!(resume(&plan, &journal).is_err());
    }

    #[test]
    fn a_journal_for_another_plan_refuses_appends() {
        let mut journal = MemoryJournal::new(PlanId::from("another"));
        let error = journal
            .append(JournalEntry::new(
                PlanId::from("different"),
                0,
                1,
                StepState::Started,
                1_000,
            ))
            .unwrap_err();
        assert_eq!(error.code(), crate::error::JOURNAL_MISMATCH);
        assert!(journal.entries().is_empty());
    }

    #[test]
    fn a_step_marked_done_without_its_dependencies_is_refused() {
        let plan = crate::plan::plan(&reference()).unwrap();
        let index = plan
            .steps()
            .iter()
            .position(|step| !step.depends_on.is_empty())
            .unwrap();
        let mut journal = MemoryJournal::new(plan.id().clone());
        journal.started(index, 1_000).unwrap();
        journal.succeeded(index, 1_010).unwrap();
        let error = resume(&plan, &journal).unwrap_err();
        assert_eq!(error.code(), crate::error::JOURNAL_MISMATCH);
        assert!(completed_steps(&plan, &journal).is_err());
    }

    #[test]
    fn a_dependant_done_after_its_dependency_was_undone_is_refused() {
        let plan = crate::plan::plan(&reference()).unwrap();
        let index = plan
            .steps()
            .iter()
            .position(|step| !step.depends_on.is_empty())
            .unwrap();
        let mut journal = MemoryJournal::new(plan.id().clone());
        for step in plan.steps().iter().take(index + 1) {
            journal.started(step.index, 1_000).unwrap();
            journal.succeeded(step.index, 1_010).unwrap();
        }
        let dependency = plan.step(index).unwrap().depends_on[0];
        journal.rolled_back(dependency, 1_020).unwrap();
        assert!(resume(&plan, &journal).is_err());
    }

    #[test]
    fn a_failed_non_idempotent_step_needs_cleanup_before_it_retries() {
        let plan = crate::plan::plan(&reference()).unwrap();
        let mut journal = MemoryJournal::new(plan.id().clone());
        let index = plan
            .steps()
            .iter()
            .position(|step| !step.idempotent)
            .unwrap();
        journal.started(index, 1_000).unwrap();
        journal.failed(index, 1_010, "database refused").unwrap();
        let point = resume(&plan, &journal).unwrap();
        assert_eq!(point.interrupted, Some(index));
        assert!(point.cleanup_required);
        journal.rolled_back(index, 1_020).unwrap();
        assert!(!resume(&plan, &journal).unwrap().cleanup_required);
    }

    #[test]
    fn details_lose_control_characters_and_are_capped() {
        let entry = JournalEntry::new(PlanId::from("p"), 0, 1, StepState::Failed, 1)
            .with_detail(format!("bad\u{1b}[31m\nline\0{}", "x".repeat(1_000)));
        let detail = entry.detail.unwrap();
        assert!(detail.chars().all(|ch| !ch.is_control()));
        assert!(detail.chars().count() <= MAX_DETAIL_CHARS);
        assert!(detail.starts_with("bad [31m line x"));
    }
}
