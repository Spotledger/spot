//! Shared fakes for the rehearsal tests: a snapshot provider, a scratch reaper, an applier and a
//! fingerprint source, plus small plan/spec builders. No real database is involved.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use spot_core::SiteId;
use spot_migration_approval::{
    ApplyTicket, Approval, ApprovalPolicy, Checks, Decision, PlanDigest, authorise_apply,
};
use spot_migration_plan::{
    BlockingClass, Change, ChangeKind, Estimate, ReleasePlan, Risk, SystemDescription,
};
use spot_rehearsal::{
    Applier, ApplyOutcome, BoxFuture, CloneRequest, CloneTarget, Error, FingerprintQuery,
    FingerprintSource, FingerprintSpec, Result, Scratch,
};
use spot_snapshot::{
    Capabilities, Consistency, Destination, ProgressSink, RestoreOptions, RestoreTarget, Restored,
    Snapshot, SnapshotError, SnapshotManifest, SnapshotOptions, SnapshotProvider, SnapshotResult,
    SnapshotTarget, TakeOutcome, VerifyReport,
};
use spot_storage_contract::{Name, Query, QueryPath, Rows, Value};

/// A UUIDv7-shaped id for the fake manifest.
pub const FAKE_SNAPSHOT_ID: &str = "01890000-0000-7000-8000-000000000000";

/// A site id for tests.
pub fn site() -> SiteId {
    SiteId::new("localhost").expect("site id")
}

/// A scratch database target.
pub fn scratch_target() -> RestoreTarget {
    RestoreTarget::Database {
        connection: "postgres://spot:spot@localhost:5432/postgres".to_owned(),
        database: "spot_scratch".to_owned(),
    }
}

/// A live-site target.
pub fn source_target() -> SnapshotTarget {
    SnapshotTarget::Database {
        connection: "postgres://spot:spot@localhost:5432/acme".to_owned(),
        tenant: "acme".to_owned(),
        database: "acme".to_owned(),
    }
}

/// A clone request whose snapshot lands in `dir` and whose scratch is [`scratch_target`].
pub fn clone_request(dir: impl Into<PathBuf>, reaper: Option<Arc<dyn Scratch>>) -> CloneRequest {
    let mut request = CloneRequest::new(
        source_target(),
        Destination { dir: dir.into() },
        scratch_target(),
    );
    request.reaper = reaper;
    request
}

/// A real ticket for `plan`, through `authorise_apply` with one valid approval. Never forged.
pub fn ticket(plan: &ReleasePlan) -> ApplyTicket {
    let approval = Approval {
        plan_digest: PlanDigest::of(plan),
        approver: "bob".to_owned(),
        role: "release_manager".to_owned(),
        decision: Decision::Approve,
        comment: "reviewed".to_owned(),
        at: 1_000,
        signature: None,
    };
    authorise_apply(
        plan,
        &[approval],
        &ApprovalPolicy::default(),
        2_000,
        &Checks::none(),
    )
    .expect("a real approval authorises the plan")
}

/// An empty plan.
pub fn empty_plan() -> ReleasePlan {
    ReleasePlan {
        from_version: 0,
        to_version: 1,
        changes: Vec::new(),
        risk: Risk::Additive,
        estimate: Estimate::empty(),
        plan_digest: "empty-plan".to_owned(),
        to: SystemDescription::new(),
    }
}

/// A plan with one behaviour change with the given id.
pub fn plan_with_behaviour_change(id: &str) -> ReleasePlan {
    ReleasePlan {
        from_version: 0,
        to_version: 1,
        changes: vec![Change::new(
            id,
            ChangeKind::StepAdded,
            "behaviour/step_added",
            "lifecycle",
            "a lifecycle step appears",
            Risk::NeedsReview,
            BlockingClass::Online,
        )],
        risk: Risk::NeedsReview,
        estimate: Estimate::empty(),
        plan_digest: format!("plan-{id}"),
        to: SystemDescription::new(),
    }
}

/// A one-section fingerprint spec: trial balance by account and company.
pub fn spec() -> FingerprintSpec {
    FingerprintSpec::new(vec![FingerprintQuery {
        key: "trial_balance".to_owned(),
        title: "Trial balance".to_owned(),
        dimensions: vec!["account".to_owned(), "company".to_owned()],
        measures: vec!["debit".to_owned()],
        bucket: None,
        query: dummy_query(),
    }])
}

/// A query the fake source ignores.
pub fn dummy_query() -> Query {
    Query::from(
        &Name::new("x").expect("name"),
        &Name::new("x").expect("name"),
        QueryPath::Report,
    )
}

/// The fake world the fingerprint source reads and the applier writes.
pub struct FakeWorld {
    data: Mutex<BTreeMap<String, Rows>>,
}

impl FakeWorld {
    /// An empty world.
    pub fn new() -> Arc<FakeWorld> {
        Arc::new(FakeWorld {
            data: Mutex::new(BTreeMap::new()),
        })
    }

    /// Sets the rows of section `key`.
    pub fn set(&self, key: &str, columns: &[&str], rows: Vec<Vec<Value>>) {
        let columns = columns
            .iter()
            .map(|c| Name::new(c.to_owned()).expect("column name"))
            .collect();
        self.data
            .lock()
            .expect("world lock")
            .insert(key.to_owned(), Rows { columns, rows });
    }

    /// Adds `delta` to one measure cell.
    pub fn bump(&self, key: &str, row: usize, column: usize, delta: i64) {
        let mut data = self.data.lock().expect("world lock");
        let rows = &mut data.get_mut(key).expect("section").rows;
        if let Value::Int(value) = &mut rows[row][column] {
            *value += delta;
        } else {
            panic!("bump target is not an integer");
        }
    }
}

/// A [`FingerprintSource`] over a [`FakeWorld`].
pub struct FakeFingerprintSource {
    world: Arc<FakeWorld>,
}

impl FakeFingerprintSource {
    /// A source over `world`.
    pub fn new(world: Arc<FakeWorld>) -> FakeFingerprintSource {
        FakeFingerprintSource { world }
    }
}

impl FingerprintSource for FakeFingerprintSource {
    fn run<'a>(
        &'a self,
        _clone: &'a CloneTarget,
        query: &'a FingerprintQuery,
    ) -> BoxFuture<'a, Result<Rows>> {
        Box::pin(async move {
            self.world
                .data
                .lock()
                .expect("world lock")
                .get(&query.key)
                .cloned()
                .ok_or_else(|| Error::FingerprintFailed {
                    section: query.key.clone(),
                    reason: "the fake world has no rows for this section".to_owned(),
                })
        })
    }
}

/// An [`Applier`] that optionally changes one measure cell, then reports success.
pub struct FakeApplier {
    world: Arc<FakeWorld>,
    change: Option<(String, usize, usize, i64)>,
    fail: bool,
}

impl FakeApplier {
    /// A no-op applier.
    pub fn noop(world: Arc<FakeWorld>) -> FakeApplier {
        FakeApplier {
            world,
            change: None,
            fail: false,
        }
    }

    /// An applier that adds `delta` to one cell (a seeded change).
    pub fn seeded(world: Arc<FakeWorld>, change: (String, usize, usize, i64)) -> FakeApplier {
        FakeApplier {
            world,
            change: Some(change),
            fail: false,
        }
    }

    /// An applier that fails.
    pub fn failing(world: Arc<FakeWorld>) -> FakeApplier {
        FakeApplier {
            world,
            change: None,
            fail: true,
        }
    }
}

impl Applier for FakeApplier {
    fn apply<'a>(
        &'a self,
        plan: &'a ReleasePlan,
        _clone: &'a CloneTarget,
    ) -> BoxFuture<'a, Result<ApplyOutcome>> {
        Box::pin(async move {
            if self.fail {
                return Err(Error::ApplyFailed {
                    reason: "seeded apply failure".to_owned(),
                });
            }
            if let Some((key, row, column, delta)) = &self.change {
                self.world.bump(key, *row, *column, *delta);
            }
            Ok(ApplyOutcome {
                executed: vec!["finalise".to_owned()],
                plan_hash: "fake-plan-hash".to_owned(),
                plan_digest: plan.plan_digest.clone(),
                backfilled_rows: 0,
            })
        })
    }
}

/// A [`Scratch`] reaper that counts its calls.
pub struct FakeScratch {
    destroyed: AtomicUsize,
    fail: bool,
}

impl FakeScratch {
    /// A reaper.
    pub fn new() -> FakeScratch {
        FakeScratch {
            destroyed: AtomicUsize::new(0),
            fail: false,
        }
    }

    /// A reaper whose drop fails (after being counted).
    pub fn failing() -> FakeScratch {
        FakeScratch {
            destroyed: AtomicUsize::new(0),
            fail: true,
        }
    }

    /// How many times the scratch was destroyed.
    pub fn destroyed(&self) -> usize {
        self.destroyed.load(Ordering::SeqCst)
    }
}

impl Scratch for FakeScratch {
    fn describe(&self) -> String {
        "scratch spot_scratch".to_owned()
    }

    fn destroy(&self) -> std::result::Result<(), String> {
        self.destroyed.fetch_add(1, Ordering::SeqCst);
        if self.fail {
            return Err("drop refused".to_owned());
        }
        Ok(())
    }
}

/// A [`SnapshotProvider`] that writes a manifest, counts calls and can fail at each step.
pub struct FakeSnapshotProvider {
    takes: AtomicUsize,
    restores: AtomicUsize,
    fail_take: bool,
    fail_take_after_write: bool,
    fail_restore: bool,
}

impl FakeSnapshotProvider {
    /// A provider that succeeds.
    pub fn ok() -> FakeSnapshotProvider {
        FakeSnapshotProvider {
            takes: AtomicUsize::new(0),
            restores: AtomicUsize::new(0),
            fail_take: false,
            fail_take_after_write: false,
            fail_restore: false,
        }
    }

    /// A provider whose `take` fails before writing anything.
    pub fn failing_take() -> FakeSnapshotProvider {
        FakeSnapshotProvider {
            fail_take: true,
            ..FakeSnapshotProvider::ok()
        }
    }

    /// A provider whose `take` writes a partial snapshot, then fails.
    pub fn failing_take_after_write() -> FakeSnapshotProvider {
        FakeSnapshotProvider {
            fail_take_after_write: true,
            ..FakeSnapshotProvider::ok()
        }
    }

    /// A provider whose `restore` fails.
    pub fn failing_restore() -> FakeSnapshotProvider {
        FakeSnapshotProvider {
            fail_restore: true,
            ..FakeSnapshotProvider::ok()
        }
    }

    /// How many takes ran.
    pub fn takes(&self) -> usize {
        self.takes.load(Ordering::SeqCst)
    }

    /// How many restores ran.
    pub fn restores(&self) -> usize {
        self.restores.load(Ordering::SeqCst)
    }
}

fn fake_manifest(tenant: &str) -> SnapshotManifest {
    SnapshotManifest {
        id: FAKE_SNAPSHOT_ID.to_owned(),
        tenant: tenant.to_owned(),
        provider: "fake".to_owned(),
        provider_version: "0.0.0".to_owned(),
        provider_kind: "fake".to_owned(),
        pg_major: spot_snapshot::SUPPORTED_MAJOR_VERSION,
        catalogue_version: "0.0.0".to_owned(),
        feed_schema: "1".to_owned(),
        outbox_generation: 1,
        outbox_last_published_seq: 0,
        started_at: "2026-01-01T00:00:00Z".to_owned(),
        finished_at: "2026-01-01T00:00:01Z".to_owned(),
        consistency: Consistency::OnlineConsistent { xmin: 1, epoch: 0 },
        total_bytes: 0,
        file_count: 0,
        files: BTreeMap::new(),
        node_local: BTreeMap::new(),
        platform_extra: BTreeMap::new(),
    }
}

impl SnapshotProvider for FakeSnapshotProvider {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            consistent_online: true,
            needs_stopped_instance: false,
            incremental: false,
            max_parallel: 1,
            estimated_throughput: 0,
        }
    }

    fn name(&self) -> &'static str {
        "fake"
    }

    fn take(
        &self,
        target: &SnapshotTarget,
        dest: &Destination,
        _opts: &SnapshotOptions,
        _progress: Arc<dyn ProgressSink>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = SnapshotResult<TakeOutcome>> + Send + '_>>
    {
        let dir = dest.dir.clone();
        let tenant = target.tenant().to_owned();
        Box::pin(async move {
            self.takes.fetch_add(1, Ordering::SeqCst);
            if self.fail_take {
                return Err(SnapshotError::io("seeded take failure"));
            }
            std::fs::create_dir_all(&dir).map_err(|e| SnapshotError::io(e.to_string()))?;
            let manifest = fake_manifest(&tenant);
            let text = manifest.to_toml()?;
            std::fs::write(dir.join("snapshot.toml"), text)
                .map_err(|e| SnapshotError::io(e.to_string()))?;
            if self.fail_take_after_write {
                return Err(SnapshotError::io("seeded take failure after writing"));
            }
            let snapshot = Snapshot::open(dir).await?;
            Ok(TakeOutcome { snapshot })
        })
    }

    fn restore(
        &self,
        snapshot: &Snapshot,
        target: &RestoreTarget,
        _opts: &RestoreOptions,
        _progress: Arc<dyn ProgressSink>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = SnapshotResult<Restored>> + Send + '_>>
    {
        let snapshot = snapshot.clone();
        let location = match target {
            RestoreTarget::Database { database, .. } => database.clone(),
            RestoreTarget::Instance { layout, .. } => layout.data_dir.display().to_string(),
        };
        Box::pin(async move {
            self.restores.fetch_add(1, Ordering::SeqCst);
            if self.fail_restore {
                return Err(SnapshotError::io("seeded restore failure"));
            }
            Ok(Restored {
                snapshot,
                location,
                activated: true,
                replaced_aside: None,
            })
        })
    }

    fn verify(
        &self,
        _snapshot: &Snapshot,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = SnapshotResult<VerifyReport>> + Send + '_>,
    > {
        Box::pin(async move {
            Ok(VerifyReport {
                files_checked: 0,
                bytes_checked: 0,
            })
        })
    }

    fn delete(
        &self,
        _snapshot: &Snapshot,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = SnapshotResult<()>> + Send + '_>> {
        Box::pin(async move { Ok(()) })
    }
}
