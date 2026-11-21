//! `l9-scale run --profile <name|file> [options]`: run a profile and judge it against the
//! budgets. Exit status: 0 all budgets met, 1 a budget breached, 2 the run could not be made.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use l9_scale::attach;
use l9_scale::budget::Budgets;
use l9_scale::launcher::{self, LaunchOptions};
use l9_scale::profile::Profile;
use l9_scale::{Mode, RunOptions, execute};

const USAGE: &str = "\
usage: l9-scale run --profile <name|file> [options]

  --profile P        a profile file, or the name of one in harness/rust/l9-scale/profiles/
  --budgets FILE     budgets file (default: harness/rust/l9-scale/budgets.toml)
  --out DIR          write report.json and report.md here (default: l9-report)
  --run-id ID        the run's id (default: random)
  --repo DIR         repository root holding apps/platform and apps/sample (default: this build's)
  --spot-bin FILE    the `spot` binary (default: next to this program, else built with cargo)
  --keep             keep the scratch site and database after the run
  --attach URL       load an existing deployment instead (http://host[:port])
  --site NAME        the site's name for the Host header (with --attach)
  --users FILE       lines of `email password` (with --attach)
  --allow-hosts FILE hosts besides loopback that --attach may target
  --i-know-this-is-a-test-system
                     let --attach target a host that is neither loopback nor allow-listed

The launched mode needs SPOT_TEST_PG_URL (a PostgreSQL server that may create databases).
";

#[derive(Default)]
struct Args {
    profile: Option<String>,
    budgets: Option<PathBuf>,
    out: Option<PathBuf>,
    run_id: Option<String>,
    repo: Option<PathBuf>,
    spot_bin: Option<PathBuf>,
    keep: bool,
    attach: Option<String>,
    site: Option<String>,
    users: Option<PathBuf>,
    allow_hosts: Option<PathBuf>,
    override_flag: bool,
}

fn parse(args: &[String]) -> Result<Args, String> {
    let mut out = Args::default();
    let mut it = args.iter();
    match it.next().map(String::as_str) {
        Some("run") => {}
        _ => return Err("expected the command `run`".to_owned()),
    }
    while let Some(flag) = it.next() {
        let mut value = |name: &str| {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match flag.as_str() {
            "--profile" => out.profile = Some(value("--profile")?),
            "--budgets" => out.budgets = Some(value("--budgets")?.into()),
            "--out" => out.out = Some(value("--out")?.into()),
            "--run-id" => out.run_id = Some(value("--run-id")?),
            "--repo" => out.repo = Some(value("--repo")?.into()),
            "--spot-bin" => out.spot_bin = Some(value("--spot-bin")?.into()),
            "--attach" => out.attach = Some(value("--attach")?),
            "--site" => out.site = Some(value("--site")?),
            "--users" => out.users = Some(value("--users")?.into()),
            "--allow-hosts" => out.allow_hosts = Some(value("--allow-hosts")?.into()),
            "--keep" => out.keep = true,
            "--i-know-this-is-a-test-system" => out.override_flag = true,
            other => return Err(format!("unknown option `{other}`")),
        }
    }
    Ok(out)
}

fn crate_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_owned()
}

fn profile_path(given: &str) -> PathBuf {
    if given.contains(['/', '\\']) || given.ends_with(".toml") {
        PathBuf::from(given)
    } else {
        crate_dir().join("profiles").join(format!("{given}.toml"))
    }
}

async fn run(args: Args) -> Result<bool, String> {
    let profile_name = args.profile.ok_or("--profile is required")?;
    let profile = Profile::load(&profile_path(&profile_name)).map_err(|e| e.to_string())?;
    let budgets_path = args
        .budgets
        .unwrap_or_else(|| crate_dir().join("budgets.toml"));
    let budgets = Budgets::load(&budgets_path).map_err(|e| e.to_string())?;
    let run_id = args.run_id.unwrap_or_else(|| launcher::random_hex(12));
    launcher::validate_run_id(&run_id)?;
    let mode = match args.attach {
        None => Mode::Launch(LaunchOptions {
            repo: args.repo.unwrap_or_else(launcher::default_repo),
            spot_bin: args.spot_bin,
            users: profile.sessions,
            run_id: run_id.clone(),
            keep: args.keep,
        }),
        Some(url) => {
            let parsed = attach::parse_url(&url)?;
            let allow = match &args.allow_hosts {
                Some(path) => attach::parse_allow_list(
                    &std::fs::read_to_string(path)
                        .map_err(|e| format!("cannot read {}: {e}", path.display()))?,
                ),
                None => Vec::new(),
            };
            attach::check_host(&parsed.host, &allow, args.override_flag)?;
            let users_file = args.users.ok_or("--attach needs --users FILE")?;
            let users = attach::load_users(&users_file)?;
            let target = attach::resolve(&parsed, args.site.as_deref()).await?;
            Mode::Attach { target, users }
        }
    };
    let out_dir = args.out.unwrap_or_else(|| PathBuf::from("l9-report"));
    let finished = execute(RunOptions {
        profile,
        budgets,
        mode,
        run_id,
        out_dir: Some(out_dir.clone()),
    })
    .await?;
    let Some(finished) = finished else {
        return Err("no PostgreSQL: set SPOT_TEST_PG_URL (or use --attach)".to_owned());
    };
    let report = finished.report;
    println!("{}", report.to_markdown());
    println!("Report written to {}", out_dir.display());
    for verdict in report.budgets.iter().filter(|v| !v.passed) {
        eprintln!(
            "BUDGET BREACHED: profile `{}` operation `{}` {}: limit {}, measured {}",
            report.profile, verdict.operation, verdict.metric, verdict.limit, verdict.measured
        );
    }
    Ok(report.passed)
}

#[tokio::main]
async fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.iter().any(|a| a == "--help" || a == "-h") {
        print!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let args = match parse(&argv) {
        Ok(args) => args,
        Err(error) => {
            eprintln!("l9-scale: {error}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match run(args).await {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(error) => {
            eprintln!("l9-scale: {error}");
            ExitCode::from(2)
        }
    }
}
