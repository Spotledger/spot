//! Resuming a rescue (WP-MIG-10.T3): a rescue killed after the restore leaves its journal behind; the
//! next call resumes it (no second restore, only the items without an outcome), a rescue of another
//! snapshot is refused while one is unfinished, and the gate opens only after the last item. The fake
//! host stands in for the engine and keeps its progress in the rescue journal exactly as the engine
//! does (the engine's own resume is tested in `spot-rescue`; the journal's durability in
//! `spot-rescue-pg`).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use spot_core::SiteId;
use spot_migration_apply::{BoxFuture, InMemoryAdmissionGate, NoFaults, RecordingHold};
use spot_migration_approval::{
    ApprovalPolicy, ApprovalService, ApprovalStore, InMemoryApprovalStore, MemoryAuditSink,
};
use spot_migration_audit::RoleAccess;
use spot_migration_ops::plan::{PlanInputs, plan};
use spot_migration_ops::rescue::{HostRun, Mode, RescueHost, SnapshotFacts, confirm_token, rescue};
use spot_migration_ops::testing::{FixedClock, InMemoryPlanStore};
use spot_migration_ops::{
    Actor, Approvals, Channel, LifecycleContext, LifecycleError, LifecyclePorts, PlanHash,
    PlanState, PlanStore, SharedAuditSink,
};
use spot_migration_plan::{
    ReleasePlan, SchemaPlan, SchemaRunKind, SchemaStep, SchemaStepKind, SystemDescription,
};
use spot_permissions::Principal;
use spot_rescue::{
    InMemoryRescueJournal, ItemKey, ItemOutcome, JournalRecord, RescueJournal, RunIdentity,
    RunProgress,
};

const ITEMS: [&str; 3] = ["op-1", "op-2", "op-3"];

/// A host whose "engine" keeps its progress in a rescue journal the way the real one does.
struct ResumingHost {
    journal: Arc<InMemoryRescueJournal>,
    snapshot: SnapshotFacts,
    restores: AtomicU32,
    drops: AtomicU32,
    items_run: Mutex<Vec<String>>,
    die_after_restore: AtomicBool,
    runs: AtomicU32,
}

impl ResumingHost {
    fn new(snapshot_id: &str) -> Arc<Self> {
        Arc::new(Self {
            journal: Arc::new(InMemoryRescueJournal::new()),
            snapshot: SnapshotFacts {
                id: snapshot_id.to_owned(),
                at: 1_000,
                verified: true,
            },
            restores: AtomicU32::new(0),
            drops: AtomicU32::new(0),
            items_run: Mutex::new(Vec::new()),
            die_after_restore: AtomicBool::new(false),
            runs: AtomicU32::new(0),
        })
    }
}

async fn append(journal: &InMemoryRescueJournal, record: JournalRecord) {
    let seq = RunProgress::fold(journal.entries().await.unwrap()).last_seq + 1;
    journal.append(seq, record, 7_000).await.unwrap();
}

impl RescueHost for ResumingHost {
    fn snapshot<'a>(
        &'a self,
        _plan: &'a ReleasePlan,
        _hash: &'a PlanHash,
    ) -> BoxFuture<'a, Result<Option<SnapshotFacts>, LifecycleError>> {
        Box::pin(async move { Ok(Some(self.snapshot.clone())) })
    }
    fn loss_window(
        &self,
        _at: i64,
    ) -> BoxFuture<'_, Result<BTreeMap<String, u64>, LifecycleError>> {
        Box::pin(async move { Ok(BTreeMap::new()) })
    }
    fn quiesced(&self) -> BoxFuture<'_, Result<bool, LifecycleError>> {
        Box::pin(async move { Ok(true) })
    }
    fn journal(&self) -> Option<Arc<dyn RescueJournal>> {
        Some(self.journal.clone())
    }
    fn run<'a>(
        &'a self,
        _mode: Mode,
        _plan: &'a ReleasePlan,
        snapshot: &'a SnapshotFacts,
        _now: i64,
    ) -> BoxFuture<'a, Result<HostRun, LifecycleError>> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            let progress = RunProgress::fold(self.journal.entries().await.unwrap());
            if progress.started {
                append(
                    &self.journal,
                    JournalRecord::Resumed {
                        attempt: progress.attempt + 1,
                    },
                )
                .await;
            } else {
                append(
                    &self.journal,
                    JournalRecord::Started {
                        run_id: "run-1".to_owned(),
                        site: "alpha".to_owned(),
                        identity: RunIdentity::new("platform:migration_rescue", &snapshot.id, 1)
                            .unwrap(),
                        snapshot: snapshot.id.clone(),
                        snapshot_at: snapshot.at,
                        attempt: 1,
                        ticket_id: "t".to_owned(),
                        approvers: vec!["alice".to_owned(), "bob".to_owned()],
                    },
                )
                .await;
                append(&self.journal, JournalRecord::GateClosed).await;
            }
            if progress.restored.is_none() {
                append(
                    &self.journal,
                    JournalRecord::RestoreStarted {
                        snapshot: snapshot.id.clone(),
                    },
                )
                .await;
                self.restores.fetch_add(1, Ordering::SeqCst);
                append(
                    &self.journal,
                    JournalRecord::Restored {
                        snapshot: snapshot.id.clone(),
                        location: "quarantine-1".to_owned(),
                        at: 7_000,
                    },
                )
                .await;
                if self.die_after_restore.load(Ordering::SeqCst) {
                    // Killed here: the gate stays closed, the journal says why.
                    return Err(LifecycleError::ApplyFailed);
                }
            }
            for key in ITEMS {
                if progress.outcomes.contains_key(&ItemKey::new(key)) {
                    continue;
                }
                self.items_run.lock().unwrap().push(key.to_owned());
                append(
                    &self.journal,
                    JournalRecord::ItemOutcome {
                        key: ItemKey::new(key),
                        outcome: ItemOutcome::Done,
                    },
                )
                .await;
            }
            append(&self.journal, JournalRecord::GateOpened).await;
            Ok(HostRun {
                text: "rescued".to_owned(),
                unreplayed: BTreeMap::new(),
            })
        })
    }
}

struct World {
    approvals: Arc<Approvals>,
    plans: Arc<InMemoryPlanStore>,
}

fn world() -> World {
    let store: Arc<dyn ApprovalStore> = Arc::new(InMemoryApprovalStore::new());
    World {
        approvals: Arc::new(
            ApprovalService::new(
                store,
                SharedAuditSink(Arc::new(MemoryAuditSink::new())),
                ApprovalPolicy::default(),
            )
            .unwrap(),
        ),
        plans: Arc::new(InMemoryPlanStore::new()),
    }
}

fn ctx_for(world: &World, actor: Principal) -> Arc<LifecycleContext> {
    let ports = LifecyclePorts {
        clock: Arc::new(FixedClock::new(5_000)),
        approvals: Arc::clone(&world.approvals),
        runs: None,
        snapshots: None,
        gate: Arc::new(InMemoryAdmissionGate::new()),
        hold: Arc::new(RecordingHold::new()),
        faults: Arc::new(NoFaults),
        plans: world.plans.clone(),
        access: Arc::new(RoleAccess::new().granting("release_manager")),
    };
    Arc::new(LifecycleContext::new(
        SiteId::new("alpha").unwrap(),
        Actor::new(actor, Channel::Cli).unwrap(),
        ports,
    ))
}

fn manager(name: &str) -> Principal {
    Principal::with_roles(name, &["release_manager"])
}

/// An additive plan, auto-approved by the default policy.
fn approved_plan(world: &World) -> String {
    let inputs = PlanInputs::new(
        SystemDescription::default(),
        SystemDescription::default(),
        SchemaPlan {
            steps: vec![SchemaStep {
                key: "add:party.nickname".to_owned(),
                kind: SchemaStepKind::AddColumn,
                entity: "party".to_owned(),
                object: Some("nickname".to_owned()),
                needs_backfill: false,
                run: SchemaRunKind::Transaction,
                sentence: "add the nickname column".to_owned(),
            }],
            refusals: Vec::new(),
        },
    );
    let ctx = ctx_for(world, manager("alice"));
    let report = std::thread::spawn(move || plan(&ctx, &inputs).unwrap())
        .join()
        .unwrap();
    assert_eq!(report.state, PlanState::Approved);
    report.hash.as_str().to_owned()
}

fn state_of(world: &World, hash: &str) -> PlanState {
    world
        .plans
        .load(&PlanHash::parse(hash).unwrap())
        .unwrap()
        .unwrap()
        .state
}

fn force(world: &World, hash: &str, next: PlanState) {
    let hash = PlanHash::parse(hash).unwrap();
    let stored = world.plans.load(&hash).unwrap().unwrap();
    world
        .plans
        .mark_state(&hash, stored.state, stored.version, next)
        .unwrap();
}

fn failed_plan(world: &World) -> String {
    let hash = approved_plan(world);
    force(world, &hash, PlanState::Applying);
    force(world, &hash, PlanState::Failed);
    hash
}

fn token(hash: &str, snapshot_at: i64) -> String {
    confirm_token(
        &PlanHash::parse(hash).unwrap(),
        Mode::Restore,
        snapshot_at,
        &BTreeMap::new(),
    )
}

#[tokio::test]
async fn a_rescue_killed_after_the_restore_resumes_without_restoring_again() {
    let w = world();
    let hash = failed_plan(&w);
    let fake = ResumingHost::new("snap-1");
    fake.die_after_restore.store(true, Ordering::SeqCst);
    let host: Arc<dyn RescueHost> = fake.clone();
    let ctx = ctx_for(&w, manager("alice"));
    let t = token(&hash, 1_000);

    // The first call is killed after the restore: the gate stays closed, the plan is not superseded.
    let first = rescue(&ctx, &host, &hash, Mode::Restore, &t, "bob").await;
    assert_eq!(first.err(), Some(LifecycleError::ApplyFailed));
    assert_eq!(state_of(&w, &hash), PlanState::Failed);
    let progress = RunProgress::fold(fake.journal.entries().await.unwrap());
    assert!(progress.in_progress() && progress.restored.is_some() && !progress.gate_open);

    // The next call resumes: no second restore, every item that has no outcome is run, then the
    // gate opens after the last one.
    fake.die_after_restore.store(false, Ordering::SeqCst);
    let report = rescue(&ctx, &host, &hash, Mode::Restore, &t, "bob")
        .await
        .unwrap();
    assert!(report.resumed);
    assert_eq!(report.attempt, 2);
    assert_eq!(report.state, PlanState::Superseded);
    assert_eq!(
        fake.restores.load(Ordering::SeqCst),
        1,
        "the restore ran once"
    );
    assert_eq!(
        *fake.items_run.lock().unwrap(),
        ITEMS.map(str::to_owned).to_vec()
    );
    let progress = RunProgress::fold(fake.journal.entries().await.unwrap());
    assert!(progress.gate_open && !progress.in_progress());
    assert_eq!(
        fake.drops.load(Ordering::SeqCst),
        0,
        "the post-migration database is never dropped"
    );
}

#[tokio::test]
async fn a_second_call_after_a_finished_rescue_is_not_a_resume() {
    let w = world();
    let hash = failed_plan(&w);
    let fake = ResumingHost::new("snap-1");
    let host: Arc<dyn RescueHost> = fake.clone();
    let ctx = ctx_for(&w, manager("alice"));
    let report = rescue(
        &ctx,
        &host,
        &hash,
        Mode::Restore,
        &token(&hash, 1_000),
        "bob",
    )
    .await
    .unwrap();
    assert!(!report.resumed);
    assert_eq!(report.attempt, 1);
}

#[tokio::test]
async fn a_new_rescue_of_another_snapshot_is_refused_while_one_is_unfinished() {
    let w = world();
    let hash = failed_plan(&w);
    let fake = ResumingHost::new("snap-1");
    fake.die_after_restore.store(true, Ordering::SeqCst);
    let host: Arc<dyn RescueHost> = fake.clone();
    let ctx = ctx_for(&w, manager("alice"));
    let t = token(&hash, 1_000);
    let _ = rescue(&ctx, &host, &hash, Mode::Restore, &t, "bob").await;
    assert_eq!(fake.runs.load(Ordering::SeqCst), 1);

    // The host now offers another snapshot: the verb refuses before the host runs anything.
    let other = Arc::new(ResumingHost {
        journal: Arc::clone(&fake.journal),
        snapshot: SnapshotFacts {
            id: "snap-2".to_owned(),
            at: 1_000,
            verified: true,
        },
        restores: AtomicU32::new(0),
        drops: AtomicU32::new(0),
        items_run: Mutex::new(Vec::new()),
        die_after_restore: AtomicBool::new(false),
        runs: AtomicU32::new(0),
    });
    let other_host: Arc<dyn RescueHost> = other.clone();
    assert_eq!(
        rescue(&ctx, &other_host, &hash, Mode::Restore, &t, "bob")
            .await
            .err(),
        Some(LifecycleError::RescueRefused)
    );
    assert_eq!(other.runs.load(Ordering::SeqCst), 0);
    assert_eq!(other.restores.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_host_without_a_journal_runs_as_before_and_reports_no_attempt() {
    struct Plain(Arc<ResumingHost>);
    impl RescueHost for Plain {
        fn snapshot<'a>(
            &'a self,
            plan: &'a ReleasePlan,
            hash: &'a PlanHash,
        ) -> BoxFuture<'a, Result<Option<SnapshotFacts>, LifecycleError>> {
            self.0.snapshot(plan, hash)
        }
        fn loss_window(
            &self,
            at: i64,
        ) -> BoxFuture<'_, Result<BTreeMap<String, u64>, LifecycleError>> {
            self.0.loss_window(at)
        }
        fn quiesced(&self) -> BoxFuture<'_, Result<bool, LifecycleError>> {
            self.0.quiesced()
        }
        fn run<'a>(
            &'a self,
            mode: Mode,
            plan: &'a ReleasePlan,
            snapshot: &'a SnapshotFacts,
            now: i64,
        ) -> BoxFuture<'a, Result<HostRun, LifecycleError>> {
            self.0.run(mode, plan, snapshot, now)
        }
    }
    let w = world();
    let hash = failed_plan(&w);
    let host: Arc<dyn RescueHost> = Arc::new(Plain(ResumingHost::new("snap-1")));
    let ctx = ctx_for(&w, manager("alice"));
    let report = rescue(
        &ctx,
        &host,
        &hash,
        Mode::Restore,
        &token(&hash, 1_000),
        "bob",
    )
    .await
    .unwrap();
    assert!(!report.resumed);
    assert_eq!(report.attempt, 0);
}
