//! The [`LifecycleContext`]: the ports every verb needs and nothing else.
//!
//! Everything is a trait object, so a verb is unit-testable without PostgreSQL. There is no raw
//! database handle here: a verb cannot run SQL outside its stores. Time is read through the
//! injected [`Clock`]; this crate never reads the wall clock.

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use spot_core::SiteId;
use spot_migration_apply::{AdmissionGate, Faults, SideEffectHold};
use spot_migration_approval::{ApprovalError, ApprovalService, ApprovalStore, AuditSink};
use spot_migration_audit::{
    ArtifactKind, ArtifactRef, FinishEvent, Outcome, Permit, PlanRef,
    PlanSummary as RunPlanSummary, RunAccess, RunHandle, RunId, RunRecorder, RunSpec, RunStore,
    permit,
};
use spot_permissions::Principal;
use spot_snapshot::SnapshotProvider;

use crate::error::LifecycleError;

/// The time source (microseconds since the epoch, as the other lifecycle crates take `now`).
pub trait Clock: Send + Sync {
    /// The current time.
    fn now(&self) -> i64;
}

/// Where a request came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Channel {
    /// `spot migrate <verb>`.
    Cli,
    /// The management API.
    Mgmt,
    /// The console.
    Console,
}

impl Channel {
    /// The stable name: `cli`, `mgmt` or `console`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cli => "cli",
            Self::Mgmt => "mgmt",
            Self::Console => "console",
        }
    }
}

/// Who is acting. There is no anonymous actor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Actor {
    principal: Principal,
    channel: Channel,
}

impl Actor {
    /// An actor from a principal and the channel it came through.
    ///
    /// # Errors
    /// [`LifecycleError::NotPermitted`] for a principal that is neither the system nor a named
    /// user.
    pub fn new(principal: Principal, channel: Channel) -> Result<Self, LifecycleError> {
        if !principal.is_system && principal.user.trim().is_empty() {
            return Err(LifecycleError::NotPermitted);
        }
        Ok(Self { principal, channel })
    }

    /// The principal.
    #[must_use]
    pub fn principal(&self) -> &Principal {
        &self.principal
    }

    /// The channel.
    #[must_use]
    pub fn channel(&self) -> Channel {
        self.channel
    }

    /// The user id as the audit record spells it: `system` or `user:<name>`.
    #[must_use]
    pub fn user_id(&self) -> String {
        if self.principal.is_system {
            "system".to_owned()
        } else {
            format!("user:{}", self.principal.user)
        }
    }
}

/// An audit sink shared as a trait object (`Arc<dyn AuditSink>` is not itself a sink).
#[derive(Clone)]
pub struct SharedAuditSink(pub Arc<dyn AuditSink>);

impl AuditSink for SharedAuditSink {
    fn record(&self, record: spot_audit_chain::AuditRecord) -> Result<(), ApprovalError> {
        self.0.record(record)
    }
}

/// The approval service over trait-object stores.
pub type Approvals = ApprovalService<Arc<dyn ApprovalStore>, SharedAuditSink>;

/// A plan's hash: 64 lowercase hex characters (the `PlanDigest` text; the `plan_hash` parameter).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PlanHash(String);

impl PlanHash {
    /// Parses a hash.
    ///
    /// # Errors
    /// [`LifecycleError::UnknownPlan`] when the text is not 64 lowercase hex characters.
    pub fn parse(text: &str) -> Result<Self, LifecycleError> {
        let ok = text.len() == 64
            && text
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if ok {
            Ok(Self(text.to_owned()))
        } else {
            Err(LifecycleError::UnknownPlan)
        }
    }

    /// The hex text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&spot_migration_approval::PlanDigest> for PlanHash {
    fn from(digest: &spot_migration_approval::PlanDigest) -> Self {
        Self(digest.as_hex().to_owned())
    }
}

/// Where a plan is in its life.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PlanState {
    /// Created, not yet decided.
    Proposed,
    /// Approved.
    Approved,
    /// Rehearsed.
    Rehearsed,
    /// Being applied.
    Applying,
    /// Applied and verified.
    Applied,
    /// The apply failed.
    Failed,
    /// Rejected.
    Rejected,
    /// Replaced by a newer plan.
    Superseded,
}

impl PlanState {
    /// The stored name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Proposed => "proposed",
            Self::Approved => "approved",
            Self::Rehearsed => "rehearsed",
            Self::Applying => "applying",
            Self::Applied => "applied",
            Self::Failed => "failed",
            Self::Rejected => "rejected",
            Self::Superseded => "superseded",
        }
    }

    /// Reads a stored name.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "proposed" => Self::Proposed,
            "approved" => Self::Approved,
            "rehearsed" => Self::Rehearsed,
            "applying" => Self::Applying,
            "applied" => Self::Applied,
            "failed" => Self::Failed,
            "rejected" => Self::Rejected,
            "superseded" => Self::Superseded,
            _ => return None,
        })
    }
}

/// A stored plan: the canonical plan text by hash, immutable except for state and version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredPlan {
    /// The plan hash (the key).
    pub hash: PlanHash,
    /// The canonical plan JSON (`ReleasePlan::json`). Schema only, never data values.
    pub json: String,
    /// The author's user id.
    pub author: String,
    /// When it was created.
    pub created_at: i64,
    /// The state.
    pub state: PlanState,
    /// The version, bumped by every state change.
    pub version: u64,
}

/// A plan without its text, for listings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanSummary {
    /// The plan hash.
    pub hash: PlanHash,
    /// The author's user id.
    pub author: String,
    /// When it was created.
    pub created_at: i64,
    /// The state.
    pub state: PlanState,
    /// The version.
    pub version: u64,
}

/// Plans by hash (WP-MIG-04.T6 adds the PostgreSQL implementation).
pub trait PlanStore: Send + Sync {
    /// Saves a plan. Saving the same hash with the same text and author is a no-op.
    ///
    /// # Errors
    /// [`LifecycleError::PlanChanged`] when the hash exists with different text or author;
    /// [`LifecycleError::Store`] when the store fails.
    fn save(&self, plan: StoredPlan) -> Result<(), LifecycleError>;

    /// The plan with this hash, or `None`.
    ///
    /// # Errors
    /// [`LifecycleError::PlanChanged`] when the stored text no longer matches its hash;
    /// [`LifecycleError::Store`] when the store fails.
    fn load(&self, hash: &PlanHash) -> Result<Option<StoredPlan>, LifecycleError>;

    /// Every plan, oldest first.
    ///
    /// # Errors
    /// [`LifecycleError::Store`] when the store fails.
    fn list(&self) -> Result<Vec<PlanSummary>, LifecycleError>;

    /// Compare-and-set of the state: moves `hash` from `expected` at `expected_version` to
    /// `next` and returns the new version.
    ///
    /// # Errors
    /// [`LifecycleError::UnknownPlan`]; [`LifecycleError::PlanChanged`] when the state or the
    /// version is not the expected one (a lost race, never a blind overwrite);
    /// [`LifecycleError::Store`].
    fn mark_state(
        &self,
        hash: &PlanHash,
        expected: PlanState,
        expected_version: u64,
        next: PlanState,
    ) -> Result<u64, LifecycleError>;
}

/// One rehearsal result: the report of a rehearsal of a plan, by plan hash and clock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RehearsalRecord {
    /// The rehearsed plan.
    pub plan_hash: PlanHash,
    /// When the rehearsal finished (the caller's clock).
    pub at: i64,
    /// The verdict name (at most 32 characters).
    pub verdict: String,
    /// The report as canonical JSON (counts and hashes, never row data).
    pub report_json: String,
}

/// Rehearsal results by plan hash (WP-MIG-04.T6 adds the PostgreSQL implementation).
pub trait RehearsalLedger: Send + Sync {
    /// Records a result. The same `(plan_hash, at)` again is a no-op.
    ///
    /// # Errors
    /// [`LifecycleError::Store`].
    fn record(&self, record: RehearsalRecord) -> Result<(), LifecycleError>;

    /// The newest result of `hash`.
    ///
    /// # Errors
    /// [`LifecycleError::Store`].
    fn latest(&self, hash: &PlanHash) -> Result<Option<RehearsalRecord>, LifecycleError>;

    /// Every result of `hash`, oldest first.
    ///
    /// # Errors
    /// [`LifecycleError::Store`].
    fn history(&self, hash: &PlanHash) -> Result<Vec<RehearsalRecord>, LifecycleError>;
}

/// The ports a context is built from.
pub struct LifecyclePorts {
    /// The time source.
    pub clock: Arc<dyn Clock>,
    /// Approvals (author, approver, policy, apply ticket).
    pub approvals: Arc<Approvals>,
    /// Read access to the migration audit runs; `None` when the host has none wired (a verb
    /// that needs it then refuses with `migration.port_unavailable`).
    pub runs: Option<Arc<dyn RunStore>>,
    /// Snapshots (clone for rehearsal, restore for rescue); `None` when the host has none
    /// wired (a verb that needs it refuses with `migration.port_unavailable`).
    pub snapshots: Option<Arc<dyn SnapshotProvider>>,
    /// The maintenance gate.
    pub gate: Arc<dyn AdmissionGate>,
    /// The side-effect hold.
    pub hold: Arc<dyn SideEffectHold>,
    /// Fault injection (tests); `NoFaults` in production.
    pub faults: Arc<dyn Faults>,
    /// Plans by hash.
    pub plans: Arc<dyn PlanStore>,
    /// The `migration.run` permission check.
    pub access: Arc<dyn RunAccess>,
}

/// Everything a verb needs to run for one site on behalf of one actor.
pub struct LifecycleContext {
    site: SiteId,
    actor: Actor,
    ports: LifecyclePorts,
    /// The run recorder (WP-MIG-11.T3); `None` while the host has not wired one.
    recorder: Option<Arc<dyn RunRecorder>>,
    /// Finish records that could not be written for an operation that must not be undone.
    pending: Mutex<Vec<(RunHandle, FinishEvent)>>,
}

impl LifecycleContext {
    /// A context for `site` acting as `actor`.
    #[must_use]
    pub fn new(site: SiteId, actor: Actor, ports: LifecyclePorts) -> Self {
        Self {
            site,
            actor,
            ports,
            recorder: None,
            pending: Mutex::new(Vec::new()),
        }
    }

    /// Wires the run recorder: from then on every verb runs under a run record (see
    /// [`LifecycleContext::audited`]). A host passes the recorder over the same audit store its
    /// `RunStore` reads, so there is one audit store, not two.
    ///
    /// Without a recorder the verbs run unrecorded, as before WP-MIG-11.T3; a production host must
    /// wire one (open question of the task: `LifecyclePorts` could not gain a required field
    /// without breaking every constructor outside this task's owned files).
    #[must_use]
    pub fn with_recorder(mut self, recorder: Arc<dyn RunRecorder>) -> Self {
        self.recorder = Some(recorder);
        self
    }

    /// The run recorder, when one is wired.
    #[must_use]
    pub fn recorder(&self) -> Option<&Arc<dyn RunRecorder>> {
        self.recorder.as_ref()
    }

    /// The site.
    #[must_use]
    pub fn site(&self) -> &SiteId {
        &self.site
    }

    /// The actor.
    #[must_use]
    pub fn actor(&self) -> &Actor {
        &self.actor
    }

    /// The current time.
    #[must_use]
    pub fn now(&self) -> i64 {
        self.ports.clock.now()
    }

    /// The approval service.
    #[must_use]
    pub fn approvals(&self) -> &Arc<Approvals> {
        &self.ports.approvals
    }

    /// The run store (audit).
    ///
    /// # Errors
    /// [`LifecycleError::PortUnavailable`] when the host wired none.
    pub fn runs(&self) -> Result<&Arc<dyn RunStore>, LifecycleError> {
        self.ports
            .runs
            .as_ref()
            .ok_or(LifecycleError::PortUnavailable { port: "runs" })
    }

    /// The snapshot provider.
    ///
    /// # Errors
    /// [`LifecycleError::PortUnavailable`] when the host wired none.
    pub fn snapshots(&self) -> Result<&Arc<dyn SnapshotProvider>, LifecycleError> {
        self.ports
            .snapshots
            .as_ref()
            .ok_or(LifecycleError::PortUnavailable { port: "snapshots" })
    }

    /// The maintenance gate.
    #[must_use]
    pub fn gate(&self) -> &Arc<dyn AdmissionGate> {
        &self.ports.gate
    }

    /// The side-effect hold.
    #[must_use]
    pub fn hold(&self) -> &Arc<dyn SideEffectHold> {
        &self.ports.hold
    }

    /// The fault hooks.
    #[must_use]
    pub fn faults(&self) -> &Arc<dyn Faults> {
        &self.ports.faults
    }

    /// The plan store.
    #[must_use]
    pub fn plans(&self) -> &Arc<dyn PlanStore> {
        &self.ports.plans
    }

    /// Whether `principal` (another person, for example a second approver) holds the
    /// `migration.run` permission. The actor's own check is [`Self::require_permission`].
    #[must_use]
    pub fn permits(&self, principal: &Principal) -> bool {
        self.ports.access.may_run(principal)
    }

    /// Refuses unless the actor holds the `migration.run` permission.
    ///
    /// # Errors
    /// [`LifecycleError::NotPermitted`].
    pub fn require_permission(&self) -> Result<(), LifecycleError> {
        if self.ports.access.may_run(self.actor.principal()) {
            Ok(())
        } else {
            Err(LifecycleError::NotPermitted)
        }
    }

    /// The audit permit for a begun run, checked against this actor and site.
    ///
    /// # Errors
    /// [`LifecycleError::NotPermitted`] (also for a run of another site or actor).
    pub fn permit(&self, handle: &RunHandle) -> Result<Permit, LifecycleError> {
        permit(
            self.ports.access.as_ref(),
            self.actor.principal(),
            handle,
            self.site.as_str(),
            self.now(),
        )
        .map_err(|_| LifecycleError::NotPermitted)
    }
}

/// The warning code of a rescue whose finish record is waiting for the audit store (see
/// [`LifecycleContext::pending_finishes`]).
pub const FINISH_PENDING: &str = "migration_audit.finish_pending";

/// What a verb tells [`LifecycleContext::audited`] about itself.
pub struct AuditSpec {
    /// The verb's name (letters, digits and `-`): part of the run id.
    pub verb: &'static str,
    /// The plan digest the run belongs to (64 lowercase hex characters when known).
    pub plan_digest: String,
    /// The plan as a run record keeps it.
    pub plan: PlanRef,
    /// The terminal outcome a successful verb finishes with.
    pub on_success: Outcome,
    /// Whether a finish record that cannot be written is kept for a retry instead of failing the
    /// verb (a rescue that is already restoring must never be left half done).
    pub keep_finish_on_failure: bool,
}

impl AuditSpec {
    /// A spec that finishes with [`Outcome::Verified`] on success and fails the verb when the
    /// finish cannot be written.
    #[must_use]
    pub fn new(verb: &'static str, plan_digest: impl Into<String>, plan: PlanRef) -> Self {
        Self {
            verb,
            plan_digest: plan_digest.into(),
            plan,
            on_success: Outcome::Verified,
            keep_finish_on_failure: false,
        }
    }
}

/// An error type a verb can be audited under: it can carry a [`LifecycleError`] made by the audit
/// itself and shows the one it holds.
pub trait AuditedError: From<LifecycleError> {
    /// The lifecycle error behind this one.
    fn lifecycle(&self) -> &LifecycleError;
}

impl AuditedError for LifecycleError {
    fn lifecycle(&self) -> &LifecycleError {
        self
    }
}

/// The plan reference of a record that carries no plan: a refusal records no plan content.
#[must_use]
pub fn no_plan() -> PlanRef {
    PlanRef {
        artifact: ArtifactRef::of(ArtifactKind::Plan, "none", b""),
        summary: RunPlanSummary {
            risk: "none".to_owned(),
            changes: 0,
            schema: 0,
            metadata: 0,
            patches: 0,
            behaviour: 0,
        },
    }
}

/// The digest recorded when no plan is known: 64 zeros.
#[must_use]
pub fn no_digest() -> String {
    "0".repeat(64)
}

/// Runs a future to completion on the calling thread (a sync verb reaching the async recorder).
/// Only for the threads [`off_runtime`] makes: it parks, so it must not run on a runtime thread.
fn block_on<F: Future>(future: F) -> F::Output {
    use std::task::{Context, Poll, Wake, Waker};
    struct Parker(std::thread::Thread);
    impl Wake for Parker {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = Waker::from(Arc::new(Parker(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::park(),
        }
    }
}

impl LifecycleContext {
    fn run_id(&self, verb: &str, digest: &str) -> Result<RunId, LifecycleError> {
        // Contexts of one process share the audit store: a per-context counter would give two
        // contexts the same id for the same verb at the same instant. The process-wide count
        // keeps every id of a process distinct; the instant keeps processes apart.
        static PROCESS_RUNS: AtomicU64 = AtomicU64::new(0);
        let n = PROCESS_RUNS.fetch_add(1, Ordering::SeqCst);
        let short: String = digest.chars().take(8).collect();
        RunId::new(format!("{verb}-{short}-{}-{n}", self.now())).map_err(|_| LifecycleError::Store)
    }

    /// Finish records that could not be written and are waiting for a retry (a rescue).
    #[must_use]
    pub fn pending_finishes(&self) -> usize {
        self.pending.lock().map_or(0, |p| p.len())
    }

    /// The warning a host shows while finish records wait: [`FINISH_PENDING`], or `None`.
    #[must_use]
    pub fn pending_warning(&self) -> Option<&'static str> {
        (self.pending_finishes() > 0).then_some(FINISH_PENDING)
    }

    /// Tries once more to write the finish records kept by a failed audit store; returns how many
    /// were written. A host calls it from its own timer.
    pub async fn retry_pending_finishes(&self) -> usize {
        let Some(recorder) = self.recorder.clone() else {
            return 0;
        };
        let waiting: Vec<_> = match self.pending.lock() {
            Ok(mut pending) => std::mem::take(&mut *pending),
            Err(_) => return 0,
        };
        let mut written = 0;
        for (handle, event) in waiting {
            if recorder.finish(&handle, event.clone()).await.is_ok() {
                written += 1;
            } else if let Ok(mut pending) = self.pending.lock() {
                pending.push((handle, event));
            }
        }
        written
    }

    /// Runs `work` under a run record (WP-MIG-11.T3).
    ///
    /// The permit is checked **first**. A caller without the right gets a refusal that is itself
    /// recorded (a refused run with the reason code, the actor and no plan content) and `work` never
    /// runs. Otherwise the run begins before `work`, `work` receives the run's handle (to add
    /// approval, rehearsal or apply events), and the run finishes with `spec.on_success` on success
    /// or `refused` / `failed` with the error's reason code. Start and finish are separate records,
    /// so a crash between them leaves a run the store folds as open.
    ///
    /// Fail closed: a run that cannot be begun does not start `work`. A finish that cannot be written
    /// fails the verb, except for a spec with `keep_finish_on_failure` (the rescue): its own result
    /// is returned and the finish waits in memory for [`Self::retry_pending_finishes`].
    ///
    /// With no recorder wired `work` simply runs (see [`Self::with_recorder`]).
    ///
    /// # Errors
    /// The work's own error unchanged; `migration.store_failed` when the run cannot be begun, or
    /// finished (unless kept).
    pub async fn audited<T, E, F, Fut>(&self, spec: AuditSpec, work: F) -> Result<T, E>
    where
        E: AuditedError,
        F: FnOnce(Option<RunHandle>) -> Fut,
        Fut: Future<Output = Result<T, E>>,
    {
        let Some(recorder) = self.recorder.clone() else {
            return work(None).await;
        };
        let permitted = self.require_permission();
        let (digest, plan) = if permitted.is_ok() {
            (spec.plan_digest.clone(), spec.plan.clone())
        } else {
            (no_digest(), no_plan())
        };
        let id = self.run_id(spec.verb, &digest)?;
        let handle = recorder
            .begin(RunSpec {
                id,
                plan_digest: digest,
                plan,
                actor: self.actor.user_id(),
                at: self.now(),
            })
            .await
            .map_err(|_| E::from(LifecycleError::Store))?;
        let result = match permitted {
            Err(error) => Err(E::from(error)),
            Ok(()) => match self.permit(&handle) {
                Err(error) => Err(E::from(error)),
                Ok(_) => work(Some(handle.clone())).await,
            },
        };
        let (outcome, reason) = match &result {
            Ok(_) => (spec.on_success, None),
            Err(error) => (
                crate::audit::outcome_of(error.lifecycle()),
                Some(error.lifecycle().code().to_owned()),
            ),
        };
        let event = FinishEvent {
            outcome,
            at: self.now(),
            reason,
        };
        if recorder.finish(&handle, event.clone()).await.is_err() {
            if spec.keep_finish_on_failure {
                if let Ok(mut pending) = self.pending.lock() {
                    pending.push((handle, event));
                }
                return result;
            }
            return match result {
                Ok(_) => Err(E::from(LifecycleError::Store)),
                Err(error) => Err(error),
            };
        }
        result
    }

    /// [`Self::audited`] for a synchronous verb: the recorder is awaited on the calling thread,
    /// which must be one [`off_runtime`] made (a sync verb already must not run on a runtime thread).
    ///
    /// # Errors
    /// As [`Self::audited`].
    pub fn audited_sync<T, E>(
        &self,
        spec: AuditSpec,
        work: impl FnOnce(Option<RunHandle>) -> Result<T, E>,
    ) -> Result<T, E>
    where
        E: AuditedError,
    {
        block_on(self.audited(spec, |handle| async move { work(handle) }))
    }

    /// Records one approval event on a begun run, from a synchronous verb.
    ///
    /// # Errors
    /// `migration.store_failed` when the event cannot be written (the verb then fails: evidence is
    /// part of the operation).
    pub(crate) fn record_approval_sync(
        &self,
        handle: &RunHandle,
        event: spot_migration_audit::ApprovalEvent,
    ) -> Result<(), LifecycleError> {
        match &self.recorder {
            Some(recorder) => {
                block_on(recorder.record_approval(handle, event)).map_err(|_| LifecycleError::Store)
            }
            None => Ok(()),
        }
    }

    /// The digest and plan reference of the stored plan `hash` for a run record; a plan that cannot
    /// be loaded gives [`no_digest`] and [`no_plan`] (the verb itself then fails with its own code).
    #[must_use]
    pub fn plan_audit(&self, hash: &str) -> (String, PlanRef) {
        let Ok((plan_hash, stored)) = crate::plan::load(self, hash) else {
            return (no_digest(), no_plan());
        };
        let digest = plan_hash.as_str().to_owned();
        match crate::plan::parse_stored(&stored) {
            Ok(plan) => {
                let reference = crate::audit::plan_ref(
                    &plan,
                    &stored.json,
                    &format!("plans/{}", plan_hash.as_str()),
                );
                (digest, reference)
            }
            Err(_) => (digest, no_plan()),
        }
    }
}

/// Runs `work` on a plain OS thread and awaits it, **off the tokio runtime**.
///
/// The PostgreSQL stores of the lifecycle (`PlanStore`, `ApprovalStore`, `RehearsalLedger`) are
/// synchronous and reach the database through the shared blocking bridge, which refuses any
/// thread that belongs to a tokio runtime (ADR-082 addendum 1). An async caller (the management
/// handlers, the CLI) therefore runs every call to them through this function.
///
/// # Errors
/// [`LifecycleError::Store`] when the thread cannot be started or `work` panicked.
pub async fn off_runtime<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, LifecycleError> {
    use std::sync::Mutex;
    use std::task::{Poll, Waker};

    struct Slot<T> {
        value: Option<T>,
        waker: Option<Waker>,
        done: bool,
    }

    let slot = Arc::new(Mutex::new(Slot {
        value: None,
        waker: None,
        done: false,
    }));
    let theirs = Arc::clone(&slot);
    std::thread::Builder::new()
        .name("migration-ops".to_owned())
        .spawn(move || {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)).ok();
            let mut guard = theirs.lock().unwrap_or_else(|p| p.into_inner());
            guard.value = outcome;
            guard.done = true;
            if let Some(waker) = guard.waker.take() {
                waker.wake();
            }
        })
        .map_err(|_| LifecycleError::Store)?;
    std::future::poll_fn(|cx| {
        let mut guard = slot.lock().unwrap_or_else(|p| p.into_inner());
        if guard.done {
            Poll::Ready(guard.value.take())
        } else {
            guard.waker = Some(cx.waker().clone());
            Poll::Pending
        }
    })
    .await
    .ok_or(LifecycleError::Store)
}
