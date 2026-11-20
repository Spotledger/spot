//! The five scenarios, each on a scratch PostgreSQL that the harness starts, kills and restarts.
//!
//! Every scenario runs the engine workload on 8 workers, applies its fault, lets every
//! indeterminate call settle through the command ledger, and then runs the L7 engine invariants
//! and the engine audit on what the run says committed.

use std::sync::Arc;
use std::time::Duration;

use spot_document::engine::TOPIC_JOB;
use spot_l7_engine::world::World;
use spot_l7_invariants::schedule::{Profile, Rng};
use spot_l8_faults::faults::{Direction, Fault, MessageTrigger, ScheduledFault, Trigger};
use spot_l8_faults::proxy::{FaultProxy, ProxyConfig};
use spot_l8_faults::wire::PgMessage;
use spot_l8_faults::{ScratchCluster, profile_from_env};
use spot_postgres::PgBackend;

use crate::audit::Finding;
use crate::core::{Core, build_engine, run_core, seed};
use crate::log::OpKind;
use crate::workload::{Honesty, Mode, WorkloadConfig};

/// The scenarios of the brief.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `kill -9` mid-run, restart, workers reconnect, indeterminate calls settle.
    CrashDuringEngineLoad,
    /// The proxy cuts the connection right after the commit of many transactions.
    CommitReplyLost,
    /// A `correct` killed at seeded moments: both halves or neither.
    CrashBetweenCancelAndAmend,
    /// The committed-delivery seam: one job per committed post, none for an uncommitted one.
    CrashWithCommittedSteps,
    /// Three kills in one run.
    RepeatedCrashes,
}

impl Kind {
    /// The name in reports.
    pub fn name(self) -> &'static str {
        match self {
            Kind::CrashDuringEngineLoad => "crash_during_engine_load",
            Kind::CommitReplyLost => "commit_reply_lost",
            Kind::CrashBetweenCancelAndAmend => "crash_between_cancel_and_amend",
            Kind::CrashWithCommittedSteps => "crash_with_committed_steps",
            Kind::RepeatedCrashes => "repeated_crashes",
        }
    }

    /// Every scenario.
    pub const ALL: [Kind; 5] = [
        Kind::CrashDuringEngineLoad,
        Kind::CommitReplyLost,
        Kind::CrashBetweenCancelAndAmend,
        Kind::CrashWithCommittedSteps,
        Kind::RepeatedCrashes,
    ];
}

/// The size of a run.
#[derive(Clone, Debug)]
pub struct Config {
    /// The seed.
    pub seed: u64,
    /// Workers.
    pub workers: u32,
    /// Calls per worker.
    pub ops_per_worker: u64,
    /// How long the load runs before the first fault.
    pub warmup: Duration,
    /// How many commit replies the proxy may cut.
    pub cuts: u32,
}

impl Config {
    /// The documented size of a profile. `smoke`: 4 workers and 200 operations in all, 20 cuts.
    pub fn for_profile(profile: Profile, seed: u64) -> Config {
        match profile {
            Profile::Smoke => Config {
                seed,
                workers: 4,
                ops_per_worker: 50,
                warmup: Duration::from_millis(1_500),
                cuts: 20,
            },
            Profile::Standard => Config {
                seed,
                workers: 8,
                ops_per_worker: 150,
                warmup: Duration::from_secs(3),
                cuts: 200,
            },
            Profile::Nightly => Config {
                seed,
                workers: 8,
                ops_per_worker: 400,
                warmup: Duration::from_secs(6),
                cuts: 200,
            },
        }
    }

    /// The configuration of the profile in `SPOT_L8_FAULTS_PROFILE`.
    pub fn from_env(seed: u64) -> Config {
        Config::for_profile(profile_from_env(), seed)
    }
}

/// The scenarios the smoke profile runs.
pub const SMOKE: [Kind; 2] = [Kind::CrashDuringEngineLoad, Kind::CommitReplyLost];

/// What a scenario found.
pub struct ScenarioOutcome {
    /// The scenario.
    pub kind: Kind,
    /// The seed.
    pub seed: u64,
    /// What the run found.
    pub core: Core,
    /// Extra findings of the scenario itself.
    pub extra: Vec<Finding>,
}

impl ScenarioOutcome {
    /// Whether every check held.
    pub fn ok(&self) -> bool {
        self.core.ok() && self.extra.is_empty()
    }

    /// A text report: the audit table, the L7 report when it failed, and the fault notes.
    pub fn report(&self) -> String {
        let mut out = format!(
            "{} seed {}\n{}",
            self.kind.name(),
            self.seed,
            self.core.table
        );
        for f in &self.extra {
            out.push_str(&format!("  extra {} {} {}\n", f.rule, f.subject, f.detail));
        }
        for note in &self.core.notes {
            out.push_str(&format!("  note: {note}\n"));
        }
        if !self.core.l7_ok {
            out.push_str(&self.core.l7_text);
        }
        out
    }
}

/// Whether PostgreSQL binaries are available; prints `SKIPPED` or panics under `SPOT_REQUIRE_PG=1`.
pub fn pg_available(what: &str) -> bool {
    spot_l8_faults::cluster::pg_bin_dir().is_some() || {
        let _ = spot_l8_faults::skip_or_fail(what);
        false
    }
}

/// Starts a scratch cluster, or `None` when there are no binaries (a skip) or the harness runs as
/// root (a failure reported as a panic with the reason).
pub async fn start_cluster() -> Option<ScratchCluster> {
    let bin = spot_l8_faults::cluster::pg_bin_dir()?;
    match ScratchCluster::start(spot_l8_faults::ClusterConfig::new(bin)).await {
        Ok(c) => Some(c),
        Err(e) => panic!("the scratch cluster did not start: {e}"),
    }
}

fn seeded_offsets(seed: u64, count: u32, warmup: Duration) -> Vec<Duration> {
    let mut rng = Rng::new(seed ^ 0x6B69_6C6C);
    let base = u64::try_from(warmup.as_millis()).unwrap_or(1_000).max(200);
    (0..count)
        .map(|i| Duration::from_millis(base / 2 + rng.below(base) + u64::from(i) * 250))
        .collect()
}

async fn kills(cluster: &ScratchCluster, offsets: Vec<Duration>) -> Vec<String> {
    let mut notes = Vec::new();
    for offset in offsets {
        tokio::time::sleep(offset).await;
        match cluster.kill9() {
            Ok(()) => match cluster.restart_after_kill().await {
                Ok(d) => notes.push(format!("kill -9 then recovery in {} ms", d.as_millis())),
                Err(e) => notes.push(format!("restart failed: {e}")),
            },
            Err(e) => notes.push(format!("kill failed: {e}")),
        }
    }
    notes
}

fn workload(
    cfg: &Config,
    mode: Mode,
    seeded_user: spot_app_api::prelude::RecordId,
) -> WorkloadConfig {
    WorkloadConfig {
        seed: cfg.seed,
        workers: cfg.workers,
        ops_per_worker: cfg.ops_per_worker,
        mode,
        honesty: Honesty::Honest,
        settle_deadline: Duration::from_secs(120),
        backoff: Duration::from_millis(250),
        person: seeded_user,
    }
}

/// Runs one scenario on `cluster`.
///
/// # Errors
///
/// A message when the harness itself failed (the backend or the seed), never for a failed check.
pub async fn run(
    kind: Kind,
    cluster: &ScratchCluster,
    cfg: &Config,
) -> Result<ScenarioOutcome, String> {
    let world = Arc::new(World::new());
    let proxy = if kind == Kind::CommitReplyLost {
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
    pg.pool_max_size = 12;
    pg.pool_wait_timeout = Duration::from_secs(60);
    if let Some(p) = &proxy {
        pg.port = p.addr().port();
    }
    let backend = PgBackend::connect(pg).await.map_err(|e| e.to_string())?;
    let engine = build_engine(&world, backend, true, kind == Kind::CrashWithCommittedSteps)
        .map_err(|e| e.code.clone())?;
    let seeded = seed(&world, &engine).await.map_err(|e| e.code.clone())?;
    let mode = if kind == Kind::CrashBetweenCancelAndAmend {
        Mode::CorrectCycle
    } else {
        Mode::Mixed
    };
    let work = workload(cfg, mode, seeded.user);
    let service: Arc<dyn spot_app_api::context::DocumentService> = engine.clone();
    // The fault arms after the seed, so setup is never cut.
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
    let fault = async {
        match kind {
            Kind::CommitReplyLost => Vec::new(),
            Kind::CrashDuringEngineLoad | Kind::CrashWithCommittedSteps => {
                kills(cluster, vec![cfg.warmup]).await
            }
            Kind::RepeatedCrashes => kills(cluster, seeded_offsets(cfg.seed, 3, cfg.warmup)).await,
            Kind::CrashBetweenCancelAndAmend => {
                kills(cluster, seeded_offsets(cfg.seed, 2, cfg.warmup)).await
            }
        }
    };
    let mut core = run_core(
        world.clone(),
        engine.clone(),
        service,
        work,
        seeded,
        "pg",
        fault,
    )
    .await;
    let mut extra = Vec::new();
    if let Some(p) = &proxy {
        let cut = p.events().len();
        core.notes.push(format!("{cut} commit replies cut"));
        if cut == 0 {
            extra.push(Finding {
                rule: 'n',
                subject: "proxy".into(),
                detail: "no commit reply was cut: the fault never landed".into(),
            });
        }
    }
    let indeterminate = core
        .attempts
        .iter()
        .filter(|a| a.resolution.is_some())
        .count();
    core.notes.push(format!(
        "{indeterminate} indeterminate calls settled by re-sending"
    ));
    if kind == Kind::CrashWithCommittedSteps {
        extra.extend(job_findings(&core));
    }
    if kind == Kind::CrashBetweenCancelAndAmend {
        extra.extend(correct_findings(&core));
    }
    Ok(ScenarioOutcome {
        kind,
        seed: cfg.seed,
        core,
        extra,
    })
}

/// One job row per committed post, none for any other call.
fn job_findings(core: &Core) -> Vec<Finding> {
    let jobs = core
        .outbox
        .iter()
        .filter(|r| r.event.topic.as_str() == TOPIC_JOB)
        .count();
    let posts = core
        .attempts
        .iter()
        .filter(|a| a.op == OpKind::PostTe && a.committed().is_some())
        .count();
    if jobs == posts {
        Vec::new()
    } else {
        vec![Finding {
            rule: 'j',
            subject: "outbox".into(),
            detail: format!("{jobs} committed-step jobs for {posts} committed posts"),
        }]
    }
}

/// Both halves or neither: a committed correct left its original cancelled and one draft (the
/// store audit checks both documents), and an uncommitted one left the original posted.
fn correct_findings(core: &Core) -> Vec<Finding> {
    let mut out = Vec::new();
    for a in core.attempts.iter().filter(|a| a.op == OpKind::CorrectTe) {
        if a.committed().is_none() && !a.unresolved() {
            // Refused: nothing may have changed; the document audit holds the posted original at
            // its version, because no later committed call touched it.
            continue;
        }
        if a.unresolved() {
            out.push(Finding {
                rule: 'u',
                subject: a.command_id.clone(),
                detail: "a correction was never settled".into(),
            });
        }
    }
    out
}
