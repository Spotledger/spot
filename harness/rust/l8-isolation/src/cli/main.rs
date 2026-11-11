//! `spot-l8`: runs the isolation workloads and checks the histories, from the command line.
//!
//! ```text
//! spot-l8 run --backend fake|pg --level rc|rr|ser --workload list-append|rw-register \
//!            --seed N [--txns N] [--workers W] [--keys K] [--report out.json] [--history out.jsonl]
//! spot-l8 check history.jsonl --claimed LEVEL [--report out.json]
//! spot-l8 matrix --seed N [--workload list-append|rw-register] [--report out.json]
//! spot-l8 weak --weakness NAME --level rc|rr|ser --workload W --seed N [--report out.json]
//! ```
//!
//! Exit status: 0 when the claimed level held, 1 when it was violated, 2 on a usage or setup
//! error. The `pg` backend needs `SPOT_TEST_PG_URL`; without it the run prints `SKIPPED` and
//! exits 0, unless `SPOT_REQUIRE_PG=1`, which makes a missing server a failure.

use std::process::ExitCode;
use std::sync::Arc;

use spot_l7_invariants::history::Level;
use spot_l7_invariants::pg::pg_env;
use spot_l7_invariants::schedule::Profile;
use spot_l8_isolation::backends::{run_level_weak, run_matrix};
use spot_l8_isolation::checker::check;
use spot_l8_isolation::history::Workload;
use spot_l8_isolation::levels::{LEVELS_STRONGEST_FIRST, parse_level};
use spot_l8_isolation::model::Model;
use spot_l8_isolation::weak::weakness_from_name;
use spot_l8_isolation::workload::{WorkloadConfig, run_on};
use spot_storage_fake::FakeBackend;

const USAGE: &str = "usage:
  spot-l8 run --backend fake|pg --level rc|rr|ser --workload list-append|rw-register \
--seed N [--txns N] [--workers W] [--keys K] [--report out.json] [--history out.jsonl]
  spot-l8 check history.jsonl --claimed LEVEL [--report out.json]
  spot-l8 matrix --seed N [--workload list-append|rw-register] [--report out.json]
  spot-l8 weak --weakness NAME --level rc|rr|ser --workload list-append|rw-register \
--seed N [--txns N] [--workers W] [--report out.json]";

struct RunArgs {
    backend: String,
    level: Level,
    config: WorkloadConfig,
    report: Option<String>,
    history: Option<String>,
}

fn value<'a>(flag: &str, it: &mut std::slice::Iter<'a, String>) -> Result<String, String> {
    it.next()
        .cloned()
        .ok_or_else(|| format!("{flag} needs a value\n{USAGE}"))
}

fn number<T: std::str::FromStr>(flag: &str, raw: String) -> Result<T, String> {
    raw.parse()
        .map_err(|_| format!("{flag}: not a number (`{raw}`)\n{USAGE}"))
}

fn parse_run(args: &[String]) -> Result<RunArgs, String> {
    let mut backend = "fake".to_owned();
    let mut level = Level::ReadCommitted;
    let mut workload = Workload::ListAppend;
    let mut seed = 1u64;
    let mut config: Option<WorkloadConfig> = None;
    let mut txn_over = None;
    let mut worker_over = None;
    let mut key_over = None;
    let mut report = None;
    let mut history = None;

    let mut it = args.iter();
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--backend" => backend = value(flag, &mut it)?,
            "--level" => {
                let v = value(flag, &mut it)?;
                level = parse_level(&v).ok_or_else(|| format!("unknown level `{v}`"))?;
            }
            "--workload" => {
                let v = value(flag, &mut it)?;
                workload = Workload::parse(&v).ok_or_else(|| format!("unknown workload `{v}`"))?;
            }
            "--seed" => seed = number(flag, value(flag, &mut it)?)?,
            "--txns" => txn_over = Some(number(flag, value(flag, &mut it)?)?),
            "--workers" => worker_over = Some(number(flag, value(flag, &mut it)?)?),
            "--keys" => key_over = Some(number(flag, value(flag, &mut it)?)?),
            "--profile" => {
                let v = value(flag, &mut it)?;
                let p = Profile::parse(&v).ok_or_else(|| format!("unknown profile `{v}`"))?;
                config = Some(WorkloadConfig::for_profile(p, workload, seed));
            }
            "--report" => report = Some(value(flag, &mut it)?),
            "--history" => history = Some(value(flag, &mut it)?),
            other => return Err(format!("unknown argument `{other}`\n{USAGE}")),
        }
    }
    let mut config = config.unwrap_or_else(|| {
        WorkloadConfig::for_profile(WorkloadConfig::profile_from_env(), workload, seed)
    });
    config.workload = workload;
    config.seed = seed;
    if let Some(t) = txn_over {
        config.transactions = t;
    }
    if let Some(w) = worker_over {
        config.workers = w;
    }
    if let Some(k) = key_over {
        config.keys = k;
    }
    if backend != "fake" && backend != "pg" {
        return Err(format!("unknown backend `{backend}`\n{USAGE}"));
    }
    Ok(RunArgs {
        backend,
        level,
        config,
        report,
        history,
    })
}

fn write_file(path: &str, text: &str) -> Result<(), String> {
    std::fs::write(path, text).map_err(|e| format!("cannot write {path}: {e}"))
}

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(command) = args.first().map(String::as_str) else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    match command {
        "run" => run(&args[1..]).await,
        "check" => check_history(&args[1..]),
        "matrix" => matrix(&args[1..]).await,
        "weak" => weak(&args[1..]).await,
        other => {
            eprintln!("unknown command `{other}`\n{USAGE}");
            ExitCode::from(2)
        }
    }
}

async fn run(args: &[String]) -> ExitCode {
    let a = match parse_run(args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    let model = Arc::new(Model::default());
    let result = if a.backend == "pg" {
        let Some(env) = pg_env(a.config.workers).await else {
            println!("SKIPPED: set SPOT_TEST_PG_URL to run on PostgreSQL");
            return ExitCode::SUCCESS;
        };
        println!("PostgreSQL pool of {} connections", env.pool_size);
        run_on(env.backend.clone(), model, &a.config, a.level).await
    } else {
        run_on(Arc::new(FakeBackend::new()), model, &a.config, a.level).await
    };
    let history = match result {
        Ok(h) => h,
        Err(e) => {
            eprintln!("the run failed: {e}");
            return ExitCode::from(2);
        }
    };
    if let Some(path) = &a.history
        && let Err(e) = std::fs::File::create(path)
            .and_then(|f| history.write_jsonl(&mut std::io::BufWriter::new(f)))
    {
        eprintln!("cannot write {path}: {e}");
        return ExitCode::from(2);
    }
    let report = check(&history, a.level);
    print!("{}", report.to_text());
    if let Some(path) = &a.report
        && let Err(e) = write_file(path, &report.to_json())
    {
        eprintln!("{e}");
        return ExitCode::from(2);
    }
    if report.ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

fn check_history(args: &[String]) -> ExitCode {
    let Some(path) = args.first() else {
        eprintln!("check needs a history file\n{USAGE}");
        return ExitCode::from(2);
    };
    let mut claimed = Level::ReadCommitted;
    let mut report = None;
    let mut it = args[1..].iter();
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--claimed" => {
                let v = match value(flag, &mut it) {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("{e}");
                        return ExitCode::from(2);
                    }
                };
                claimed = match parse_level(&v) {
                    Some(l) => l,
                    None => {
                        eprintln!("unknown level `{v}`");
                        return ExitCode::from(2);
                    }
                };
            }
            "--report" => match value(flag, &mut it) {
                Ok(v) => report = Some(v),
                Err(e) => {
                    eprintln!("{e}");
                    return ExitCode::from(2);
                }
            },
            other => {
                eprintln!("unknown argument `{other}`\n{USAGE}");
                return ExitCode::from(2);
            }
        }
    }
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("cannot read {path}: {e}");
            return ExitCode::from(2);
        }
    };
    let history = match spot_l8_isolation::history::TxnHistory::read_jsonl(&text) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("{path}: {e}");
            return ExitCode::from(2);
        }
    };
    let result = check(&history, claimed);
    print!("{}", result.to_text());
    if let Some(p) = &report
        && let Err(e) = write_file(p, &result.to_json())
    {
        eprintln!("{e}");
        return ExitCode::from(2);
    }
    if result.ok() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

async fn matrix(args: &[String]) -> ExitCode {
    let mut workload = Workload::ListAppend;
    let mut seed = 1u64;
    let mut report = None;
    let mut profile = WorkloadConfig::profile_from_env();
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--workload" => {
                let v = match value(flag, &mut it) {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("{e}");
                        return ExitCode::from(2);
                    }
                };
                workload = match Workload::parse(&v) {
                    Some(w) => w,
                    None => {
                        eprintln!("unknown workload `{v}`");
                        return ExitCode::from(2);
                    }
                };
            }
            "--seed" => match value(flag, &mut it).and_then(|v| number(flag, v)) {
                Ok(s) => seed = s,
                Err(e) => {
                    eprintln!("{e}");
                    return ExitCode::from(2);
                }
            },
            "--profile" => {
                let v = match value(flag, &mut it) {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("{e}");
                        return ExitCode::from(2);
                    }
                };
                profile = match Profile::parse(&v) {
                    Some(p) => p,
                    None => {
                        eprintln!("unknown profile `{v}`");
                        return ExitCode::from(2);
                    }
                };
            }
            "--report" => match value(flag, &mut it) {
                Ok(v) => report = Some(v),
                Err(e) => {
                    eprintln!("{e}");
                    return ExitCode::from(2);
                }
            },
            other => {
                eprintln!("unknown argument `{other}`\n{USAGE}");
                return ExitCode::from(2);
            }
        }
    }
    let config = WorkloadConfig::for_profile(profile, workload, seed);
    let m = run_matrix(Arc::new(Model::default()), &config).await;
    print!("{}", m.to_text());
    if let Some(p) = &report
        && let Err(e) = write_file(p, &m.to_json())
    {
        eprintln!("{e}");
        return ExitCode::from(2);
    }
    if m.meets_expectation() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

async fn weak(args: &[String]) -> ExitCode {
    let mut weakness_name = String::new();
    let mut level = Level::ReadCommitted;
    let mut workload = Workload::ListAppend;
    let mut seed = 1u64;
    let mut profile = Profile::Smoke;
    let mut report = None;
    let mut txn_over = None;
    let mut worker_over = None;
    let mut key_over = None;
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--weakness" => weakness_name = value(flag, &mut it).unwrap_or_default(),
            "--level" => {
                let raw = value(flag, &mut it).unwrap_or_default();
                level = parse_level(&raw).unwrap_or(Level::ReadCommitted);
            }
            "--workload" => {
                let raw = value(flag, &mut it).unwrap_or_default();
                workload = Workload::parse(&raw).unwrap_or(Workload::ListAppend);
            }
            "--seed" => {
                seed = value(flag, &mut it)
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(1)
            }
            "--txns" => txn_over = value(flag, &mut it).ok().and_then(|s| s.parse().ok()),
            "--workers" => worker_over = value(flag, &mut it).ok().and_then(|s| s.parse().ok()),
            "--keys" => key_over = value(flag, &mut it).ok().and_then(|s| s.parse().ok()),
            "--profile" => {
                let raw = value(flag, &mut it).unwrap_or_default();
                profile = Profile::parse(&raw).unwrap_or(Profile::Smoke);
            }
            "--report" => report = value(flag, &mut it).ok(),
            other => {
                eprintln!("unknown argument `{other}`\n{USAGE}");
                return ExitCode::from(2);
            }
        }
    }
    let Some(weakness) = weakness_from_name(&weakness_name) else {
        eprintln!("unknown weakness `{weakness_name}`\n{USAGE}");
        return ExitCode::from(2);
    };
    let mut config = WorkloadConfig::for_profile(profile, workload, seed);
    if let Some(t) = txn_over {
        config.transactions = t;
    }
    if let Some(w) = worker_over {
        config.workers = w;
    }
    if let Some(k) = key_over {
        config.keys = k;
    }
    let model = Arc::new(Model::default());
    let result = run_level_weak(model, &config, level, weakness).await;
    let report_data = match result {
        Ok(r) => r,
        Err(e) => {
            eprintln!("the run failed: {e}");
            return ExitCode::from(2);
        }
    };
    print!("{}", report_data.to_text());
    if let Some(p) = &report
        && let Err(e) = write_file(p, &report_data.to_json())
    {
        eprintln!("{e}");
        return ExitCode::from(2);
    }
    // A weakened backend is expected to show an anomaly; that is a success for this command.
    if report_data.violations.is_empty() {
        eprintln!(
            "warning: weakness {} produced no forbidden anomaly at {}",
            weakness.name(),
            spot_l8_isolation::levels::level_name(level)
        );
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

/// The levels the checker accepts, for the usage text.
pub fn levels() -> [Level; 3] {
    LEVELS_STRONGEST_FIRST
}
