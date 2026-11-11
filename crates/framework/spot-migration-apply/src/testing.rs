//! The backend-independent test doubles for the apply engine.
//!
//! Everything here works without a database, a network or a clock, so the engine's state machine,
//! its restart logic and the "a crash never opens the site" invariant are exercised on every
//! machine. A host that wires the real PostgreSQL runner and snapshot port (T2) uses the same
//! traits.

use std::collections::BTreeMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use spot_core::SiteId;
use spot_migration_approval::{ApplyTicket, ApprovalPolicy, Checks, authorise_apply};
use spot_migration_plan::{
    Area, BlockingClass, Change, ChangeKind, Estimate, ReleasePlan, Risk, SystemDescription,
};

use crate::BoxFuture;
use crate::error::{Error, Result};
use crate::snapshot::{RestoreOutcome, SnapshotId, SnapshotPort};
use crate::step::{
    Checkpoint, Idempotency, RestorePoint, StepContext, StepDescriptor, StepOutcome, StepRunner,
};
use crate::verify::{AppliedState, VerifyOutcome, VerifyPort};

/// A valid test site id.
#[must_use]
pub fn site_id() -> SiteId {
    SiteId::new("localhost").expect("localhost is a valid site id")
}

/// A ticket that authorises `plan` under the default policy (an additive plan auto-approves).
///
/// # Errors
/// [`Error::InvalidInput`] when the plan's risk class needs approvals the default policy cannot
/// supply without records.
pub fn ticket_for(plan: &ReleasePlan, now: i64) -> Result<ApplyTicket> {
    authorise_apply(plan, &[], &ApprovalPolicy::default(), now, &Checks::none()).map_err(
        |refusal| Error::InvalidInput {
            what: "ticket".to_owned(),
            reason: refusal.to_string(),
        },
    )
}

/// A plan with one change per area in `areas`, and a digest derived from its changes.
///
/// `ReleasePlan::steps` then yields one schema step per schema change, a `publish_metadata` step
/// when there is any metadata change, one patch step per patch change, and an
/// `activate_behaviour` step when there is any behaviour change.
#[must_use]
pub fn sample_plan(areas: &[Area]) -> ReleasePlan {
    let mut changes = Vec::new();
    let (mut schema, mut meta, mut patch, mut behaviour) = (0usize, 0usize, 0usize, 0usize);
    for area in areas {
        let (kind, id) = match area {
            Area::Schema => {
                schema += 1;
                (ChangeKind::SchemaAddColumn, format!("schema/{schema}"))
            }
            Area::Metadata => {
                meta += 1;
                (ChangeKind::FieldAdded, format!("meta/{meta}"))
            }
            Area::Patch => {
                patch += 1;
                (ChangeKind::PatchAdded, format!("patch/{patch}"))
            }
            Area::Behaviour => {
                behaviour += 1;
                (ChangeKind::StepAdded, format!("behaviour/{behaviour}"))
            }
        };
        changes.push(Change::new(
            id,
            kind,
            "rule",
            "target",
            "a change",
            Risk::Additive,
            BlockingClass::Online,
        ));
    }
    let mut plan = ReleasePlan {
        from_version: 1,
        to_version: 2,
        changes,
        risk: Risk::Additive,
        estimate: Estimate::empty(),
        plan_digest: String::new(),
        to: SystemDescription::new(),
    };
    let mut hasher = DefaultHasher::new();
    for change in &plan.changes {
        change.id.hash(&mut hasher);
    }
    plan.plan_digest = format!("plan-{:016x}", hasher.finish());
    plan
}

/// The in-memory site the fake runner writes and the fake snapshot restores.
///
/// Its state is the list of step ids that have taken effect, in order, which is what the end-state
/// assertions compare against.
#[derive(Debug, Default)]
pub struct FakeSite {
    inner: Mutex<SiteState>,
}

#[derive(Debug, Default, Clone)]
struct SiteState {
    applied: Vec<String>,
    points: BTreeMap<String, Vec<String>>,
}

impl FakeSite {
    /// An empty site.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The step ids that have taken effect, in order.
    #[must_use]
    pub fn applied(&self) -> Vec<String> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .applied
            .clone()
    }

    /// Applies `step_id` (records it once; applying it again is a no-op, as an idempotent step).
    pub fn apply(&self, step_id: &str) {
        let mut state = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !state.applied.iter().any(|id| id == step_id) {
            state.applied.push(step_id.to_owned());
        }
    }

    /// Records the current state as the restore point `id`.
    pub fn capture(&self, id: &str) {
        let mut state = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let applied = state.applied.clone();
        state.points.insert(id.to_owned(), applied);
    }

    /// Resets the state to the restore point `id`.
    pub fn rollback(&self, id: &str) {
        let mut state = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(applied) = state.points.get(id).cloned() {
            state.applied = applied;
        }
    }

    /// Resets the state to an explicit list (what a snapshot restore does).
    pub fn reset_to(&self, applied: Vec<String>) {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .applied = applied;
    }

    /// How many times `step_id` has taken effect.
    #[must_use]
    pub fn count(&self, step_id: &str) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .applied
            .iter()
            .filter(|id| id.as_str() == step_id)
            .count()
    }
}

/// A snapshot port over a [`FakeSite`]: `take` captures the site, `restore` resets it.
#[derive(Debug)]
pub struct FakeSnapshotPort {
    site: Arc<FakeSite>,
    taken: Mutex<BTreeMap<String, Vec<String>>>,
    next: AtomicU64,
    fail: AtomicBool,
}

impl FakeSnapshotPort {
    /// A port over `site`.
    #[must_use]
    pub fn new(site: Arc<FakeSite>) -> Self {
        Self {
            site,
            taken: Mutex::new(BTreeMap::new()),
            next: AtomicU64::new(1),
            fail: AtomicBool::new(false),
        }
    }

    /// Makes every `restore` fail, to exercise the "the site stays closed" path.
    pub fn fail_restores(&self) {
        self.fail.store(true, Ordering::SeqCst);
    }
}

impl SnapshotPort for FakeSnapshotPort {
    fn take<'a>(&'a self, _label: &'a str, _now: i64) -> BoxFuture<'a, Result<SnapshotId>> {
        Box::pin(async move {
            let n = self.next.fetch_add(1, Ordering::SeqCst);
            let id = format!("snap-{n}");
            self.taken
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(id.clone(), self.site.applied());
            SnapshotId::new(id)
        })
    }

    fn exists<'a>(&'a self, id: &'a SnapshotId) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            Ok(self
                .taken
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .contains_key(id.as_str()))
        })
    }

    fn restore<'a>(
        &'a self,
        id: &'a SnapshotId,
        now: i64,
    ) -> BoxFuture<'a, Result<RestoreOutcome>> {
        Box::pin(async move {
            if self.fail.load(Ordering::SeqCst) {
                return Err(Error::SnapshotFailed {
                    message: "the fake snapshot restore was told to fail".to_owned(),
                });
            }
            let captured = self
                .taken
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(id.as_str())
                .cloned();
            let Some(captured) = captured else {
                return Err(Error::SnapshotFailed {
                    message: format!("snapshot {id} was not taken"),
                });
            };
            self.site.reset_to(captured);
            Ok(RestoreOutcome {
                snapshot: id.clone(),
                location: "fake-site".to_owned(),
                at: now,
            })
        })
    }
}

/// How one step behaves in the [`FakeRunner`].
#[derive(Clone, Debug)]
pub struct StepConfig {
    /// Whether the step runs in one transaction.
    pub transactional: bool,
    /// The declared idempotency (`None` means undeclared).
    pub idempotency: Option<Idempotency>,
    /// A per-step timeout.
    pub timeout: Option<Duration>,
    /// Fail the step with a typed error.
    pub fail: bool,
    /// Kill the run inside the step (a simulated crash).
    pub kill: bool,
}

impl Default for StepConfig {
    fn default() -> Self {
        Self {
            transactional: false,
            idempotency: Some(Idempotency::Idempotent),
            timeout: None,
            fail: false,
            kill: false,
        }
    }
}

impl StepConfig {
    /// A transactional, idempotent step.
    #[must_use]
    pub fn transactional() -> Self {
        Self {
            transactional: true,
            ..Self::default()
        }
    }

    /// An undeclared-idempotency step.
    #[must_use]
    pub fn undeclared() -> Self {
        Self {
            idempotency: None,
            ..Self::default()
        }
    }
}

/// An in-memory step runner over a [`FakeSite`].
#[derive(Debug)]
pub struct FakeRunner {
    site: Arc<FakeSite>,
    config: Mutex<BTreeMap<usize, StepConfig>>,
    runs: Mutex<Vec<usize>>,
}

impl FakeRunner {
    /// A runner over `site`; every step is non-transactional and idempotent unless configured.
    #[must_use]
    pub fn new(site: Arc<FakeSite>) -> Self {
        Self {
            site,
            config: Mutex::new(BTreeMap::new()),
            runs: Mutex::new(Vec::new()),
        }
    }

    /// Configures the step at `index`.
    #[must_use]
    pub fn configure(self, index: usize, config: StepConfig) -> Self {
        self.config
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(index, config);
        self
    }

    /// The indices `run` was called for, in order.
    #[must_use]
    pub fn runs(&self) -> Vec<usize> {
        self.runs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// How many times the step at `index` ran.
    #[must_use]
    pub fn run_count(&self, index: usize) -> usize {
        self.runs().iter().filter(|ran| **ran == index).count()
    }

    fn config_for(&self, index: usize) -> StepConfig {
        self.config
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&index)
            .cloned()
            .unwrap_or_default()
    }
}

impl StepRunner for FakeRunner {
    fn describe(&self, index: usize, step: &spot_migration_plan::ApplyStep) -> StepDescriptor {
        let config = self.config_for(index);
        StepDescriptor {
            index,
            step: step.clone(),
            transactional: config.transactional,
            idempotency: config.idempotency,
            timeout: config.timeout,
        }
    }

    fn restore_point(&self, descriptor: &StepDescriptor) -> Option<RestorePoint> {
        if descriptor.transactional {
            return None;
        }
        let id = format!("rp-{}", descriptor.index);
        self.site.capture(&id);
        Some(RestorePoint {
            id,
            detail: "fake restore point".to_owned(),
        })
    }

    fn run<'a>(
        &'a self,
        descriptor: &'a StepDescriptor,
        _ctx: &'a StepContext,
    ) -> BoxFuture<'a, Result<StepOutcome>> {
        Box::pin(async move {
            self.runs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(descriptor.index);
            let config = self.config_for(descriptor.index);
            if config.kill {
                // A kill stands for the process dying once: clear it so a restart's resume can
                // succeed.
                if let Some(entry) = self
                    .config
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .get_mut(&descriptor.index)
                {
                    entry.kill = false;
                }
                if !config.transactional {
                    // A non-transactional step that is killed leaves a partial effect; its
                    // restore point is what compensates it.
                    self.site.apply(&format!("{}-partial", descriptor.step.id));
                }
                return Err(Error::Killed {
                    at: format!("inside_step:{}", descriptor.index),
                });
            }
            if config.fail {
                return Err(Error::StepFailed {
                    index: descriptor.index,
                    id: descriptor.step.id.clone(),
                    reason: "the fake step was told to fail".to_owned(),
                });
            }
            self.site.apply(&descriptor.step.id);
            Ok(StepOutcome {
                checkpoint: Some(Checkpoint {
                    key: format!("ck-{}", descriptor.index),
                    detail: "fake checkpoint".to_owned(),
                }),
                detail: "fake step applied".to_owned(),
            })
        })
    }

    fn rollback<'a>(
        &'a self,
        restore_point: &'a RestorePoint,
        _ctx: &'a StepContext,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.site.rollback(&restore_point.id);
            Ok(())
        })
    }
}

/// A verifier that always passes or always fails.
#[derive(Debug)]
pub struct FakeVerify {
    pass: AtomicBool,
}

impl FakeVerify {
    /// A verifier that passes.
    #[must_use]
    pub fn passing() -> Self {
        Self {
            pass: AtomicBool::new(true),
        }
    }

    /// A verifier that fails.
    #[must_use]
    pub fn failing() -> Self {
        Self {
            pass: AtomicBool::new(false),
        }
    }

    /// Changes whether it passes.
    pub fn set_pass(&self, pass: bool) {
        self.pass.store(pass, Ordering::SeqCst);
    }
}

impl VerifyPort for FakeVerify {
    fn verify<'a>(&'a self, _plan: &'a ReleasePlan) -> BoxFuture<'a, Result<VerifyOutcome>> {
        Box::pin(async move {
            let passed = self.pass.load(Ordering::SeqCst);
            Ok(VerifyOutcome {
                passed,
                mismatches: if passed {
                    Vec::new()
                } else {
                    vec!["residual".to_owned()]
                },
                invariants: Vec::new(),
            })
        })
    }
}

/// The system as it is after apply, for a real [`crate::PlanVerifier`].
#[derive(Debug)]
pub struct FakeApplied {
    after: Mutex<SystemDescription>,
}

impl FakeApplied {
    /// A source returning `after`.
    #[must_use]
    pub fn new(after: SystemDescription) -> Self {
        Self {
            after: Mutex::new(after),
        }
    }

    /// Replaces the description.
    pub fn set(&self, after: SystemDescription) {
        *self
            .after
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = after;
    }
}

impl AppliedState for FakeApplied {
    fn applied(&self) -> BoxFuture<'_, Result<SystemDescription>> {
        Box::pin(async move {
            Ok(self
                .after
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone())
        })
    }
}
