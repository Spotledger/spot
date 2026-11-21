//! The run shared by the offline machinery and the scratch-cluster scenarios: seed, run the
//! workers while a fault future does its damage, build the oracle from the resolved log, then
//! judge it with the L7 engine invariants and the engine audit.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use spot_app_api::context::{DocumentService, HookContext, Invocation};
use spot_app_api::prelude::{CommandId, Name, RecordId, Value};
use spot_app_api::{
    AppResult, BoxFuture, Controller, Document, HookPoint, Operation, Phase, StepDecl, StepName,
    Target,
};
use spot_document::engine::{DurableDelivery, Engine, RetentionPolicy, V7Ids};
use spot_l7_engine::driver::Run;
use spot_l7_engine::runner::check_engine_with;
use spot_l7_engine::world::{PROJECT, TouchController, USER, World};
use spot_l7_invariants::history::History;
use spot_l7_invariants::invariant::RunMeta;
use spot_storage_contract::{FeedPosition, OutboxRecord, StorageBackend};

use crate::audit::{Finding, audit_documents, audit_events, audit_log, table};
use crate::log::{Attempt, AttemptLog};
use crate::oracle::{Seeded, build};
use crate::workload::{WorkloadConfig, run_worker};

fn n(s: &str) -> Name {
    Name::new(s).unwrap_or_else(|e| panic!("a name: {e:?}"))
}

/// A committed step on `time_entry` posts: the committed-delivery seam writes one job per post.
pub struct CommittedProbe;

impl Controller for CommittedProbe {
    fn doctype(&self) -> &str {
        "time_entry"
    }

    fn steps(&self) -> Vec<StepDecl> {
        vec![StepDecl {
            name: StepName::new("l8:posted").unwrap_or_else(|e| panic!("a step name: {e:?}")),
            point: HookPoint::of(Phase::Committed, Target::Op(Operation::Post)),
            priority: 0,
            budget: Duration::from_secs(5),
        }]
    }

    fn run_step<'a>(
        &'a self,
        _step: &'a StepName,
        _ctx: &'a mut dyn HookContext,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async { Ok(()) })
    }
}

/// An engine over `backend`: the command ledger on (or off, for the honesty proof) and, for the
/// committed-steps scenario, the durable delivery and the probe step.
///
/// # Errors
///
/// The engine's error when it cannot be built.
pub fn build_engine<B: StorageBackend>(
    world: &World,
    backend: B,
    ledger: bool,
    committed_steps: bool,
) -> AppResult<Arc<Engine<B>>> {
    let mut engine = world.engine(backend)?;
    if ledger {
        engine = engine.with_command_ledger(RetentionPolicy::default());
    }
    if committed_steps {
        engine = engine.with_delivery(Arc::new(DurableDelivery::new(Arc::new(V7Ids))));
        engine.set_controllers(vec![Arc::new(TouchController), Arc::new(CommittedProbe)]);
    }
    Ok(Arc::new(engine))
}

fn system(world: &World, command: &str) -> Invocation {
    let mut inv = world.inv("system");
    inv.command_id = CommandId::new(command).ok();
    inv
}

/// Prepares the store and creates the person and the project every worker links.
///
/// # Errors
///
/// The engine's error.
pub async fn seed<B: StorageBackend>(world: &World, engine: &Engine<B>) -> AppResult<Seeded> {
    engine.prepare().await?;
    let mut user = Document::new(n(USER), RecordId::new_v7());
    user.set(n("user_name"), Value::Text("l8-user".to_owned()));
    user.set(n("enabled"), Value::Bool(true));
    let user = engine
        .create(&system(world, "seed-user"), user)
        .await?
        .value
        .id();
    let mut project = Document::new(n(PROJECT), RecordId::new_v7());
    project.set(n("code"), Value::Text("PRJ-L8".to_owned()));
    project.set(n("title"), Value::Text("L8".to_owned()));
    project.set(n("status"), Value::Text("Planning".to_owned()));
    project.set(n("owner"), Value::Id(user));
    let project = engine
        .create(&system(world, "seed-project"), project)
        .await?
        .value
        .id();
    Ok(Seeded { user, project })
}

/// Everything a run found.
pub struct Core {
    /// Every attempt, sorted.
    pub attempts: Vec<Attempt>,
    /// The L7 engine report as text, and whether it held.
    pub l7_text: String,
    /// Whether the L7 engine invariants held.
    pub l7_ok: bool,
    /// The audit findings.
    pub findings: Vec<Finding>,
    /// The audit table.
    pub table: String,
    /// What the fault future reported.
    pub notes: Vec<String>,
    /// The outbox after the run.
    pub outbox: Vec<OutboxRecord>,
}

impl Core {
    /// Whether every check held.
    pub fn ok(&self) -> bool {
        self.l7_ok && self.findings.is_empty()
    }
}

/// The whole outbox, in position order.
pub async fn read_outbox<B: StorageBackend>(engine: &Engine<B>) -> Vec<OutboxRecord> {
    let mut all = Vec::new();
    let mut after = FeedPosition::START;
    loop {
        let Ok(page) = engine.backend().read_outbox(after, 5_000).await else {
            break;
        };
        let Some(last) = page.last() else { break };
        after = last.position;
        let full = page.len() >= 5_000;
        all.extend(page);
        if !full {
            break;
        }
    }
    all
}

/// Runs `cfg` over `service` while `fault` runs, then judges the result. `engine` is the engine
/// behind `service` (the checks read the store through it).
pub async fn run_core<B, F>(
    world: Arc<World>,
    engine: Arc<Engine<B>>,
    service: Arc<dyn DocumentService>,
    cfg: WorkloadConfig,
    seeded: Seeded,
    backend_name: &str,
    fault: F,
) -> Core
where
    B: StorageBackend + 'static,
    F: Future<Output = Vec<String>>,
{
    let log = Arc::new(AttemptLog::new());
    let cfg = Arc::new(cfg);
    let mut tasks = Vec::new();
    for worker in 0..cfg.workers {
        tasks.push(tokio::spawn(run_worker(
            service.clone(),
            world.clone(),
            cfg.clone(),
            seeded.project,
            worker,
            log.clone(),
        )));
    }
    let join = async {
        for t in tasks {
            let _ = t.await;
        }
    };
    let (_, notes) = tokio::join!(join, fault);
    let attempts = log.snapshot();
    let oracle = Arc::new(build(
        cfg.seed,
        cfg.workers,
        "engine-faults",
        backend_name,
        seeded,
        &attempts,
        engine.denial_count(),
    ));
    let run = Run {
        history: History::new(cfg.seed, cfg.workers, "engine-faults"),
        oracle: oracle.clone(),
        engine: engine.clone(),
        meta: RunMeta {
            seed: cfg.seed,
            backend: backend_name.to_owned(),
            profile: "engine-faults".to_owned(),
            workers: cfg.workers,
        },
    };
    let report = check_engine_with(&world, &run, oracle).await;
    let outbox = read_outbox(&engine).await;
    let mut findings = audit_log(&attempts);
    findings.extend(audit_documents(service.as_ref(), &world, &attempts).await);
    findings.extend(audit_events(&outbox, &attempts));
    Core {
        table: table(&attempts, &findings),
        attempts,
        l7_text: report.to_text(),
        l7_ok: report.ok,
        findings,
        notes,
        outbox,
    }
}
