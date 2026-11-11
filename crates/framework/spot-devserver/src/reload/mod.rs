//! The reload pipeline: one [`Step`] per change class, and the [`Reloader`] that runs them.
//!
//! Every step builds its result off to the side and only swaps it in when it is good. The
//! metadata step is the clearest case: it calls `spot-meta-live`, which builds and validates the
//! whole new snapshot **before** it moves the pointer, so a change that does not compile leaves
//! the previous version serving. The other steps follow the same rule — a bad view, a script
//! with a diagnostic, or a component that does not load changes nothing the client can see.
//!
//! Each step returns a [`StepOutcome`]; the [`Reloader`] wraps it in a [`ReloadRecord`] with the
//! duration the injected [`Clock`](spot_core::Clock) measured, keeps it in a bounded history and
//! hands it to the [`ReloadNotifier`] (the realtime bus, in the running server).

pub mod components;
pub mod meta;
pub mod scripts;
pub mod views;

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

pub use components::{
    BuildCommand, BuildOutput, BuildRunner, CommandBuildRunner, ComponentHost, ComponentStep,
    InMemoryComponentHost, WasmHostComponentHost,
};
pub use meta::{LiveDefinitions, MetaStep, load_definitions};
pub use scripts::{
    DeclarationNameChecker, DtsProvider, InMemoryScriptHost, LiveDtsProvider, ScriptHost,
    ScriptStep, TypeChecker,
};
pub use views::{DefinitionsSource, ViewStep};

use spot_core::Clock;

use crate::classify::{Change, ChangeKind};
use crate::error::DevError;
use crate::publish::ReloadNotifier;

/// Which step a change belongs to.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum StepName {
    /// Metadata rebuild and swap.
    Metadata,
    /// View parse and validation.
    Views,
    /// Script transpile, type-check and swap.
    Scripts,
    /// Build and component reload.
    Components,
    /// Fixture reseed.
    Fixtures,
    /// Asset change (notification only).
    Assets,
    /// A change no step handles.
    Unknown,
}

impl StepName {
    /// The lowercase name used in logs and status JSON.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Metadata => "metadata",
            Self::Views => "views",
            Self::Scripts => "scripts",
            Self::Components => "components",
            Self::Fixtures => "fixtures",
            Self::Assets => "assets",
            Self::Unknown => "unknown",
        }
    }

    /// The step a change kind maps to, or [`StepName::Unknown`].
    #[must_use]
    pub fn of(kind: ChangeKind) -> Self {
        kind.step().unwrap_or(Self::Unknown)
    }
}

/// What the running server tells a step about the change batch.
pub struct ReloadContext<'a> {
    /// The changes assigned to this step, in deterministic order.
    pub changes: &'a [Change],
    /// The monotonic time the step started, from the injected clock.
    pub now: Duration,
}

/// The result of one step, success or failure. A step never panics and never propagates an
/// error: a failed change is a normal outcome the server reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepOutcome {
    /// Whether the change was applied (or the step was a no-op that is safe to report as done).
    pub ok: bool,
    /// One sentence for the developer.
    pub detail: String,
    /// Every problem found, already rendered; empty on success.
    pub errors: Vec<String>,
    /// The new metadata version, when the step minted one.
    pub version: Option<u64>,
    /// Artefacts the step produced, for the status page (for example the new component hash).
    pub artifacts: Vec<String>,
}

impl StepOutcome {
    /// A successful outcome.
    #[must_use]
    pub fn ok(detail: impl Into<String>) -> Self {
        Self {
            ok: true,
            detail: detail.into(),
            errors: Vec::new(),
            version: None,
            artifacts: Vec::new(),
        }
    }

    /// A failed outcome with every problem.
    #[must_use]
    pub fn failed(detail: impl Into<String>, errors: Vec<String>) -> Self {
        Self {
            ok: false,
            detail: detail.into(),
            errors,
            version: None,
            artifacts: Vec::new(),
        }
    }

    /// Attaches the metadata version the step minted.
    #[must_use]
    pub fn with_version(mut self, version: u64) -> Self {
        self.version = Some(version);
        self
    }

    /// Attaches an artefact name (a hash, a path).
    #[must_use]
    pub fn with_artifact(mut self, artifact: impl Into<String>) -> Self {
        self.artifacts.push(artifact.into());
        self
    }
}

/// One reload step. Implementations are shared across the server's threads.
pub trait Step: Send + Sync {
    /// Which step this is.
    fn name(&self) -> StepName;

    /// Runs the step for `ctx`. Never fails: a problem is a [`StepOutcome::failed`].
    fn reload(&self, ctx: &ReloadContext<'_>) -> StepOutcome;
}

/// One recorded reload: the step, the paths, the outcome and how long it took.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReloadRecord {
    /// The step that ran.
    pub step: StepName,
    /// The primary change class (the batch's first change).
    pub kind: ChangeKind,
    /// The paths the batch named.
    pub paths: Vec<String>,
    /// Whether the change reached the runtime.
    pub ok: bool,
    /// One sentence for the developer.
    pub detail: String,
    /// Every problem found; empty on success.
    pub errors: Vec<String>,
    /// The new metadata version, when the step minted one.
    pub version: Option<u64>,
    /// Artefacts the step produced.
    pub artifacts: Vec<String>,
    /// The monotonic time the batch started, from the injected clock.
    pub started_at_ms: u64,
    /// How long the step took, in whole milliseconds.
    pub duration_ms: u64,
}

impl ReloadRecord {
    /// The stable error code a failed record reports, or `None` on success.
    #[must_use]
    pub fn error_code(&self) -> Option<&'static str> {
        if self.ok {
            return None;
        }
        match self.step {
            StepName::Metadata => Some("devserver.metadata"),
            StepName::Views => Some("devserver.view"),
            StepName::Scripts => Some("devserver.script"),
            StepName::Components => Some("devserver.component"),
            StepName::Unknown => Some("devserver.unknown_step"),
            StepName::Fixtures | StepName::Assets => Some("devserver.site"),
        }
    }
}

/// The outcome of a whole change batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReloadSummary {
    /// One record per step that ran, in step order.
    pub records: Vec<ReloadRecord>,
}

impl ReloadSummary {
    /// Whether every step in the batch succeeded.
    #[must_use]
    pub fn all_ok(&self) -> bool {
        self.records.iter().all(|record| record.ok)
    }

    /// How many steps failed.
    #[must_use]
    pub fn failures(&self) -> usize {
        self.records.iter().filter(|record| !record.ok).count()
    }
}

/// Runs the steps for a batch of changes, keeps a bounded history and notifies the bus.
pub struct Reloader {
    steps: BTreeMap<StepName, Arc<dyn Step>>,
    history: Mutex<VecDeque<ReloadRecord>>,
    capacity: usize,
    notifier: Option<Arc<dyn ReloadNotifier>>,
}

impl Reloader {
    /// A reloader that keeps the last `capacity` records.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            steps: BTreeMap::new(),
            history: Mutex::new(VecDeque::new()),
            capacity: capacity.max(1),
            notifier: None,
        }
    }

    /// Registers `step`, replacing any step of the same name.
    #[must_use]
    pub fn with_step(mut self, step: Arc<dyn Step>) -> Self {
        self.steps.insert(step.name(), step);
        self
    }

    /// Publishes every record on `notifier`.
    #[must_use]
    pub fn with_notifier(mut self, notifier: Arc<dyn ReloadNotifier>) -> Self {
        self.notifier = Some(notifier);
        self
    }

    /// Runs the pipeline for `changes`, grouping them by step. Records are appended to the
    /// history and published on the bus; a batch with no changes runs nothing.
    pub fn reload(&self, changes: &[Change], clock: &dyn Clock) -> ReloadSummary {
        let mut groups: BTreeMap<StepName, Vec<Change>> = BTreeMap::new();
        for change in changes {
            groups
                .entry(StepName::of(change.kind))
                .or_default()
                .push(change.clone());
        }
        let mut records = Vec::new();
        for (step_name, batch) in groups {
            let started_at = clock.monotonic();
            let outcome = match self.steps.get(&step_name) {
                Some(step) => step.reload(&ReloadContext {
                    changes: &batch,
                    now: started_at,
                }),
                None => StepOutcome::failed(
                    format!("no reload step handles `{}` changes", step_name.as_str()),
                    vec![format!(
                        "{}: a `{}` change has no step; check the classifier and the registered steps",
                        DevError::UnknownStep {
                            kind: step_name.as_str().into()
                        }
                        .code(),
                        step_name.as_str()
                    )],
                ),
            };
            let elapsed = clock.monotonic().saturating_sub(started_at);
            let record = ReloadRecord {
                step: step_name,
                kind: batch
                    .first()
                    .map_or(ChangeKind::Other, |change| change.kind),
                paths: batch
                    .iter()
                    .map(|change| change.relative.to_string())
                    .collect(),
                ok: outcome.ok,
                detail: outcome.detail,
                errors: outcome.errors,
                version: outcome.version,
                artifacts: outcome.artifacts,
                started_at_ms: millis(started_at),
                duration_ms: millis(elapsed),
            };
            if let Some(notifier) = &self.notifier {
                notifier.reloaded(&record);
            }
            self.lock().push_back(record.clone());
            while self.history_len() > self.capacity {
                self.lock().pop_front();
            }
            records.push(record);
        }
        ReloadSummary { records }
    }

    /// The recorded history, oldest first.
    #[must_use]
    pub fn history(&self) -> Vec<ReloadRecord> {
        self.lock().iter().cloned().collect()
    }

    /// The most recent failed record, when the last failure has not been superseded.
    #[must_use]
    pub fn last_error(&self) -> Option<ReloadRecord> {
        self.lock().iter().rev().find(|record| !record.ok).cloned()
    }

    fn history_len(&self) -> usize {
        self.lock().len()
    }

    fn lock(&self) -> MutexGuard<'_, VecDeque<ReloadRecord>> {
        // The guarded data is plain records; a panic elsewhere cannot leave it inconsistent.
        self.history
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// The file a change names, resolved against the step's directory when the event path is
/// relative: the event's absolute path, then the root-relative path, then the file name under
/// `dir`.
#[must_use]
pub fn change_file(dir: &camino::Utf8Path, change: &Change) -> camino::Utf8PathBuf {
    if change.path.is_file() {
        return change.path.clone();
    }
    if change.relative.is_file() {
        return change.relative.clone();
    }
    match change.relative.file_name() {
        Some(name) => dir.join(name),
        None => dir.join(&change.relative),
    }
}

/// Whole milliseconds of `duration`, saturating (no floats anywhere).
#[must_use]
pub fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    struct Fixed(StepName, bool);

    impl Step for Fixed {
        fn name(&self) -> StepName {
            self.0
        }

        fn reload(&self, _ctx: &ReloadContext<'_>) -> StepOutcome {
            if self.1 {
                StepOutcome::ok("done")
            } else {
                StepOutcome::failed("broken", vec!["devserver.metadata: broken".into()])
            }
        }
    }

    #[test]
    fn unhandled_change_is_reported_not_guessed() {
        let clock = spot_core::ManualClock::new(time_start());
        let reloader = Reloader::new(8);
        let change = Change {
            path: "/scratch/dev/README".into(),
            relative: "README".into(),
            op: crate::watch::Op::Modified,
            kind: ChangeKind::Other,
        };
        let summary = reloader.reload(&[change], &clock);
        assert_eq!(summary.records.len(), 1);
        assert!(!summary.records[0].ok);
        assert_eq!(summary.records[0].step, StepName::Unknown);
        assert_eq!(
            summary.records[0].error_code(),
            Some("devserver.unknown_step")
        );
    }

    #[test]
    fn history_is_bounded_and_last_error_is_reported() {
        let clock = spot_core::ManualClock::new(time_start());
        let reloader = Reloader::new(1)
            .with_step(Arc::new(Fixed(StepName::Metadata, true)))
            .with_step(Arc::new(Fixed(StepName::Views, false)));
        let change = |kind| Change {
            path: "/scratch/dev/views/x.toml".into(),
            relative: "views/x.toml".into(),
            op: crate::watch::Op::Modified,
            kind,
        };
        reloader.reload(&[change(ChangeKind::Metadata)], &clock);
        reloader.reload(&[change(ChangeKind::View)], &clock);
        assert_eq!(reloader.history().len(), 1);
        assert_eq!(reloader.last_error().unwrap().step, StepName::Views);
    }

    fn time_start() -> time::OffsetDateTime {
        // The reloader never reads the wall clock; the manual clock needs a start value only.
        time::OffsetDateTime::UNIX_EPOCH
    }
}
