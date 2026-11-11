//! L9 scale harness (WP-TST-15.T1).
//!
//! No other platform harness measures the README's non-functional targets (tier M: 50 to 300
//! concurrent users and 10 tx/s; tier S: 10 users). This crate is the foundation of that
//! suite:
//!
//! * [`profile`] and [`schedule`]: a workload profile (sessions, ramp, steady phase, seeded
//!   think time, operation mix) and the **open-loop** arrival schedule it fixes in advance.
//! * [`runner`]: dispatches the schedule and measures every operation from its *intended*
//!   start, so a stalled server cannot hide latency (coordinated omission).
//! * [`session`] and [`http`]: simulated users over the native API, one sign-in and one
//!   persistent connection each, with errors classified (an expected 4xx is not a failure; a
//!   5xx or a timeout is).
//! * [`hist`] and [`metrics`]: HDR-style histograms and per-operation counts.
//! * [`budget`] and [`report`]: `budgets.toml` per profile and operation, `report.json` and
//!   `report.md`; the runner exits non-zero naming the breached budget and the measured value.
//! * [`launcher`] and [`attach`]: a real `spot serve` on PostgreSQL, started and torn down by
//!   the harness, or `--attach URL` to an existing test deployment (loopback or allow-listed
//!   only).
//!
//! Time is an argument: only [`clock::MonotonicClock`] reads a clock, and it is monotonic.
//! Nothing here uses a float in a calculation.

pub mod attach;
pub mod budget;
pub mod clock;
pub mod hist;
pub mod http;
pub mod launcher;
pub mod metrics;
pub mod profile;
pub mod report;
pub mod rng;
pub mod runner;
pub mod schedule;
pub mod session;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use budget::Budgets;
use clock::MonotonicClock;
use http::Target;
use profile::Profile;
use report::Report;
use session::{ApiExecutor, SimUser};

/// Where the load goes.
pub enum Mode {
    /// Start a scratch `spot serve` on the PostgreSQL server `SPOT_TEST_PG_URL` names.
    Launch(launcher::LaunchOptions),
    /// Load an existing deployment with these users.
    Attach {
        /// The server and the `Host` header.
        target: Target,
        /// At least as many users as the profile has sessions.
        users: Vec<SimUser>,
    },
}

/// Everything one run needs.
pub struct RunOptions {
    /// The workload.
    pub profile: Profile,
    /// The budgets it is judged against.
    pub budgets: Budgets,
    /// Launch or attach.
    pub mode: Mode,
    /// The run's id (it appears in the report and in request ids).
    pub run_id: String,
    /// Where `report.json` and `report.md` are written, when anywhere.
    pub out_dir: Option<PathBuf>,
}

/// A finished run.
pub struct Finished {
    /// The report.
    pub report: Report,
}

/// Runs a profile end to end. `Ok(None)` means there is no PostgreSQL to launch against
/// (`SPOT_TEST_PG_URL` unset), which the launcher has already printed.
pub async fn execute(options: RunOptions) -> Result<Option<Finished>, String> {
    let RunOptions {
        profile,
        budgets,
        mode,
        run_id,
        out_dir,
    } = options;
    // A profile with no budget would pass by default; refuse before any server is started.
    if budgets.for_profile(&profile.name).is_empty() {
        return Err(format!(
            "no budget for profile `{}`: add a [[budget]] table to the budgets file",
            profile.name
        ));
    }
    let (target, users, sampler, launched, kind) = match mode {
        Mode::Launch(mut launch) => {
            launch.users = profile.sessions;
            launch.run_id = run_id.clone();
            let Some(launched) = launcher::Launched::start(&launch).await? else {
                return Ok(None);
            };
            (
                launched.target.clone(),
                launched.users.clone(),
                Some(launched.sampler()),
                Some(launched),
                "launched",
            )
        }
        Mode::Attach { target, users } => {
            if users.len() < profile.sessions as usize {
                return Err(format!(
                    "the profile needs {} users and the users file has {}",
                    profile.sessions,
                    users.len()
                ));
            }
            (target, users, None, None, "attached")
        }
    };
    let outcome = run_against(&profile, &budgets, target, users, sampler, &run_id, kind).await;
    if let Some(launched) = launched {
        launched.shutdown().await;
    }
    let report = outcome?;
    if let Some(dir) = out_dir {
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        std::fs::write(dir.join(format!("report.{}", "json")), report.to_json())
            .map_err(|e| format!("cannot write report.json: {e}"))?;
        std::fs::write(dir.join(format!("report.{}", "md")), report.to_markdown())
            .map_err(|e| format!("cannot write report.md: {e}"))?;
    }
    Ok(Some(Finished { report }))
}

async fn run_against(
    profile: &Profile,
    budgets: &Budgets,
    target: Target,
    mut users: Vec<SimUser>,
    sampler: Option<Arc<dyn runner::Sampler>>,
    run_id: &str,
    kind: &str,
) -> Result<Report, String> {
    users.truncate(profile.sessions as usize);
    let executor = ApiExecutor::start(
        target,
        users,
        run_id,
        profile.seed,
        Duration::from_millis(profile.request_timeout_ms),
        &profile.report,
    )
    .await?;
    let result = runner::run(
        profile,
        Arc::new(executor),
        Arc::new(MonotonicClock::new()),
        sampler,
    )
    .await;
    let verdicts = budgets
        .evaluate(&profile.name, &result.metrics, profile.steady_seconds)
        .map_err(|e| e.to_string())?;
    Ok(Report::build(run_id, kind, profile, &result, verdicts))
}
