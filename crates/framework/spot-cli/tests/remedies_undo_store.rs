//! WP-AI-07.T4: the remedy host's durable undo store. A restart is simulated by dropping the host
//! and building a new one over the same world and the same store; the store is the in-memory one
//! (or a wrapper that fails on demand), so no database is needed. The PostgreSQL round trip needs
//! `SPOT_TEST_PG_URL` and skips without it.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value as Json, json};
use spot_agent_tiers::{AgentIdentity, AgentRegistry, Domain, Status, Tier};
use spot_cli::services::remedies::host::{
    AgentSource, ApplyPath, Deps, Fut, HostError, PipelineRun, RemedyHost, UNDO_STORE_UNAVAILABLE,
};
use spot_cli::services::remedies::undo_store::{
    MemoryUndoStore, PgUndoStore, StoreError, StoredEntry, StoredUndo, UndoStore,
};
use spot_cli::services::remedies::world::{
    CtxRequest, ShippedCatalogue, WorldAccess, WorldError, WorldKind,
};
use spot_core::ManualClock;
use spot_mcp::AuditSink;
use spot_remedies::ports::{IndexHealth, JobLife};
use spot_remedies::{
    Applied, Change, FakeWorld, PlanHash, RemedyCtx, RemedyError, RemedyId, RemedySettings,
    UndoKind, UndoRecord, UndoToken,
};
use time::OffsetDateTime;

const START: i64 = 1_795_262_400;
const WINDOW: u64 = 86_400;

fn retry_failed() -> RemedyId {
    RemedyId::new(concat!("job", ".retry_failed"))
}

/// The framework's in-memory world, once live and once as the rehearsal snapshot.
struct Worlds {
    live: Mutex<FakeWorld>,
    rehearsal: Mutex<FakeWorld>,
}

impl Worlds {
    fn seeded() -> Arc<Self> {
        let seed = || {
            let mut world = FakeWorld::new();
            world.jobs.seed("j1", JobLife::Failed, 1, None);
            world.index.seed("retrieval", IndexHealth::Stale, 3);
            world
        };
        Arc::new(Self {
            live: Mutex::new(seed()),
            rehearsal: Mutex::new(seed()),
        })
    }

    fn live_state(&self) -> String {
        format!("{:?}", *self.live.lock().unwrap())
    }
}

impl WorldAccess for Worlds {
    fn with_ctx(
        &self,
        kind: WorldKind,
        request: CtxRequest<'_>,
        work: &mut dyn FnMut(&mut RemedyCtx<'_>),
    ) -> Result<(), WorldError> {
        let cell = match kind {
            WorldKind::Live => &self.live,
            WorldKind::Rehearsal => &self.rehearsal,
        };
        let mut guard = cell.lock().unwrap();
        let world = &mut *guard;
        let mut ctx = RemedyCtx::new(
            &mut world.jobs,
            &mut world.cache,
            &mut world.index,
            &mut world.edge,
            &mut world.quarantine,
            &mut world.case,
            &mut world.document,
            request.audit,
            request.actor,
            request.now,
            request.settings,
        );
        work(&mut ctx);
        Ok(())
    }
}

/// Stored registrations, as the tier service would answer them.
struct Agents(HashMap<String, AgentIdentity>);

impl Agents {
    fn standard() -> Arc<Self> {
        let mut map = HashMap::new();
        for (name, tier) in [("agent-t1", Tier::T1), ("agent-t2", Tier::T2)] {
            let mut registry = AgentRegistry::new();
            registry
                .enroll(
                    AgentIdentity::new(name, "owner", Domain::Support, tier)
                        .with_status(Status::Active),
                )
                .unwrap();
            map.insert(name.to_owned(), registry.get(name).cloned().unwrap());
        }
        Arc::new(Self(map))
    }
}

impl AgentSource for Agents {
    fn identity<'a>(
        &'a self,
        _tenant: &'a str,
        agent_id: &'a str,
    ) -> Fut<'a, Option<AgentIdentity>> {
        Box::pin(async move { self.0.get(agent_id).cloned() })
    }
}

/// The fix pipeline stand-in: rehearse on the snapshot, then apply live.
struct Pipeline;

impl ApplyPath for Pipeline {
    fn apply<'a>(&'a self, run: PipelineRun<'a>) -> Fut<'a, Result<Applied, HostError>> {
        Box::pin(async move {
            run.direct.apply(WorldKind::Rehearsal).await?;
            run.direct.apply(WorldKind::Live).await
        })
    }
}

/// An in-memory store whose writes (or everything) can be made to fail.
#[derive(Default)]
struct Flaky {
    inner: MemoryUndoStore,
    fail_put: AtomicBool,
    puts: Mutex<usize>,
}

impl Flaky {
    fn puts(&self) -> usize {
        *self.puts.lock().unwrap()
    }
}

impl UndoStore for Flaky {
    fn ensure_schema(&self) -> Fut<'_, Result<(), StoreError>> {
        self.inner.ensure_schema()
    }

    fn put<'a>(&'a self, row: &'a StoredUndo) -> Fut<'a, Result<(), StoreError>> {
        Box::pin(async move {
            *self.puts.lock().unwrap() += 1;
            if self.fail_put.load(Ordering::SeqCst) {
                return Err(StoreError("the undo record could not be written"));
            }
            self.inner.put(row).await
        })
    }

    fn remove<'a>(&'a self, token: &'a UndoToken) -> Fut<'a, Result<(), StoreError>> {
        self.inner.remove(token)
    }

    fn list_open(&self, now: i64) -> Fut<'_, Result<Vec<StoredEntry>, StoreError>> {
        self.inner.list_open(now)
    }

    fn sweep(&self, now: i64) -> Fut<'_, Result<u64, StoreError>> {
        self.inner.sweep(now)
    }
}

struct Rig {
    worlds: Arc<Worlds>,
    audit: Arc<spot_mcp::MemoryAudit>,
    clock: Arc<ManualClock>,
    store: Arc<Flaky>,
}

impl Rig {
    fn new() -> Self {
        Self {
            worlds: Worlds::seeded(),
            audit: Arc::new(spot_mcp::MemoryAudit::new()),
            clock: Arc::new(ManualClock::new(
                OffsetDateTime::from_unix_timestamp(START).unwrap(),
            )),
            store: Arc::new(Flaky::default()),
        }
    }

    /// A host for `site` over this rig's world, audit chain, clock and store. Building another
    /// one is the restart: nothing but the world and the store survives.
    fn host(&self, site: &str, pipeline: bool) -> RemedyHost {
        RemedyHost::new(Deps {
            site: site.to_owned(),
            catalogue: Arc::new(ShippedCatalogue),
            settings: RemedySettings::default(),
            world: Some(Arc::clone(&self.worlds) as Arc<dyn WorldAccess>),
            agents: Agents::standard(),
            apply_path: pipeline.then(|| Arc::new(Pipeline) as Arc<dyn ApplyPath>),
            sink: Arc::clone(&self.audit) as Arc<dyn AuditSink>,
            clock: Arc::clone(&self.clock) as Arc<dyn spot_core::Clock>,
        })
        .unwrap()
        .with_undo_store(Arc::clone(&self.store) as Arc<dyn UndoStore>)
    }

    fn kinds(&self) -> Vec<String> {
        self.audit.records().into_iter().map(|r| r.kind).collect()
    }
}

async fn apply_retry(host: &RemedyHost) -> Result<Applied, HostError> {
    let plan = host
        .dry_run(
            "site-1",
            "agent-t1",
            &retry_failed(),
            &json!({"job_id": "j1"}),
            None,
        )
        .await
        .unwrap();
    host.apply("site-1", "agent-t1", &plan, None).await
}

#[tokio::test]
async fn an_undo_token_survives_a_restart() {
    let rig = Rig::new();
    let before = rig.worlds.live_state();
    let first = rig.host("site-1", false);
    let applied = apply_retry(&first).await.unwrap();
    assert_ne!(rig.worlds.live_state(), before);
    assert_eq!(
        rig.store.inner.len(),
        1,
        "the record is kept before the answer returns"
    );
    drop(first);

    let second = rig.host("site-1", false);
    let report = second.restore_undo_records().await.unwrap();
    assert_eq!((report.restored, report.failed), (1, 0));
    assert_eq!(second.undo_restore_failures(), 0);
    // Restoring again is a no-op.
    assert_eq!(second.restore_undo_records().await.unwrap().restored, 1);

    second
        .undo(
            "site-1",
            "agent-t1",
            &retry_failed(),
            &applied.undo_token,
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        rig.worlds.live_state(),
        before,
        "the world returns to its exact earlier state"
    );
    assert!(
        rig.store.inner.is_empty(),
        "an undone record leaves the store"
    );
}

#[tokio::test]
async fn without_a_restore_the_old_token_is_unknown() {
    let rig = Rig::new();
    let first = rig.host("site-1", false);
    let applied = apply_retry(&first).await.unwrap();
    drop(first);
    let second = rig.host("site-1", false);
    let error = second
        .undo(
            "site-1",
            "agent-t1",
            &retry_failed(),
            &applied.undo_token,
            None,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error, HostError::Remedy(RemedyError::UndoUnknown { .. })),
        "{error}"
    );
}

#[tokio::test]
async fn an_expired_row_is_swept_and_its_token_is_unknown() {
    let rig = Rig::new();
    let first = rig.host("site-1", false);
    let applied = apply_retry(&first).await.unwrap();
    drop(first);

    rig.clock.advance(Duration::from_secs(WINDOW + 1));
    let second = rig.host("site-1", false);
    // An expired row is not restored, and the sweep deletes it.
    let report = second.restore_undo_records().await.unwrap();
    assert_eq!((report.restored, report.failed), (0, 0));
    assert_eq!(rig.store.inner.len(), 1);
    assert_eq!(second.sweep_undo().await.unwrap(), 1);
    assert!(rig.store.inner.is_empty());

    let error = second
        .undo(
            "site-1",
            "agent-t1",
            &retry_failed(),
            &applied.undo_token,
            None,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            HostError::Remedy(RemedyError::UndoUnknown { .. } | RemedyError::UndoExpired { .. })
        ),
        "{error}"
    );
}

#[tokio::test]
async fn a_store_that_fails_on_put_rolls_the_apply_back_at_once() {
    let rig = Rig::new();
    let before = rig.worlds.live_state();
    let host = rig.host("site-1", false);
    rig.store.fail_put.store(true, Ordering::SeqCst);

    let error = apply_retry(&host).await.unwrap_err();
    assert_eq!(error, HostError::UndoStoreUnavailable);
    assert_eq!(error.code(), UNDO_STORE_UNAVAILABLE);
    assert_eq!(error.code(), "remedies_host.undo_store_unavailable");
    assert_eq!(rig.store.puts(), 1);
    assert_eq!(
        rig.worlds.live_state(),
        before,
        "no change is left standing with no way back"
    );
    let kinds = rig.kinds();
    assert!(
        kinds.iter().any(|k| k == "remedy.undo_store_unavailable"),
        "the refusal is on the audit chain: {kinds:?}"
    );
    assert!(kinds.iter().any(|k| k.ends_with(".undone")), "{kinds:?}");
    assert!(rig.store.inner.is_empty());
}

#[tokio::test]
async fn a_row_of_another_tenant_is_refused_and_reported() {
    let rig = Rig::new();
    let first = rig.host("site-1", false);
    apply_retry(&first).await.unwrap();
    drop(first);

    let other = rig.host("site-2", false);
    let report = other.restore_undo_records().await.unwrap();
    assert_eq!((report.restored, report.failed), (0, 1));
    assert_eq!(other.undo_restore_failures(), 1);
    assert_eq!(rig.store.inner.len(), 1, "the row is kept, not dropped");
}

#[tokio::test]
async fn a_corrupt_row_is_kept_and_reported_and_the_others_restore() {
    let rig = Rig::new();
    let before = rig.worlds.live_state();
    let first = rig.host("site-1", false);
    let applied = apply_retry(&first).await.unwrap();
    drop(first);
    rig.store.inner.plant_raw(
        "undo-corrupt",
        "site-1",
        "agent-t1",
        i64::MAX,
        "{\"token\": 12",
    );

    let second = rig.host("site-1", false);
    let report = second.restore_undo_records().await.unwrap();
    assert_eq!((report.restored, report.failed), (1, 1));
    assert_eq!(second.undo_restore_failures(), 1);
    assert_eq!(
        rig.store.inner.len(),
        2,
        "the corrupt row stays for the operator"
    );

    second
        .undo(
            "site-1",
            "agent-t1",
            &retry_failed(),
            &applied.undo_token,
            None,
        )
        .await
        .unwrap();
    assert_eq!(rig.worlds.live_state(), before);
    assert_eq!(rig.store.inner.len(), 1);
}

#[tokio::test]
async fn the_rehearsal_registry_writes_nothing() {
    let rig = Rig::new();
    let host = rig.host("site-1", true);
    let remedy = RemedyId::new(concat!("index", ".rebuild_retrieval"));
    let plan = host
        .dry_run(
            "site-1",
            "agent-t2",
            &remedy,
            &json!({"index": "retrieval"}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(rig.store.puts(), 0, "a dry-run writes nothing");
    host.apply("site-1", "agent-t2", &plan, None).await.unwrap();
    // The pipeline applied twice (rehearsal, then live) and only the live apply was kept.
    assert_eq!(rig.store.puts(), 1);
    assert_eq!(rig.store.inner.len(), 1);
}

#[tokio::test]
async fn the_stored_record_holds_only_the_frameworks_own_keys() {
    let rig = Rig::new();
    let host = rig.host("site-1", false);
    let applied = apply_retry(&host).await.unwrap();
    let raw = rig.store.inner.raw_record(&applied.undo_token).unwrap();
    let value: Json = serde_json::from_str(&raw).unwrap();
    let mut keys: Vec<&str> = value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "applied_at",
            "changes",
            "expires_at",
            "plan_hash",
            "remedy",
            "token",
            "undo"
        ]
    );
    for change in value["changes"].as_array().unwrap() {
        let mut keys: Vec<&str> = change
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, ["after", "before", "target"]);
    }
}

#[tokio::test]
async fn postgres_round_trip() {
    let Ok(url) = std::env::var("SPOT_TEST_PG_URL") else {
        return;
    };
    let mut config = deadpool_postgres::Config::new();
    config.url = Some(url);
    let pool = config
        .create_pool(
            Some(deadpool_postgres::Runtime::Tokio1),
            tokio_postgres::NoTls,
        )
        .unwrap();
    let store = PgUndoStore::new(pool);
    store.ensure_schema().await.unwrap();
    store.ensure_schema().await.unwrap();

    let remedy = retry_failed();
    let plan_hash = PlanHash::compute(&remedy, &json!({"job_id": "j1"}), &[]);
    let seq = u64::from(std::process::id());
    let record = |expires_at: i64, seq: u64| UndoRecord {
        token: UndoToken::derive(&plan_hash, 100, seq),
        remedy: remedy.clone(),
        plan_hash: plan_hash.clone(),
        applied_at: 100,
        expires_at,
        changes: vec![Change {
            target: "job:j1".to_owned(),
            before: "failed".to_owned(),
            after: "queued".to_owned(),
        }],
        undo: UndoKind::reversible("restores the job"),
    };
    let open = record(4_000_000_000, seq);
    let closed = record(1, seq + 1_000_000);
    for rec in [&open, &closed] {
        let row = StoredUndo {
            site: "site-1".to_owned(),
            agent: "agent-t1".to_owned(),
            record: rec.clone(),
        };
        store.put(&row).await.unwrap();
        store.put(&row).await.unwrap();
    }

    let listed = store.list_open(10).await.unwrap();
    let mine = listed
        .iter()
        .find(|entry| entry.token == open.token.as_str())
        .unwrap();
    assert_eq!(mine.record.as_ref(), Some(&open));
    assert_eq!(mine.site, "site-1");
    assert!(
        !listed
            .iter()
            .any(|entry| entry.token == closed.token.as_str()),
        "an expired row is not listed as open"
    );

    assert!(store.sweep(10).await.unwrap() >= 1);
    store.remove(&open.token).await.unwrap();
    let after = store.list_open(10).await.unwrap();
    assert!(!after.iter().any(|entry| entry.token == open.token.as_str()));
}
