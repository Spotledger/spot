//! The real pipeline on one scratch database: engine, intake, job store, workers, handler.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use spot_app_api::context::{DocumentService, HookContext, Invocation, JobRequest};
use spot_app_api::prelude::{CommandId, Date, Decimal, Month, Name, RecordId, Value};
use spot_app_api::{
    AppResult, BoxFuture, Controller, Document, HookPoint, Operation, Phase, StepDecl, StepName,
    Target,
};
use spot_core::SiteId;
use spot_document::engine::Engine;
use spot_jobs::intake::process_batch;
use spot_jobs::model::JobState;
use spot_jobs::store::{JobFilter, JobStore};
use spot_jobs_pg::PgJobStore;
use spot_l7_engine::world::{PROJECT, SITE, TASK, TIME_ENTRY, TouchController, USER, World};
use spot_postgres::PgBackend;
use spot_seams::RegistryBuilder;
use time::OffsetDateTime;

use crate::checks::{Fate, JobView, Truth, TruthOp};

/// The job every operation enqueues.
pub const JOB: &str = "l8:effect";

/// The lease of a claim and the reap tick: short, so a killed run recovers in seconds.
pub const LEASE: Duration = Duration::from_secs(2);
/// How often lapsed leases are reaped.
pub const REAP_TICK: Duration = Duration::from_millis(500);

fn n(s: &str) -> Name {
    Name::new(s).unwrap_or_else(|e| panic!("a name: {e:?}"))
}

/// The step that enqueues the job in the operation's own transaction (phase 3, after the write).
struct Enqueuer;

impl Controller for Enqueuer {
    fn doctype(&self) -> &str {
        TIME_ENTRY
    }

    fn steps(&self) -> Vec<StepDecl> {
        vec![StepDecl {
            name: StepName::new("l8:enqueue").unwrap_or_else(|e| panic!("a step name: {e:?}")),
            point: HookPoint::of(Phase::After, Target::Op(Operation::Create)),
            priority: 0,
            budget: Duration::from_secs(5),
        }]
    }

    fn run_step<'a>(
        &'a self,
        _step: &'a StepName,
        ctx: &'a mut dyn HookContext,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            let key = ctx.doc().id().to_string();
            ctx.enqueue_job(JobRequest {
                job: JOB.to_owned(),
                args: serde_json::json!({}),
                idempotency_key: Some(key),
                not_before: None,
            })
        })
    }
}

/// Whether the effect table has the unique key that makes the handler idempotent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Guard {
    /// `job_key` is the primary key and the handler says `ON CONFLICT DO NOTHING`.
    Guarded,
    /// No key: the handler's insert is not idempotent (the honesty demonstration).
    Unguarded,
}

/// The pipeline.
pub struct Pipeline {
    /// The world.
    pub world: Arc<World>,
    /// The engine.
    pub engine: Arc<Engine<PgBackend>>,
    /// The job store.
    pub store: Arc<PgJobStore>,
    /// The handler's own connections (a proxy may sit in front of them).
    pub effects: deadpool_postgres::Pool,
    /// The seeded person and task every operation links.
    pub person: RecordId,
    /// The task.
    pub task: RecordId,
    /// The seeded project.
    pub project: RecordId,
    /// The site of the jobs.
    pub site: SiteId,
}

fn system(world: &World, command: &str) -> Invocation {
    let mut inv = world.inv("system");
    inv.command_id = CommandId::new(command).ok();
    inv
}

impl Pipeline {
    /// Builds the pipeline on `backend` (the engine and the store) and `effects` (the handler).
    ///
    /// # Errors
    ///
    /// A message when the engine, the store or the seed fails.
    pub async fn build(
        backend: PgBackend,
        effects: deadpool_postgres::Pool,
        guard: Guard,
    ) -> Result<Pipeline, String> {
        let world = Arc::new(World::new());
        let store_pool = backend.pool();
        let engine = world.engine(backend).map_err(|e| e.code.clone())?;
        engine.set_controllers(vec![Arc::new(TouchController), Arc::new(Enqueuer)]);
        // The enqueue step runs from the seam registry (`set_controllers` only dispatches the
        // controllers' whitelisted methods): without it no operation ever enqueued a job.
        let mut seams = RegistryBuilder::new();
        seams.add_controller("l8", Arc::new(Enqueuer));
        let seams = seams.build().map_err(|e| format!("seams: {e:?}"))?;
        engine.set_seams(Arc::new(seams));
        let engine = Arc::new(engine);
        engine.prepare().await.map_err(|e| e.code.clone())?;
        let store =
            Arc::new(PgJobStore::new(store_pool.clone()).with_reap_retry_delay(Duration::ZERO));
        store.prepare().await.map_err(|e| e.to_string())?;
        let key = match guard {
            Guard::Guarded => "job_key text PRIMARY KEY",
            Guard::Unguarded => "job_key text NOT NULL",
        };
        store_pool
            .get()
            .await
            .map_err(|e| e.to_string())?
            .batch_execute(&format!(
                "CREATE TABLE IF NOT EXISTS effects ({key}, attempt integer NOT NULL)"
            ))
            .await
            .map_err(|e| e.to_string())?;
        let inv = |c: &str| system(&world, c);
        let mut user = Document::new(n(USER), RecordId::new_v7());
        user.set(n("user_name"), Value::Text("l8-job-user".to_owned()));
        user.set(n("enabled"), Value::Bool(true));
        let person = engine
            .create(&inv("seed-user"), user)
            .await
            .map_err(|e| e.code.clone())?
            .value
            .id();
        let mut project = Document::new(n(PROJECT), RecordId::new_v7());
        project.set(n("code"), Value::Text("PRJ-L8J".to_owned()));
        project.set(n("title"), Value::Text("L8 jobs".to_owned()));
        project.set(n("status"), Value::Text("Planning".to_owned()));
        project.set(n("owner"), Value::Id(person));
        let project = engine
            .create(&inv("seed-project"), project)
            .await
            .map_err(|e| e.code.clone())?
            .value
            .id();
        let mut task = Document::new(n(TASK), RecordId::new_v7());
        task.set(n("subject"), Value::Text("L8 task".to_owned()));
        task.set(n("project"), Value::Id(project));
        task.set(n("status"), Value::Text("Open".to_owned()));
        task.set(n("priority"), Value::Text("Medium".to_owned()));
        let task = engine
            .create(&inv("seed-task"), task)
            .await
            .map_err(|e| e.code.clone())?
            .value
            .id();
        let site = SiteId::new(SITE).map_err(|e| format!("{e:?}"))?;
        Ok(Pipeline {
            world,
            engine,
            store,
            effects,
            person,
            task,
            project,
            site,
        })
    }

    /// One operation: creates a Time Entry whose id the client chose. Returns whether the engine
    /// said it committed, was refused, or could not say.
    pub async fn operate(&self, id: RecordId) -> Result<(), bool> {
        let mut doc = Document::new(n(TIME_ENTRY), id);
        doc.set(n("entry_no"), Value::Text(format!("TE-L8J-{id}")));
        doc.set(n("task"), Value::Id(self.task));
        doc.set(n("person"), Value::Id(self.person));
        doc.set(
            n("work_date"),
            Value::Date(Date::from_calendar_date(2026, Month::January, 15).unwrap_or(Date::MIN)),
        );
        doc.set(n("hours"), Value::Decimal(Decimal::new(2, 0)));
        let inv = self.world.inv("manager");
        match self.engine.create(&inv, doc).await {
            Ok(_) => Ok(()),
            // `true`: the commit state is unknown; `false`: a clean refusal.
            Err(e) => Err(e.class == spot_app_api::ErrorClass::Unavailable),
        }
    }

    /// Whether the Time Entry exists: the truth of an operation whose reply was lost.
    pub async fn exists(&self, id: RecordId) -> Option<bool> {
        match self
            .engine
            .load(&self.world.inv("system"), TIME_ENTRY, id)
            .await
        {
            Ok(_) => Some(true),
            Err(e) if e.class == spot_app_api::ErrorClass::NotFound => Some(false),
            Err(_) => None,
        }
    }

    /// Settles an operation whose reply was lost: waits for the database to answer, then reads
    /// the row.
    pub async fn settle(&self, id: RecordId, deadline: Duration) -> Fate {
        let start = std::time::Instant::now();
        while start.elapsed() < deadline {
            match self.exists(id).await {
                Some(true) => return Fate::Committed,
                Some(false) => return Fate::Absent,
                None => tokio::time::sleep(Duration::from_millis(250)).await,
            }
        }
        Fate::Absent
    }

    /// One intake pass: moves outbox events to the job store. Errors are swallowed: the next pass
    /// retries from the stored position.
    pub async fn intake_once(&self) -> u32 {
        match process_batch(
            self.store.as_ref(),
            self.engine.backend(),
            &self.site,
            200,
            OffsetDateTime::now_utc(),
        )
        .await
        {
            Ok(report) => report.inserted,
            Err(_) => 0,
        }
    }

    /// Runs the intake until `stop`.
    pub async fn run_intake(self: Arc<Self>, stop: Arc<AtomicBool>) {
        while !stop.load(Ordering::SeqCst) {
            self.intake_once().await;
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Runs one worker until `stop`: claim, work, write the effect in its own transaction,
    /// complete. `work` is the pause before the effect (a wide window for a kill).
    pub async fn run_worker(self: Arc<Self>, name: String, work: Duration, stop: Arc<AtomicBool>) {
        while !stop.load(Ordering::SeqCst) {
            let claimed = self
                .store
                .claim(
                    &["default".to_owned()],
                    &name,
                    LEASE,
                    OffsetDateTime::now_utc(),
                    1,
                )
                .await
                .unwrap_or_default();
            if claimed.is_empty() {
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
            for c in claimed {
                let key = c.job.idempotency_key.clone().unwrap_or_default();
                let attempt = i32::try_from(c.job.attempt).unwrap_or(i32::MAX);
                tokio::time::sleep(work).await;
                if self.write_effect(&key, attempt).await.is_ok() {
                    let _ = self
                        .store
                        .complete(&c.claim, JobState::Succeeded, None)
                        .await;
                }
            }
        }
    }

    async fn write_effect(&self, key: &str, attempt: i32) -> Result<(), String> {
        let mut conn = self.effects.get().await.map_err(|e| e.to_string())?;
        let tx = conn.transaction().await.map_err(|e| e.to_string())?;
        tx.execute(
            "INSERT INTO effects (job_key, attempt) VALUES ($1, $2) ON CONFLICT DO NOTHING",
            &[&key, &attempt],
        )
        .await
        .map_err(|e| e.to_string())?;
        tx.commit().await.map_err(|e| e.to_string())
    }

    /// Reaps lapsed leases until `stop`.
    pub async fn run_reaper(self: Arc<Self>, stop: Arc<AtomicBool>) {
        while !stop.load(Ordering::SeqCst) {
            let _ = self.store.reap_lapsed().await;
            tokio::time::sleep(REAP_TICK).await;
        }
    }

    /// What the job store and the effect table hold.
    ///
    /// # Errors
    ///
    /// A message when either cannot be read.
    pub async fn snapshot(&self) -> Result<(Vec<JobView>, BTreeMap<String, u32>), String> {
        let jobs = self
            .store
            .list(JobFilter::default())
            .await
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|j| JobView {
                key: j.idempotency_key.unwrap_or_default(),
                state: j.state,
                attempt: j.attempt,
                max_attempts: j.max_attempts,
            })
            .collect();
        let conn = self.store_conn().await?;
        let rows = conn
            .query(
                "SELECT job_key, count(*) FROM effects GROUP BY job_key",
                &[],
            )
            .await
            .map_err(|e| e.to_string())?;
        let effects = rows
            .iter()
            .map(|r| {
                (
                    r.get::<_, String>(0),
                    u32::try_from(r.get::<_, i64>(1)).unwrap_or(u32::MAX),
                )
            })
            .collect();
        Ok((jobs, effects))
    }

    async fn store_conn(&self) -> Result<deadpool_postgres::Object, String> {
        self.engine
            .backend()
            .pool()
            .get()
            .await
            .map_err(|e| e.to_string())
    }
}

/// The oracle of a run, from the fates the operations ended in.
pub fn truth(ops: Vec<(RecordId, Fate)>) -> Truth {
    Truth {
        ops: ops
            .into_iter()
            .map(|(id, fate)| TruthOp {
                key: id.to_string(),
                fate,
            })
            .collect(),
    }
}
