//! The run: provisions the database, runs the three phases and the recovery probe, checks what
//! was left behind, cleans up (also on failure) and judges the report against the budgets.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use deadpool_postgres::Pool;
use spot_ai_isolation::{AgentId, AiAdmission, AiRequest, LoadSignal, Outcome, TenantId};
use spot_postgres::PgBackend;
use tokio::task::JoinHandle;

use crate::agent::{AgentEnv, AgentOutput, run_agent};
use crate::budget::{Budgets, evaluate};
use crate::cancel::PgCanceller;
use crate::clock::RunClock;
use crate::config::RunConfig;
use crate::error::{HarnessError, Result};
use crate::fixture::Fixture;
use crate::human::{HumanEnv, HumanOutput, run_user};
use crate::load::HumanLoad;
use crate::report::{AgentStats, LeakReport, PhaseReport, RecoveryReport, Report};
use crate::sampler;

/// Runs the harness. Returns `None` when no PostgreSQL server is configured
/// (`SPOT_TEST_PG_URL` unset; `SPOT_REQUIRE_PG=1` turns that into a panic).
///
/// The role and the schema are dropped at the end of every run, also when it fails.
///
/// # Errors
///
/// A [`HarnessError`] when the run could not be carried out. A budget breach is not an error:
/// it is in [`Report::breaches`].
pub async fn run(config: &RunConfig, budgets: &Budgets) -> Result<Option<Report>> {
    let Some(mut fixture) =
        Fixture::create(config.rows, &config.ai_pool, &config.role_settings).await?
    else {
        return Ok(None);
    };
    let outcome = run_phases(config, &fixture).await;
    let left_behind = fixture.teardown().await;
    let mut report = outcome?;
    report.leaks.ai_write_allowed = fixture.ai_write_allowed();
    report.leaks.left_behind = left_behind?;
    report.breaches = evaluate(&report, budgets);
    report.passed = report.breaches.is_empty();
    Ok(Some(report))
}

/// Everything that holds a connection to the scratch database lives inside this function, so
/// it is all gone when the clean-up runs.
async fn run_phases(config: &RunConfig, fixture: &Fixture) -> Result<Report> {
    let config = Arc::new(config.clone());
    let human_backend = connect_backend(fixture.human_config(config.human_pool_size)).await?;
    let ai_backend = connect_backend(fixture.ai_config(config.ai_pool.pool_size)).await?;
    let (human_pool, ai_pool) = (human_backend.pool(), ai_backend.pool());
    warm(&human_pool, config.human_pool_size).await;
    warm(&ai_pool, config.ai_pool.pool_size).await;

    let clock = RunClock::start();
    let load = HumanLoad::new(human_pool.clone());
    let (canceller, canceller_task) = PgCanceller::spawn();
    let ctx = Ctx {
        config: Arc::clone(&config),
        fixture,
        clock,
        human_pool,
        ai_pool,
        load,
        canceller,
    };

    let mut report = Report {
        mode: config.mode.to_owned(),
        rows: config.rows,
        human_pool_size: config.human_pool_size,
        ai_pool_size: config.ai_pool.pool_size,
        ..Report::default()
    };
    let mut admissions = Vec::new();
    for (name, agents, runaway_every) in [
        ("baseline", 0, 0),
        ("with_agents", config.agents, 0),
        ("overload", config.overload_agents, config.runaway_every),
    ] {
        let (phase, admission) = ctx.phase(name, agents, runaway_every).await?;
        report.phases.push(phase);
        admissions.push(admission);
    }
    let overload_admission = admissions
        .last()
        .cloned()
        .ok_or_else(|| HarnessError::Task("no phase ran".to_owned()))?;
    report.recovery = ctx.recover(&overload_admission).await?;

    // After the last agent stopped: no statement may still run, nothing may be held.
    let observer = fixture.observer().await?;
    let mut running = u32::MAX;
    let polled = ctx.clock.now();
    while ctx.clock.now().saturating_sub(polled) < config.settle {
        running = sampler::count_backends(&observer, fixture.role())
            .await?
            .active;
        if running == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let held = |pool: &Pool| {
        let s = pool.status();
        u32::try_from(s.size.saturating_sub(s.available)).unwrap_or(u32::MAX)
    };
    report.leaks = LeakReport {
        permits: admissions.iter().map(|a| a.in_flight()).sum(),
        connections: held(&ctx.ai_pool) + held(&ctx.human_pool),
        running_after_cancel: if running == u32::MAX { 0 } else { running },
        ..LeakReport::default()
    };
    drop(ctx);
    let _ = canceller_task.await;
    drop((human_backend, ai_backend));
    Ok(report)
}

/// What the phases share.
struct Ctx<'a> {
    config: Arc<RunConfig>,
    fixture: &'a Fixture,
    clock: RunClock,
    human_pool: Pool,
    ai_pool: Pool,
    load: Arc<HumanLoad>,
    canceller: Arc<PgCanceller>,
}

impl Ctx<'_> {
    fn human_env(&self, seed_salt: u64) -> Arc<HumanEnv> {
        Arc::new(HumanEnv {
            pool: self.human_pool.clone(),
            clock: self.clock,
            table: self.fixture.table_path(),
            rows: self.config.rows,
            think: self.config.human_think,
            report_percent: self.config.report_percent,
            load: Arc::clone(&self.load),
            seed: self.config.seed ^ seed_salt,
            stop: AtomicBool::new(false),
        })
    }

    fn new_admission(&self) -> Result<Arc<AiAdmission>> {
        let signal: Arc<dyn LoadSignal> = self.load.clone();
        let admission = AiAdmission::new(self.config.limits, self.config.ai_pool, signal)?
            .with_thresholds(self.config.thresholds)
            .with_breaker_policy(self.config.breaker);
        Ok(Arc::new(admission))
    }

    /// One phase: the human load, and `agents` agents (every `runaway_every`-th one a runaway
    /// when that is non-zero), for the configured duration.
    async fn phase(
        &self,
        name: &str,
        agents: u32,
        runaway_every: u32,
    ) -> Result<(PhaseReport, Arc<AiAdmission>)> {
        let started = self.clock.now();
        let end = started + self.config.phase_duration;
        let admission = self.new_admission()?;
        let stats = Arc::new(AgentStats::default());
        let env = self.human_env(name.len() as u64);
        let agent_env = Arc::new(AgentEnv {
            admission: Arc::clone(&admission),
            pool: self.ai_pool.clone(),
            canceller: Arc::clone(&self.canceller),
            clock: self.clock,
            table: self.fixture.table_path(),
            rows: self.config.rows,
            stats: Arc::clone(&stats),
            config: Arc::clone(&self.config),
        });
        let (stop_sampler, sampler_task) = sampler::spawn(
            self.fixture.observer().await?,
            self.fixture.role().to_owned(),
            Arc::clone(&admission),
            self.ai_pool.clone(),
            self.config.sample_interval,
            self.clock,
        );
        let users: Vec<JoinHandle<HumanOutput>> = (0..self.config.human_users)
            .map(|i| tokio::spawn(run_user(Arc::clone(&env), i, end)))
            .collect();
        let bots: Vec<JoinHandle<AgentOutput>> = (0..agents)
            .map(|i| {
                let runaway = runaway_every != 0 && i % runaway_every == 0;
                tokio::spawn(run_agent(Arc::clone(&agent_env), i, runaway, end))
            })
            .collect();

        let mut human = HumanOutput::default();
        for user in users {
            human.merge(&user.await.map_err(task_error)?);
        }
        let mut ai = AgentOutput::default();
        for bot in bots {
            ai.merge(&bot.await.map_err(task_error)?);
        }
        let _ = stop_sampler.send(());
        let sampled = sampler_task.await.map_err(task_error)??;

        let report = PhaseReport {
            name: name.to_owned(),
            duration_ms: u64::try_from(self.clock.now().saturating_sub(started).as_millis())
                .unwrap_or(u64::MAX),
            agents,
            human: human.all.summary(),
            human_by_op: human
                .by_op
                .iter()
                .map(|(op, s)| ((*op).to_owned(), s.summary()))
                .collect::<BTreeMap<_, _>>(),
            human_errors: human.errors,
            human_pool_wait: human.pool_wait.summary(),
            ai_pool_wait: ai.pool_wait.summary(),
            ai_queue_wait: ai.queue_wait.summary(),
            agent_calls: stats.snapshot(),
            ai_active_backends_max: sampled.active.iter().copied().max().unwrap_or(0),
            ai_active_client_backends_max: sampled
                .active_clients
                .iter()
                .copied()
                .max()
                .unwrap_or(0),
            ai_parallel_workers_max: sampled
                .active
                .iter()
                .zip(&sampled.active_clients)
                .map(|(all, clients)| all.saturating_sub(*clients))
                .max()
                .unwrap_or(0),
            ai_active_backends: sampled.active,
            ai_open_backends: sampled.open,
            aimd_limit: sampled.aimd,
            ai_pool_waiting_max: sampled.pool_waiting_max,
            breakers: sampled.breakers,
        };
        Ok((report, admission))
    }

    /// After the overload: the humans keep going (the load is calm again) and one probe agent
    /// asks admission until its limit is back at the configured level.
    async fn recover(&self, admission: &Arc<AiAdmission>) -> Result<RecoveryReport> {
        let configured = admission.pool().pool_size;
        let mut report = RecoveryReport {
            aimd_limit_at_overload_end: admission.aimd_limit(),
            configured_limit: configured,
            ..RecoveryReport::default()
        };
        let env = self.human_env(0xEC0);
        let started = self.clock.now();
        // Probe for longer than the budget, so the report can say how late it was.
        let give_up = started + Duration::from_secs(30);
        let users: Vec<JoinHandle<HumanOutput>> = (0..self.config.human_users)
            .map(|i| tokio::spawn(run_user(Arc::clone(&env), i, give_up)))
            .collect();
        let request = AiRequest::new(AgentId::new("probe")?, TenantId::new("tenant-probe")?);
        loop {
            let now = self.clock.now();
            if now >= give_up {
                break;
            }
            report.probes += 1;
            if let Ok(permit) = admission.admit(&request, now) {
                permit.complete(now, Outcome::Success);
            }
            if admission.aimd_limit() >= configured {
                report.recovered_after_ms =
                    u64::try_from(now.saturating_sub(started).as_millis()).ok();
                break;
            }
            tokio::time::sleep(self.config.probe_interval).await;
        }
        env.stop.store(true, Ordering::Relaxed);
        for user in users {
            let _ = user.await.map_err(task_error)?;
        }
        Ok(report)
    }
}

async fn connect_backend(config: spot_postgres::PgConfig) -> Result<PgBackend> {
    PgBackend::connect(config)
        .await
        .map_err(|e| HarnessError::Database(format!("open a pool: {e}")))
}

/// Opens every connection of a pool once, so the first phase does not pay for connecting.
async fn warm(pool: &Pool, size: u32) {
    let mut held = Vec::new();
    for _ in 0..size {
        if let Ok(client) = pool.get().await {
            held.push(client);
        }
    }
    drop(held);
}

fn task_error(error: tokio::task::JoinError) -> HarnessError {
    HarnessError::Task(error.to_string())
}
