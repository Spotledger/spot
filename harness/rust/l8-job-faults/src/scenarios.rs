//! The five kills. Each: 6 workers issue operations that enqueue a job each, the database is
//! killed at a seeded moment, restarts, the pipeline reconnects and runs until the job store is
//! empty or the deadline passes, then the checks of [`crate::checks`].

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use deadpool_postgres::{Config as PoolConfig, PoolConfig as Sizes, Runtime};
use spot_app_api::prelude::RecordId;
use spot_jobs::model::JobState;
use spot_l7_invariants::schedule::Rng;
use spot_l8_faults::faults::{Direction, Fault, MessageTrigger, ScheduledFault, Trigger};
use spot_l8_faults::proxy::{FaultProxy, ProxyConfig};
use spot_l8_faults::wire::PgMessage;
use spot_l8_faults::{ScratchCluster, profile_from_env};
use spot_postgres::PgBackend;
use tokio_postgres::NoTls;

use crate::checks::{Fate, Violation, check_all};
use crate::pipeline::{Guard, Pipeline, truth};

/// The scenarios.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Committed operations sit in the outbox, the intake has not run.
    KillBeforeIntake,
    /// Jobs are inserted, none claimed.
    KillBetweenIntakeAndClaim,
    /// Handlers are mid-flight.
    KillWhileRunning,
    /// The handler's commit is cut: the effect is committed, the completion is not recorded.
    KillDuringEffectCommit,
    /// Three kills in one run.
    RepeatedKills,
}

impl Kind {
    /// The name in reports.
    pub fn name(self) -> &'static str {
        match self {
            Kind::KillBeforeIntake => "kill_before_intake",
            Kind::KillBetweenIntakeAndClaim => "kill_between_intake_and_claim",
            Kind::KillWhileRunning => "kill_while_running",
            Kind::KillDuringEffectCommit => "kill_during_effect_commit",
            Kind::RepeatedKills => "repeated_kills",
        }
    }

    /// Every scenario.
    pub const ALL: [Kind; 5] = [
        Kind::KillBeforeIntake,
        Kind::KillBetweenIntakeAndClaim,
        Kind::KillWhileRunning,
        Kind::KillDuringEffectCommit,
        Kind::RepeatedKills,
    ];
}

/// The size of a run.
#[derive(Clone, Debug)]
pub struct Config {
    /// The seed of the kill plan.
    pub seed: u64,
    /// Operations in all (spread over 6 issuers).
    pub operations: u64,
    /// How long the run may take to finish after the last fault.
    pub deadline: Duration,
    /// How many handler commits the proxy may cut.
    pub cuts: u32,
    /// The pause inside a handler before its effect.
    pub work: Duration,
}

impl Config {
    /// `smoke`: 100 operations; `standard` and `nightly`: 300.
    pub fn for_profile(profile: spot_l8_faults::Profile, seed: u64) -> Config {
        let operations = match profile {
            spot_l8_faults::Profile::Smoke => 100,
            _ => 300,
        };
        Config {
            seed,
            operations,
            deadline: Duration::from_secs(120),
            cuts: 30,
            work: Duration::from_millis(50),
        }
    }

    /// The configuration of the profile in `SPOT_L8_FAULTS_PROFILE`.
    pub fn from_env(seed: u64) -> Config {
        Config::for_profile(profile_from_env(), seed)
    }
}

/// What a scenario found.
pub struct Outcome {
    /// The scenario.
    pub kind: Kind,
    /// The seed.
    pub seed: u64,
    /// The failed checks.
    pub violations: Vec<Violation>,
    /// What happened, for the report.
    pub notes: Vec<String>,
}

impl Outcome {
    /// Whether every check held.
    pub fn ok(&self) -> bool {
        self.violations.is_empty()
    }

    /// The report text: names the failing key of every violation, never a value.
    pub fn report(&self) -> String {
        let mut out = format!("{} seed {}\n", self.kind.name(), self.seed);
        for n in &self.notes {
            out.push_str(&format!("  note: {n}\n"));
        }
        for v in &self.violations {
            out.push_str(&format!("  FAIL ({}) {}: {}\n", v.check, v.key, v.detail));
        }
        if self.violations.is_empty() {
            out.push_str("  result: OK\n");
        }
        out
    }
}

/// The pool of the handler: through the proxy when there is one.
fn effects_pool(cluster: &ScratchCluster, port: u16) -> Result<deadpool_postgres::Pool, String> {
    let c = cluster.backend_config();
    let mut cfg = PoolConfig::new();
    cfg.host = Some(c.host.clone());
    cfg.port = Some(port);
    cfg.dbname = Some(c.dbname.clone());
    cfg.user = Some(c.user.clone());
    cfg.password = Some(c.password.clone());
    cfg.pool = Some(Sizes::new(12));
    cfg.create_pool(Some(Runtime::Tokio1), NoTls)
        .map_err(|e| e.to_string())
}

async fn kill_and_restart(cluster: &ScratchCluster, notes: &mut Vec<String>) {
    match cluster.kill9() {
        Ok(()) => match cluster.restart_after_kill().await {
            Ok(d) => notes.push(format!("kill -9, recovery {} ms", d.as_millis())),
            Err(e) => notes.push(format!("restart failed: {e}")),
        },
        Err(e) => notes.push(format!("kill failed: {e}")),
    }
}

/// Runs one scenario on `cluster`, with a guarded (idempotent) handler unless `guard` says not.
///
/// # Errors
///
/// A message when the harness itself failed.
pub async fn run(
    kind: Kind,
    cluster: &ScratchCluster,
    cfg: &Config,
    guard: Guard,
) -> Result<Outcome, String> {
    let proxy = if kind == Kind::KillDuringEffectCommit {
        let upstream = format!("127.0.0.1:{}", cluster.port())
            .parse()
            .map_err(|e| format!("upstream: {e}"))?;
        Some(
            FaultProxy::start(ProxyConfig::new(upstream))
                .await
                .map_err(|e| e.to_string())?,
        )
    } else {
        None
    };
    let mut pg = cluster.backend_config();
    pg.pool_max_size = 16;
    pg.pool_wait_timeout = Duration::from_secs(60);
    let backend = PgBackend::connect(pg).await.map_err(|e| e.to_string())?;
    let effects = effects_pool(
        cluster,
        proxy.as_ref().map_or(cluster.port(), |p| p.addr().port()),
    )?;
    let pipe = Arc::new(Pipeline::build(backend, effects, guard).await?);
    let mut notes = Vec::new();
    let stop = Arc::new(AtomicBool::new(false));
    let mut rng = Rng::new(cfg.seed ^ 0x4A_4F_42);
    let kill_at = Duration::from_millis(300 + rng.below(1_200));

    // The issuers: 6 tasks, each its share of the operations. Every operation's fate is read
    // back after recovery, so a lost reply is never guessed.
    let per = cfg.operations / 6;
    let mut issuers = Vec::new();
    for _ in 0..6 {
        let p = pipe.clone();
        issuers.push(tokio::spawn(async move {
            let mut ops = Vec::new();
            for _ in 0..per {
                let id = RecordId::new_v7();
                let fate = match p.operate(id).await {
                    Ok(()) => Fate::Committed,
                    Err(false) => Fate::Refused,
                    Err(true) => p.settle(id, Duration::from_secs(120)).await,
                };
                ops.push((id, fate));
            }
            ops
        }));
    }

    // Who runs when: the pipeline stages start according to the scenario.
    let mut tasks = Vec::new();
    let start_intake = |tasks: &mut Vec<tokio::task::JoinHandle<()>>| {
        tasks.push(tokio::spawn(pipe.clone().run_intake(stop.clone())))
    };
    let start_workers = |tasks: &mut Vec<tokio::task::JoinHandle<()>>| {
        for i in 0..6 {
            tasks.push(tokio::spawn(pipe.clone().run_worker(
                format!("w{i}"),
                cfg.work,
                stop.clone(),
            )));
        }
        tasks.push(tokio::spawn(pipe.clone().run_reaper(stop.clone())));
    };
    match kind {
        Kind::KillBeforeIntake => {
            tokio::time::sleep(kill_at).await;
            kill_and_restart(cluster, &mut notes).await;
            start_intake(&mut tasks);
            start_workers(&mut tasks);
        }
        Kind::KillBetweenIntakeAndClaim => {
            start_intake(&mut tasks);
            tokio::time::sleep(kill_at).await;
            kill_and_restart(cluster, &mut notes).await;
            start_workers(&mut tasks);
        }
        Kind::KillWhileRunning => {
            start_intake(&mut tasks);
            start_workers(&mut tasks);
            tokio::time::sleep(kill_at).await;
            kill_and_restart(cluster, &mut notes).await;
        }
        Kind::KillDuringEffectCommit => {
            start_intake(&mut tasks);
            start_workers(&mut tasks);
            if let Some(p) = &proxy {
                p.schedule(
                    ScheduledFault::new(
                        Direction::ClientToServer,
                        Trigger::OnMessage(MessageTrigger::every(PgMessage::Query, true, 1)),
                        Fault::Reset,
                    )
                    .at_most(cfg.cuts),
                );
            }
        }
        Kind::RepeatedKills => {
            start_intake(&mut tasks);
            start_workers(&mut tasks);
            tokio::time::sleep(kill_at).await;
            kill_and_restart(cluster, &mut notes).await;
            // One of them twice within two seconds.
            tokio::time::sleep(Duration::from_millis(800)).await;
            kill_and_restart(cluster, &mut notes).await;
            tokio::time::sleep(Duration::from_millis(500)).await;
            kill_and_restart(cluster, &mut notes).await;
        }
    }

    let mut ops = Vec::new();
    for t in issuers {
        ops.extend(t.await.map_err(|e| e.to_string())?);
    }
    if let Some(p) = &proxy {
        notes.push(format!("{} handler commits cut", p.events().len()));
    }
    // Run until every committed key has a succeeded job, or the deadline.
    let committed = ops.iter().filter(|(_, f)| *f == Fate::Committed).count();
    let start = std::time::Instant::now();
    loop {
        if let Ok((jobs, _)) = pipe.snapshot().await
            && jobs.len() >= committed
            && jobs
                .iter()
                .all(|j| matches!(j.state, JobState::Succeeded | JobState::Dead))
        {
            break;
        }
        if start.elapsed() > cfg.deadline {
            notes.push("deadline passed before the job store emptied".to_owned());
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    stop.store(true, Ordering::SeqCst);
    for t in tasks {
        let _ = t.await;
    }
    if let Some(p) = &proxy {
        p.clear();
    }
    let (jobs, effects) = pipe.snapshot().await?;
    notes.push(format!(
        "{committed} committed operations, {} jobs",
        jobs.len()
    ));
    let violations = check_all(&truth(ops), &jobs, &effects);
    Ok(Outcome {
        kind,
        seed: cfg.seed,
        violations,
        notes,
    })
}
