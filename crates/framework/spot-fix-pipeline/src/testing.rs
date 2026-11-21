//! In-memory doubles for the pipeline's ports (feature `testing`).
//!
//! [`FakeWorld`] is a map of typed targets to integer record values. Its [`Fingerprint`] is one
//! row per target, so a change moves exactly one measure and the residue check is exact. The
//! doubles let a test kill a run at any boundary, restart it and prove the final state is either
//! fully verified or byte-identical to the pre-state.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use spot_agent_tiers::Approval;
use spot_rehearsal::{
    Dimension, Fingerprint, FingerprintRow, FingerprintSection, InvariantReport,
    InvariantSetResult, InvariantViolation,
};

use crate::engine::FixContext;
use crate::error::{FixError, Result};
use crate::model::{
    AgentRef, ChangeRecord, FixId, FixPlan, FixStep, PreImage, RehearsalEvidence, SnapshotRef,
    VerifyReport,
};
use crate::ports::{
    ApprovalPort, FixExecutor, FixVerifier, PreImageStore, RehearsalPort, SnapshotPort, StepChange,
};

/// A deterministic world of integer record values.
#[derive(Clone, Debug, Default)]
pub struct FakeWorld {
    values: std::sync::Arc<Mutex<BTreeMap<String, i128>>>,
}

impl FakeWorld {
    /// An empty world.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A world with one value per `(target, value)` pair.
    #[must_use]
    pub fn with(entries: &[(&str, i128)]) -> Self {
        let world = Self::new();
        for (target, value) in entries {
            let _ = world.set(target, *value);
        }
        world
    }

    /// Sets one target.
    ///
    /// # Errors
    ///
    /// [`FixError::Internal`] when the lock is poisoned.
    pub fn set(&self, target: &str, value: i128) -> Result<()> {
        self.lock()?.insert(target.to_owned(), value);
        Ok(())
    }

    /// Reads one target (`0` when absent).
    ///
    /// # Errors
    ///
    /// [`FixError::Internal`] when the lock is poisoned.
    pub fn get(&self, target: &str) -> Result<i128> {
        Ok(self.lock()?.get(target).copied().unwrap_or(0))
    }

    /// The whole world.
    ///
    /// # Errors
    ///
    /// [`FixError::Internal`] when the lock is poisoned.
    pub fn snapshot(&self) -> Result<BTreeMap<String, i128>> {
        Ok(self.lock()?.clone())
    }

    /// Replaces the whole world.
    ///
    /// # Errors
    ///
    /// [`FixError::Internal`] when the lock is poisoned.
    pub fn restore(&self, state: &BTreeMap<String, i128>) -> Result<()> {
        *self.lock()? = state.clone();
        Ok(())
    }

    /// The state fingerprint: one `records` section with one row per target.
    ///
    /// # Errors
    ///
    /// [`FixError::Internal`] when the lock is poisoned.
    pub fn fingerprint(&self) -> Result<Fingerprint> {
        let state = self.lock()?;
        let rows = state
            .iter()
            .map(|(target, value)| {
                FingerprintRow::new(
                    vec![Dimension::new("target", target.clone())],
                    BTreeMap::from([("value".to_owned(), *value)]),
                )
            })
            .collect();
        Ok(Fingerprint::new(vec![FingerprintSection::new(
            "records",
            "Record values",
            vec!["target".to_owned()],
            vec!["value".to_owned()],
            rows,
        )]))
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, BTreeMap<String, i128>>> {
        self.values.lock().map_err(|_| FixError::Internal {
            detail: "the fake world lock is poisoned".to_owned(),
        })
    }
}

/// The logical pre-image store over a [`FakeWorld`].
#[derive(Clone, Debug)]
pub struct FakePreImages {
    /// The world the images are read from and restored into.
    pub world: FakeWorld,
}

impl FakePreImages {
    /// A pre-image store over `world`.
    #[must_use]
    pub fn new(world: &FakeWorld) -> Self {
        Self {
            world: world.clone(),
        }
    }
}

impl PreImageStore for FakePreImages {
    fn capture(&mut self, targets: &[String]) -> Result<Vec<PreImage>> {
        targets
            .iter()
            .map(|target| {
                let value = self.world.get(target)?;
                Ok(PreImage::new(
                    target,
                    &value.to_string(),
                    serde_json::Value::String(value.to_string()),
                ))
            })
            .collect()
    }

    fn restore(&mut self, image: &PreImage) -> Result<()> {
        let malformed = || FixError::PreImageFailed {
            detail: format!("the pre-image of `{}` is not a whole number", image.target),
        };
        let value = match &image.value {
            serde_json::Value::String(text) => text.parse::<i128>().map_err(|_| malformed())?,
            other => i128::from(other.as_i64().ok_or_else(malformed)?),
        };
        self.world.set(&image.target, value)
    }

    fn digest(&mut self, target: &str) -> Result<String> {
        Ok(self.world.get(target)?.to_string())
    }
}

/// The snapshot and maintenance-gate port over a [`FakeWorld`].
#[derive(Clone, Debug)]
pub struct FakeSnapshots {
    /// The world the snapshots are taken from and restored into.
    pub world: FakeWorld,
    saved: std::sync::Arc<Mutex<Option<BTreeMap<String, i128>>>>,
    gate: std::sync::Arc<Mutex<bool>>,
    counter: std::sync::Arc<AtomicU64>,
}

impl FakeSnapshots {
    /// A snapshot port over `world`.
    #[must_use]
    pub fn new(world: &FakeWorld) -> Self {
        Self {
            world: world.clone(),
            saved: std::sync::Arc::new(Mutex::new(None)),
            gate: std::sync::Arc::new(Mutex::new(false)),
            counter: std::sync::Arc::new(AtomicU64::new(1)),
        }
    }

    /// Whether the maintenance gate is closed.
    #[must_use]
    pub fn gate_closed(&self) -> bool {
        self.gate.lock().map(|gate| *gate).unwrap_or(false)
    }

    /// Whether a snapshot is stored.
    #[must_use]
    pub fn has_snapshot(&self) -> bool {
        self.saved
            .lock()
            .map(|saved| saved.is_some())
            .unwrap_or(false)
    }
}

impl SnapshotPort for FakeSnapshots {
    fn take(&mut self, _fix: &FixId) -> Result<SnapshotRef> {
        let state = self.world.snapshot()?;
        let id = self.counter.fetch_add(1, Ordering::SeqCst);
        *self.saved.lock().map_err(|_| FixError::SnapshotFailed {
            detail: "the fake snapshot lock is poisoned".to_owned(),
        })? = Some(state);
        Ok(SnapshotRef::new(&format!("snap-{id}"), "digest"))
    }

    fn restore(&mut self, _snapshot: &SnapshotRef) -> Result<()> {
        let saved = self
            .saved
            .lock()
            .map_err(|_| FixError::SnapshotFailed {
                detail: "the fake snapshot lock is poisoned".to_owned(),
            })?
            .clone()
            .ok_or_else(|| FixError::SnapshotFailed {
                detail: "no snapshot was taken".to_owned(),
            })?;
        self.world.restore(&saved)
    }

    fn close_gate(&mut self, _reason: &str) -> Result<()> {
        *self.gate.lock().map_err(|_| FixError::SnapshotFailed {
            detail: "the fake gate lock is poisoned".to_owned(),
        })? = true;
        Ok(())
    }

    fn open_gate(&mut self) -> Result<()> {
        *self.gate.lock().map_err(|_| FixError::SnapshotFailed {
            detail: "the fake gate lock is poisoned".to_owned(),
        })? = false;
        Ok(())
    }
}

/// The step executor over a [`FakeWorld`].
#[derive(Clone, Debug)]
pub struct FakeExecutor {
    /// The world the steps change.
    pub world: FakeWorld,
    /// The step index that fails instead of running.
    pub fail_at: Option<usize>,
    /// How much each step adds to its target.
    pub delta: i128,
}

impl FakeExecutor {
    /// An executor that adds `delta` to each step's target.
    #[must_use]
    pub fn new(world: &FakeWorld, delta: i128) -> Self {
        Self {
            world: world.clone(),
            fail_at: None,
            delta,
        }
    }

    /// The executor that fails at `index`.
    #[must_use]
    pub fn failing_at(mut self, index: usize) -> Self {
        self.fail_at = Some(index);
        self
    }
}

impl FixExecutor for FakeExecutor {
    fn run_step(&mut self, step: &FixStep, _idempotency_key: &str) -> Result<StepChange> {
        if self.fail_at == Some(step.index) {
            return Err(FixError::StepFailed {
                index: step.index,
                id: step.id.clone(),
                detail: "injected step failure".to_owned(),
            });
        }
        let before = self.world.get(&step.target)?;
        let after = before.saturating_add(self.delta);
        self.world.set(&step.target, after)?;
        Ok(StepChange {
            before: before.to_string(),
            after: after.to_string(),
        })
    }

    fn undo_step(&mut self, _step: &FixStep, change: &ChangeRecord) -> Result<()> {
        let before = change
            .before
            .parse::<i128>()
            .map_err(|_| FixError::Internal {
                detail: "the change's before digest is not an integer".to_owned(),
            })?;
        self.world.set(&change.target, before)
    }
}

/// The verifier over a [`FakeWorld`].
#[derive(Clone, Debug)]
pub struct FakeVerifier {
    /// The world it measures.
    pub world: FakeWorld,
    /// Whether the invariant sets hold.
    pub invariants_pass: bool,
    /// Whether the remedy-specific verification passes.
    pub remedy_verified: bool,
    /// Whether the symptom cleared.
    pub symptom_cleared: bool,
}

impl FakeVerifier {
    /// A verifier where every check passes.
    #[must_use]
    pub fn passing(world: &FakeWorld) -> Self {
        Self {
            world: world.clone(),
            invariants_pass: true,
            remedy_verified: true,
            symptom_cleared: true,
        }
    }

    /// A verifier whose symptom re-check fails.
    #[must_use]
    pub fn symptom_fails(world: &FakeWorld) -> Self {
        Self {
            symptom_cleared: false,
            ..Self::passing(world)
        }
    }

    /// A verifier whose invariants fail.
    #[must_use]
    pub fn invariants_fail(world: &FakeWorld) -> Self {
        Self {
            invariants_pass: false,
            ..Self::passing(world)
        }
    }
}

impl FixVerifier for FakeVerifier {
    fn verify(&mut self, _plan: &FixPlan, _changes: &[ChangeRecord]) -> Result<VerifyReport> {
        Ok(VerifyReport {
            invariants: report(self.invariants_pass),
            remedy_verified: self.remedy_verified,
            symptom_cleared: self.symptom_cleared,
            violations: Vec::new(),
            fingerprint: self.world.fingerprint()?,
        })
    }

    fn invariants(&mut self) -> Result<InvariantReport> {
        Ok(report(self.invariants_pass))
    }

    fn fingerprint(&mut self) -> Result<Fingerprint> {
        self.world.fingerprint()
    }
}

/// A rehearsal that passes or fails without changing the world.
#[derive(Clone, Debug)]
pub struct FakeRehearsal {
    /// The world the clone fingerprint is measured on.
    pub world: FakeWorld,
    /// Whether the rehearsal passes.
    pub pass: bool,
}

impl FakeRehearsal {
    /// A passing rehearsal.
    #[must_use]
    pub fn passing(world: &FakeWorld) -> Self {
        Self {
            world: world.clone(),
            pass: true,
        }
    }

    /// A failing rehearsal.
    #[must_use]
    pub fn failing(world: &FakeWorld) -> Self {
        Self {
            world: world.clone(),
            pass: false,
        }
    }
}

impl RehearsalPort for FakeRehearsal {
    fn rehearse(&mut self, _plan: &FixPlan) -> Result<RehearsalEvidence> {
        let fingerprint = self.world.fingerprint()?;
        Ok(RehearsalEvidence {
            pre_digest: fingerprint.digest(),
            post_digest: fingerprint.digest(),
            fingerprint,
            invariants: report(self.pass),
            passed: self.pass,
            detail: if self.pass {
                "the clone rehearsal passed".to_owned()
            } else {
                "the clone rehearsal failed".to_owned()
            },
        })
    }
}

/// An approval port with a fixed answer.
#[derive(Clone, Debug)]
pub struct FakeApproval {
    /// The answer; `None` refuses.
    pub answer: Option<Approval>,
    /// How many requests were made.
    pub requests: std::sync::Arc<AtomicU64>,
}

impl FakeApproval {
    /// An approval port that approves as `approver`.
    #[must_use]
    pub fn approving(approver: &str) -> Self {
        Self {
            answer: Some(Approval {
                approver: approver.to_owned(),
                reference: "BPM-1".to_owned(),
                at: 0,
            }),
            requests: std::sync::Arc::new(AtomicU64::new(0)),
        }
    }

    /// An approval port that refuses.
    #[must_use]
    pub fn refusing() -> Self {
        Self {
            answer: None,
            requests: std::sync::Arc::new(AtomicU64::new(0)),
        }
    }

    /// How many requests the port received.
    #[must_use]
    pub fn requests(&self) -> u64 {
        self.requests.load(Ordering::SeqCst)
    }
}

impl ApprovalPort for FakeApproval {
    fn request(
        &mut self,
        _fix: &FixId,
        _plan: &FixPlan,
        _agent: &AgentRef,
        _reason: &str,
    ) -> Result<Option<Approval>> {
        self.requests.fetch_add(1, Ordering::SeqCst);
        Ok(self.answer.clone())
    }
}

/// A context for tests.
#[must_use]
pub fn context(site: &str, tier: spot_agent_tiers::Tier, deep: bool, now: i64) -> FixContext {
    FixContext::new(
        site,
        AgentRef::new("agent-support", tier, deep),
        crate::model::DeploymentEpoch::new(7),
        now,
    )
}

/// An invariant report with one set.
fn report(passed: bool) -> InvariantReport {
    let violations = if passed {
        Vec::new()
    } else {
        vec![InvariantViolation::new(
            "core",
            "balance",
            "ledger",
            "-",
            "the ledger does not balance",
        )]
    };
    InvariantReport {
        sets: vec![InvariantSetResult {
            set: "core".to_owned(),
            passed,
            violations,
        }],
    }
}
