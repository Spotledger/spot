//! `spot-l7`: runs the L7 invariant suite from the command line, for CI and for replaying a seed.
//!
//! ```text
//! spot-l7 run --backend fake|pg --profile smoke|standard|nightly --seed N
//!             [--report out.json] [--history out.jsonl] [--workers W] [--operations N]
//! ```
//!
//! Exit status: 0 when every invariant held, 1 when one was violated, 2 on a usage or setup
//! error. The `pg` backend needs `SPOT_TEST_PG_URL`; without it the run prints `SKIPPED` and
//! exits 0, unless `SPOT_REQUIRE_PG=1`, which makes a missing server a failure. The printed
//! report names the seed: running again with it repeats the operation schedule exactly (the
//! interleaving of workers on real threads is not claimed to repeat).

use std::process::ExitCode;
use std::sync::Arc;

use spot_l7_invariants::driver::DriverConfig;
use spot_l7_invariants::model::Model;
use spot_l7_invariants::pg::pg_env;
use spot_l7_invariants::runner::run_and_check;
use spot_l7_invariants::schedule::Profile;
use spot_storage_fake::FakeBackend;

const USAGE: &str = "usage: spot-l7 run --backend fake|pg --profile smoke|standard|nightly \
--seed N [--report out.json] [--history out.jsonl] [--workers W] [--operations N]";

struct Args {
    backend: String,
    profile: Profile,
    seed: u64,
    report: Option<String>,
    history: Option<String>,
    workers: Option<u32>,
    operations: Option<u64>,
}

fn parse(args: &[String]) -> Result<Args, String> {
    if args.first().map(String::as_str) != Some("run") {
        return Err(USAGE.to_owned());
    }
    let mut out = Args {
        backend: "fake".into(),
        profile: Profile::from_env(),
        seed: 1,
        report: None,
        history: None,
        workers: None,
        operations: None,
    };
    let mut it = args[1..].iter();
    while let Some(flag) = it.next() {
        let mut value = || {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{flag} needs a value\n{USAGE}"))
        };
        match flag.as_str() {
            "--backend" => out.backend = value()?,
            "--profile" => {
                let v = value()?;
                out.profile = Profile::parse(&v).ok_or_else(|| format!("unknown profile `{v}`"))?;
            }
            "--seed" => out.seed = value()?.parse().map_err(|_| "bad --seed".to_owned())?,
            "--report" => out.report = Some(value()?),
            "--history" => out.history = Some(value()?),
            "--workers" => out.workers = Some(value()?.parse().map_err(|_| "bad --workers")?),
            "--operations" => {
                out.operations = Some(value()?.parse().map_err(|_| "bad --operations")?);
            }
            other => return Err(format!("unknown argument `{other}`\n{USAGE}")),
        }
    }
    if out.backend != "fake" && out.backend != "pg" {
        return Err(format!("unknown backend `{}`\n{USAGE}", out.backend));
    }
    Ok(out)
}

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args = match parse(&args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    let model = match Model::new() {
        Ok(m) => Arc::new(m),
        Err(e) => {
            eprintln!("cannot build the model: {e}");
            return ExitCode::from(2);
        }
    };
    let mut config = DriverConfig::for_profile(args.profile, args.seed);
    if let Some(w) = args.workers {
        config.workers = w;
    }
    if let Some(n) = args.operations {
        config.operations = n;
    }
    println!(
        "spot-l7: backend {}, profile {}, {} workers, {} operations, seed {}",
        args.backend, config.profile, config.workers, config.operations, config.seed
    );
    let result = if args.backend == "pg" {
        let Some(env) = pg_env(config.workers).await else {
            println!("SKIPPED: set SPOT_TEST_PG_URL to run on PostgreSQL");
            return ExitCode::SUCCESS;
        };
        println!("PostgreSQL pool of {} connections", env.pool_size);
        run_and_check(env.backend.clone(), model, config, "pg", |_| {}).await
    } else {
        run_and_check(Arc::new(FakeBackend::new()), model, config, "fake", |_| {}).await
    };
    let (done, report) = match result {
        Ok(r) => r,
        Err(e) => {
            eprintln!("the run failed: {e}");
            return ExitCode::from(2);
        }
    };
    print!("{}", report.to_text());
    if let Some(path) = &args.report
        && let Err(e) = std::fs::write(path, report.to_json())
    {
        eprintln!("cannot write {path}: {e}");
        return ExitCode::from(2);
    }
    if let Some(path) = &args.history {
        let written = std::fs::File::create(path)
            .and_then(|f| done.history.write_jsonl(&mut std::io::BufWriter::new(f)));
        if let Err(e) = written {
            eprintln!("cannot write {path}: {e}");
            return ExitCode::from(2);
        }
    }
    if report.ok {
        ExitCode::SUCCESS
    } else {
        println!(
            "replay with: spot-l7 run --backend {} --profile {} --seed {}",
            args.backend,
            args.profile.name(),
            args.seed
        );
        ExitCode::from(1)
    }
}
