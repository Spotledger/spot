//! The `soak` profile (WP-TST-15.T5): half of tier M for two hours (scaled for the test), with
//! periodic events, sampled every 15 s, judged by `src/leak.rs` against `budgets/soak.toml`.
//!
//! Needs `SPOT_TEST_PG_URL`; without it the PostgreSQL test prints why and returns, never
//! faked. `L9_DURATION_SCALE` (default `0.025`: a 180 s steady phase) scales the run and the
//! cadence of its events; the full two hour run is nightly (WP-TST-15.T8). With
//! `L9_SOAK_STRICT=1` an event the server cannot serve (no custom-field operation for this
//! user, no backup command) fails the run instead of being recorded as unavailable.
//!
//! The report (`soak-report.md` and `soak-series.csv`, in `L9_SOAK_OUT` or the temp directory)
//! holds counts, resource readings, the fitted slopes and the first breached sample: no query
//! text, header or payload.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

#[path = "../src/leak.rs"]
mod leak;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use l9_scale::budget::Budgets;
use l9_scale::clock::{Clock, MonotonicClock};
use l9_scale::http::{Conn, Target};
use l9_scale::launcher::{self, LaunchOptions, Launched};
use l9_scale::profile::{Profile, parse_decimal};
use l9_scale::runner::Sampler;
use l9_scale::{Mode, RunOptions, execute};
use serde_json::{Value as Json, json};
use spot_postgres::testing::TestDb;

use leak::{Limits, Sample};

const SCALE_ENV: &str = "L9_DURATION_SCALE";
const STRICT_ENV: &str = "L9_SOAK_STRICT";
const OUT_ENV: &str = "L9_SOAK_OUT";
const DEFAULT_PERMILLE: u64 = 25;
const TIMEOUT: Duration = Duration::from_secs(30);
/// Metric names the soak reads when the server exports them (summed over labels); a name the
/// server does not export leaves its series "not measured".
const BACKLOG_METRICS: [&str; 2] = ["spot_jobs_pending", "spot_job_backlog"];
const OUTBOX_LAG_MS_METRICS: [&str; 1] = ["spot_outbox_lag_ms"];

fn crate_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn scale_permille() -> u64 {
    std::env::var(SCALE_ENV)
        .ok()
        .and_then(|v| parse_decimal(&v, 3))
        .map_or(DEFAULT_PERMILLE, |n| n.clamp(1, 1000))
}

fn load_profile() -> Profile {
    Profile::load(&crate_dir().join("profiles/soak.toml")).unwrap()
}

fn load_limits() -> Limits {
    Limits::from_toml(&std::fs::read_to_string(crate_dir().join("budgets/soak.toml")).unwrap())
        .unwrap()
}

/// The latency and error budgets of the soak itself (the leak limits are in `budgets/soak.toml`).
fn run_budgets() -> Budgets {
    Budgets::from_toml(
        "[[budget]]\nprofile = \"soak\"\noperation = \"all\"\nmax_error_rate = 0.001\n\
         [[budget]]\nprofile = \"soak\"\noperation = \"create_submit\"\np95_ms = 1000\n",
    )
    .unwrap()
}

// --- the event schedule --------------------------------------------------------------------------

/// A periodic event of the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Event {
    MetadataChange,
    CrossSiteCheck,
    SignWave,
    Backup,
    Report,
    Reload,
}

impl Event {
    fn name(self) -> &'static str {
        match self {
            Event::MetadataChange => "metadata_change",
            Event::CrossSiteCheck => "cross_site_check",
            Event::SignWave => "sign_out_in_wave",
            Event::Backup => "backup",
            Event::Report => "report",
            Event::Reload => "reload",
        }
    }
}

/// The events and their period in seconds of a full run.
const SCHEDULE: [(Event, u64); 6] = [
    (Event::MetadataChange, 600),
    (Event::CrossSiteCheck, 600),
    (Event::SignWave, 900),
    (Event::Backup, 1800),
    (Event::Report, 1800),
    (Event::Reload, 1800),
];

/// The period of `full` seconds scaled by `permille` thousandths, never under 10 s.
fn scaled_period(full: u64, permille: u64) -> u64 {
    (full.saturating_mul(permille) / 1000).max(10)
}

/// The events due at `now_s` given when each last fired (`None` = never): an event is due one
/// period after the run starts and one period after its last firing.
fn due(last: &[Option<u64>; 6], now_s: u64, permille: u64) -> Vec<Event> {
    SCHEDULE
        .iter()
        .zip(last)
        .filter(|((_, full), last)| {
            now_s
                >= last
                    .unwrap_or(0)
                    .saturating_add(scaled_period(*full, permille))
        })
        .map(|((event, _), _)| *event)
        .collect()
}

#[derive(Debug, Clone)]
enum Outcome {
    Done,
    Unavailable(String),
    Failed(String),
}

// --- helpers ------------------------------------------------------------------------------------

async fn sign_in(
    conn: &mut Conn,
    target: &Target,
    email: &str,
    password: &str,
) -> Result<String, String> {
    let body = json!({ "user": email, "password": password }).to_string();
    let r = conn
        .request(
            target,
            "POST",
            "/api/v1/auth/login",
            &[("Content-Type", "application/json")],
            Some(body.as_bytes()),
            TIMEOUT,
        )
        .await
        .map_err(|e| format!("sign-in failed: {e:?}"))?;
    if r.status != 200 {
        return Err(format!("sign-in answered {}", r.status));
    }
    let token = r
        .json()
        .pointer("/data/token")
        .and_then(Json::as_str)
        .map(str::to_owned)
        .ok_or_else(|| "sign-in carried no token".to_owned())?;
    Ok(format!("Bearer {token}"))
}

async fn call(
    conn: &mut Conn,
    target: &Target,
    bearer: &str,
    method: &str,
    path: &str,
    body: Option<&Json>,
) -> Result<(u16, Vec<u8>), String> {
    let bytes = body.map(|b| b.to_string().into_bytes());
    let r = conn
        .request(
            target,
            method,
            path,
            &[
                ("Authorization", bearer),
                ("Content-Type", "application/json"),
            ],
            bytes.as_deref(),
            TIMEOUT,
        )
        .await
        .map_err(|e| format!("{method} {path} failed: {e:?}"))?;
    Ok((r.status, r.body))
}

/// The sum of the values of the metric `name` (any labels) in Prometheus text, integer part
/// only; `None` when no line carries it.
fn metric_sum(text: &str, name: &str) -> Option<u64> {
    let mut found = false;
    let mut sum = 0u64;
    for line in text.lines() {
        let Some(rest) = line.strip_prefix(name) else {
            continue;
        };
        if !(rest.starts_with('{') || rest.starts_with(' ')) {
            continue;
        }
        let Some(value) = line.rsplit(' ').next() else {
            continue;
        };
        let whole = value.split('.').next().unwrap_or("0");
        if let Ok(n) = whole.parse::<u64>() {
            found = true;
            sum = sum.saturating_add(n);
        }
    }
    found.then_some(sum)
}

/// `spot_lease_events_total` lines for `event` summed over lease kinds.
fn lease_events(text: &str, event: &str) -> u64 {
    let needle = format!("event=\"{event}\"");
    text.lines()
        .filter(|l| l.starts_with("spot_lease_events_total{") && l.contains(&needle))
        .filter_map(|l| l.rsplit(' ').next())
        .filter_map(|v| v.split('.').next()?.parse::<u64>().ok())
        .fold(0, u64::saturating_add)
}

/// The size in kB of every file below `dir`.
fn dir_kb(dir: &Path) -> u64 {
    fn walk(dir: &Path, depth: u32) -> u64 {
        if depth > 12 {
            return 0;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return 0;
        };
        entries
            .flatten()
            .map(|e| match e.metadata() {
                Ok(m) if m.is_dir() => walk(&e.path(), depth + 1),
                Ok(m) => m.len(),
                Err(_) => 0,
            })
            .fold(0u64, u64::saturating_add)
    }
    walk(dir, 0) / 1024
}

struct Site {
    launched: Launched,
    bearer: String,
    marker: String,
}

async fn start_site(users: u32) -> Option<Site> {
    let launched = Launched::start(&LaunchOptions {
        repo: launcher::default_repo(),
        spot_bin: None,
        users,
        run_id: launcher::random_hex(12),
        keep: false,
    })
    .await
    .unwrap_or_else(|e| panic!("cannot start a scratch deployment: {e}"))?;
    let user = launched.users.first().expect("users").clone();
    let mut conn = Conn::new();
    let bearer = sign_in(
        &mut conn,
        &launched.target,
        &user.email,
        user.password.expose(),
    )
    .await
    .unwrap();
    let marker = format!("L9SOAK{}", launcher::random_hex(16));
    let (_, me) = call(
        &mut conn,
        &launched.target,
        &bearer,
        "GET",
        "/api/v1/auth/session",
        None,
    )
    .await
    .unwrap();
    let owner = serde_json::from_slice::<Json>(&me)
        .ok()
        .and_then(|j| {
            j.pointer("/data/user/id")
                .and_then(Json::as_str)
                .map(str::to_owned)
        })
        .expect("the session answer names a user id");
    let project = json!({
        "code": marker, "title": format!("{marker} project"), "status": "Active",
        "owner": owner, "currency": "USD", "budget": "1000.00"
    });
    let (status, _) = call(
        &mut conn,
        &launched.target,
        &bearer,
        "POST",
        "/api/v1/resource/project",
        Some(&project),
    )
    .await
    .unwrap();
    assert_eq!(status, 201, "the marker project answered {status}");
    Some(Site {
        launched,
        bearer,
        marker,
    })
}

async fn run_spot(bin: &Path, root: &Path, args: &[&str]) -> Outcome {
    let output = tokio::process::Command::new(bin)
        .arg("--root")
        .arg(root)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await;
    match output {
        Ok(o) if o.status.success() => Outcome::Done,
        // Only the exit status and the first line of the error stream (never standard output).
        Ok(o) => Outcome::Unavailable(format!(
            "`spot {}` exited {}: {}",
            args.first().copied().unwrap_or(""),
            o.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&o.stderr)
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(200)
                .collect::<String>()
        )),
        Err(e) => Outcome::Failed(format!("cannot start spot: {e}")),
    }
}

/// Everything an event needs.
struct World {
    a: Site,
    b: Site,
    bin: PathBuf,
    root: PathBuf,
    site: String,
}

async fn fire(world: &World, event: Event, n: u64) -> Outcome {
    let a = &world.a;
    let mut conn = Conn::new();
    match event {
        Event::MetadataChange => {
            let body = json!({
                "doctype": "task",
                "field": { "name": format!("l9_soak_{n}"), "label": format!("L9 soak {n}"), "type": "text" }
            });
            match call(
                &mut conn,
                &a.launched.target,
                &a.bearer,
                "POST",
                "/api/v1/method/platform/customise/add_custom_field",
                Some(&body),
            )
            .await
            {
                Ok((200 | 201, _)) => Outcome::Done,
                Ok((status, _)) => {
                    Outcome::Unavailable(format!("add_custom_field answered {status}"))
                }
                Err(e) => Outcome::Failed(e),
            }
        }
        Event::CrossSiteCheck => {
            let body = json!({ "limit": 1000 });
            for (site, other) in [(&world.a, &world.b), (&world.b, &world.a)] {
                let mut conn = Conn::new();
                match call(
                    &mut conn,
                    &site.launched.target,
                    &site.bearer,
                    "POST",
                    "/api/v1/resource/project/query",
                    Some(&body),
                )
                .await
                {
                    Ok((200, raw)) => {
                        let text = String::from_utf8_lossy(&raw);
                        if text.contains(&other.marker) {
                            return Outcome::Failed(
                                "a site's list carried the other site's data".to_owned(),
                            );
                        }
                    }
                    Ok((status, _)) => {
                        return Outcome::Failed(format!("project list answered {status}"));
                    }
                    Err(e) => return Outcome::Failed(e),
                }
            }
            Outcome::Done
        }
        Event::SignWave => {
            for user in a.launched.users.iter().take(10) {
                let token = match sign_in(
                    &mut conn,
                    &a.launched.target,
                    &user.email,
                    user.password.expose(),
                )
                .await
                {
                    Ok(t) => t,
                    Err(e) => return Outcome::Failed(e),
                };
                if let Err(e) = call(
                    &mut conn,
                    &a.launched.target,
                    &token,
                    "POST",
                    "/api/v1/auth/logout",
                    None,
                )
                .await
                {
                    return Outcome::Failed(e);
                }
            }
            Outcome::Done
        }
        Event::Report => match call(
            &mut conn,
            &a.launched.target,
            &a.bearer,
            "POST",
            "/api/v1/report/task_summary/run",
            Some(&json!({ "limit": 50 })),
        )
        .await
        {
            // 404: the sample app ships no report definition yet (the same accepted answer as
            // the session's `report_run`).
            Ok((200 | 404, _)) => Outcome::Done,
            Ok((status, _)) => Outcome::Failed(format!("the report answered {status}")),
            Err(e) => Outcome::Failed(e),
        },
        Event::Backup => run_spot(&world.bin, &world.root, &["backup", "run", &world.site]).await,
        Event::Reload => run_spot(&world.bin, &world.root, &["reload", &world.site]).await,
    }
}

// --- the run -------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn the_soak_runs_with_its_events_and_the_series_stay_within_the_leak_budgets() {
    let permille = scale_permille();
    let strict = std::env::var(STRICT_ENV).is_ok_and(|v| v.trim() == "1");
    let mut profile = load_profile();
    profile.ramp_seconds = (profile.ramp_seconds * permille / 1000).max(1);
    profile.steady_seconds = (profile.steady_seconds * permille / 1000).max(30);
    let interval_s: u64 = if permille >= 500 { 15 } else { 5 };

    let Some(a) = start_site(profile.sessions).await else {
        println!("SKIPPED: the l9-scale soak run needs SPOT_TEST_PG_URL");
        return;
    };
    let b = start_site(2).await.expect("the first site started");
    let wal_db = TestDb::create().await.expect("PostgreSQL is configured");
    let repo = launcher::default_repo();
    let bin = launcher::locate_spot(&repo, None).await.unwrap();
    // The scratch deployment's directory and site name are rebuilt from the run id the
    // launcher used (the same convention `tests/volume.rs` documents); the run id is the
    // suffix of the site name in the Host header.
    let site = a
        .launched
        .target
        .host
        .split(':')
        .next()
        .unwrap_or("")
        .to_owned();
    let id = site.strip_prefix("l9-scale-").unwrap_or("").to_owned();
    let root = std::env::temp_dir().join(format!("l9_scale_{id}"));
    let sampler: Arc<dyn Sampler> = a.launched.sampler();
    let world = Arc::new(World {
        a,
        b,
        bin,
        root: root.clone(),
        site,
    });

    let metrics_text = |bearer: String, target: Target| async move {
        let mut conn = Conn::new();
        match call(&mut conn, &target, &bearer, "GET", "/metrics", None).await {
            Ok((200, raw)) => Some(String::from_utf8_lossy(&raw).into_owned()),
            _ => None,
        }
    };
    let before = metrics_text(world.a.bearer.clone(), world.a.launched.target.clone()).await;

    let clock = MonotonicClock::new();
    let stop = Arc::new(AtomicBool::new(false));
    let samples: Arc<Mutex<Vec<Sample>>> = Arc::new(Mutex::new(Vec::new()));
    let events: Arc<Mutex<Vec<(u64, Event, Outcome)>>> = Arc::new(Mutex::new(Vec::new()));
    let wal_base = Arc::new(Mutex::new(None::<u64>));

    let driver = {
        let (stop, samples, events, world) =
            (stop.clone(), samples.clone(), events.clone(), world.clone());
        let wal_base = wal_base.clone();
        let wal_client = wal_db.admin_client().await;
        let clock = &clock;
        let root = root.clone();
        async move {
            let mut last: [Option<u64>; 6] = [None; 6];
            let mut next_sample = 0u64;
            let mut count = 0u64;
            let mut handles = Vec::new();
            loop {
                let finished = stop.load(Ordering::SeqCst);
                let now_s = clock.now_ns() / 1_000_000_000;
                if now_s >= next_sample || finished {
                    next_sample = now_s + interval_s;
                    let reading = sampler.sample().await;
                    let text =
                        metrics_text(world.a.bearer.clone(), world.a.launched.target.clone()).await;
                    let from_metrics = |names: &[&str]| {
                        text.as_deref()
                            .and_then(|t| names.iter().find_map(|n| metric_sum(t, n)))
                    };
                    let wal = wal_client
                        .query_one(
                            "SELECT (pg_current_wal_lsn() - '0/0'::pg_lsn)::bigint / 1024",
                            &[],
                        )
                        .await
                        .ok()
                        .and_then(|row| row.try_get::<_, i64>(0).ok())
                        .and_then(|n| u64::try_from(n).ok());
                    let wal_kb = wal.map(|now| {
                        let mut base = wal_base.lock().unwrap();
                        let base = *base.get_or_insert(now);
                        now.saturating_sub(base)
                    });
                    samples.lock().unwrap().push(Sample {
                        at_s: now_s,
                        rss_kb: reading.rss_kb,
                        open_fds: reading.open_fds,
                        pg_connections: reading.pg_connections,
                        backlog: from_metrics(&BACKLOG_METRICS),
                        outbox_lag_ms: from_metrics(&OUTBOX_LAG_MS_METRICS),
                        wal_kb,
                        log_kb: Some(dir_kb(&root)),
                        ops_total: 0,
                        ops_failed: 0,
                    });
                }
                if finished {
                    break;
                }
                for event in due(&last, now_s, permille) {
                    if let Some(i) = SCHEDULE.iter().position(|(e, _)| *e == event) {
                        last[i] = Some(now_s);
                    }
                    count += 1;
                    let (world, events) = (world.clone(), events.clone());
                    handles.push(tokio::spawn(async move {
                        let outcome = fire(&world, event, count).await;
                        events.lock().unwrap().push((now_s, event, outcome));
                    }));
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            for h in handles {
                let _ = h.await;
            }
        }
    };

    let load = {
        let stop = stop.clone();
        let world = world.clone();
        let steady = profile.steady_seconds;
        async move {
            let result = execute(RunOptions {
                profile,
                budgets: run_budgets(),
                mode: Mode::Attach {
                    target: world.a.launched.target.clone(),
                    users: world.a.launched.users.clone(),
                },
                run_id: launcher::random_hex(12),
                out_dir: None,
            })
            .await;
            stop.store(true, Ordering::SeqCst);
            (result, steady)
        }
    };
    let ((result, steady), ()) = tokio::join!(load, driver);
    let report = result
        .unwrap_or_else(|e| panic!("the soak run failed to start: {e}"))
        .expect("Mode::Attach never returns None")
        .report;
    let after = metrics_text(world.a.bearer.clone(), world.a.launched.target.clone()).await;

    // Cumulative operation counts per sample, from the run's per-second timeline.
    let mut series = samples.lock().unwrap().clone();
    for s in &mut series {
        let (total, failed) = report
            .timeline
            .iter()
            .filter(|p| p.second <= s.at_s)
            .fold((0u64, 0u64), |(t, f), p| (t + p.completed, f + p.failed));
        s.ops_total = total;
        s.ops_failed = failed;
    }
    let findings = leak::evaluate(&series, &load_limits());
    let table = leak::findings_markdown(&findings);
    let event_log = events.lock().unwrap().clone();
    let mut text = format!(
        "# L9 soak report ({steady} s steady, scale {permille}/1000)\n\n{}\n\n## Events\n\n",
        report.to_markdown()
    );
    for (at, event, outcome) in &event_log {
        text.push_str(&format!("- second {at}: {} {outcome:?}\n", event.name()));
    }
    text.push_str(&format!("\n## Leak budgets\n\n{table}\n"));
    let out = std::env::var(OUT_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    let _ = std::fs::create_dir_all(&out);
    let _ = std::fs::write(out.join("soak-report.md"), &text);
    let _ = std::fs::write(out.join("soak-series.csv"), leak::csv(&series));
    println!("{text}");

    // Events: a failure is always a failure; "unavailable" only fails in strict mode.
    for (at, event, outcome) in &event_log {
        match outcome {
            Outcome::Done => {}
            Outcome::Failed(why) => panic!("event {} at second {at} failed: {why}", event.name()),
            Outcome::Unavailable(why) if strict => {
                panic!(
                    "event {} at second {at} is unavailable: {why}",
                    event.name()
                )
            }
            Outcome::Unavailable(why) => {
                println!("l9 soak: event {} unavailable: {why}", event.name())
            }
        }
    }
    for event in [Event::CrossSiteCheck, Event::SignWave, Event::Report] {
        assert!(
            event_log.iter().any(|(_, e, _)| *e == event),
            "{} never fired in a run of {steady} s",
            event.name()
        );
    }
    // No lease lost during the run (OPS-01 metrics).
    match (before, after) {
        (Some(b), Some(a)) => {
            for kind in ["lost", "fence_refused"] {
                assert_eq!(
                    lease_events(&a, kind),
                    lease_events(&b, kind),
                    "lease events of kind `{kind}` happened during the soak"
                );
            }
        }
        _ => {
            println!(
                "l9 soak: /metrics was not readable with this user: lease metrics not asserted"
            );
            assert!(!strict, "strict soak needs readable lease metrics");
        }
    }
    for finding in &findings {
        // A series is judged only on a run long enough to have a second half worth fitting.
        if series.len() >= 8 {
            assert!(
                !finding.failed(),
                "leak budget breached: {} {} (limit {}, measured {}, first at {:?})",
                finding.series,
                finding.check,
                finding.limit,
                finding.measured,
                finding.first_breach_at_s
            );
        }
    }
    assert!(!report.budgets.is_empty());
    for operation in &report.operations {
        assert_eq!(
            operation.server_errors, 0,
            "{} answered a 5xx",
            operation.name
        );
    }
    drop(world);
}

// --- no-PostgreSQL tests --------------------------------------------------------------------------

fn flat(n: u64, step: u64, rss: impl Fn(u64) -> u64) -> Vec<Sample> {
    (0..n)
        .map(|i| Sample {
            at_s: i * step,
            rss_kb: Some(rss(i * step)),
            open_fds: Some(120),
            pg_connections: Some(12),
            ops_total: i * 1000,
            ops_failed: 0,
            ..Sample::default()
        })
        .collect()
}

#[test]
fn a_flat_series_passes_and_a_leak_of_2_mb_per_10_minutes_fails() {
    let limits = load_limits();
    // 15 s samples over 2 hours; the memory is 300 MB with +-512 kB noise.
    let noisy = |t: u64| 300_000 + (t * 7919 % 1024) / 2;
    let findings = leak::evaluate(&flat(481, 15, noisy), &limits);
    assert!(findings.iter().all(|f| !f.failed()), "{findings:?}");
    let rss = findings.iter().find(|f| f.series == "rss_kb").unwrap();
    assert_eq!(rss.passed, Some(true));

    // 2 MB per 10 minutes = 2048 kB / 600 s.
    let leaking = |t: u64| 300_000 + t * 2048 / 600;
    let findings = leak::evaluate(&flat(481, 15, leaking), &limits);
    let rss = findings.iter().find(|f| f.series == "rss_kb").unwrap();
    assert_eq!(rss.passed, Some(false), "{rss:?}");
    assert!(rss.measured.starts_with("20"), "{}", rss.measured);
    // The first breach is reported, and it lies inside the second half.
    let first = rss.first_breach_at_s.expect("a breach has a first sample");
    assert!(first >= 481 * 15 / 2 - 15, "{first}");
    assert!(leak::findings_markdown(&findings).contains("First budget breach"));
}

#[test]
fn the_slope_ignores_a_spike_and_reads_a_straight_line() {
    let line: Vec<(u64, u64)> = (0..50).map(|i| (i * 10, 1_000 + i * 10 * 3)).collect();
    // 3 units per second = 3000 thousandths.
    assert_eq!(leak::theil_sen_milli(&line), Some(3_000));
    let mut spiked = line.clone();
    spiked[20].1 += 900_000;
    spiked[21].1 += 900_000;
    assert_eq!(leak::theil_sen_milli(&spiked), Some(3_000));
    assert_eq!(leak::theil_sen_milli(&[(5, 1)]), None);
    assert_eq!(leak::theil_sen_milli(&[(5, 1), (5, 9)]), None);
    assert_eq!(leak::per_10min(3_000), 1_800);
}

#[test]
fn spreads_percentiles_growth_and_error_rates_are_judged() {
    let limits = load_limits();
    let mut samples = flat(60, 15, |_| 100_000);
    // Connections that creep upward past the pool bound fail the spread; fds stay flat.
    for (i, s) in samples.iter_mut().enumerate() {
        s.pg_connections = Some(10 + i as u64 * 2);
        // A backlog that grows in every quarter fails "no monotonic growth".
        s.backlog = Some(i as u64 * 10);
        // 2 failures in every 1000 operations: above the 0.1 percent interval limit.
        s.ops_failed = s.ops_total / 500;
    }
    let findings = leak::evaluate(&samples, &limits);
    let failed: Vec<(&str, &str)> = findings
        .iter()
        .filter(|f| f.failed())
        .map(|f| (f.series, f.check))
        .collect();
    assert!(
        failed.contains(&("pg_connections", "max minus min")),
        "{failed:?}"
    );
    assert!(
        failed.contains(&("job_backlog", "no monotonic growth")),
        "{failed:?}"
    );
    assert!(
        failed.contains(&("error_ppm", "highest interval")),
        "{failed:?}"
    );
    assert!(
        !failed.contains(&("open_fds", "max minus min")),
        "{failed:?}"
    );
    // The outbox lag was not exported: not measured, which is not a failure.
    assert!(
        findings
            .iter()
            .any(|f| f.series == "outbox_lag_ms" && f.passed.is_none())
    );

    assert_eq!(
        leak::percentile(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10], 990),
        Some(10)
    );
    assert_eq!(leak::percentile(&[], 990), None);
    assert!(leak::grows_monotonically(&(0..40).collect::<Vec<u64>>()));
    assert!(!leak::grows_monotonically(&[5; 40]));
    assert!(!leak::grows_monotonically(&[1, 2, 3]));
}

#[test]
fn wal_and_log_growth_per_operation_are_bounded() {
    let limits = Limits {
        wal_kb_per_op: Some(2),
        ..Limits::default()
    };
    let mut ok = flat(10, 15, |_| 1);
    for (i, s) in ok.iter_mut().enumerate() {
        s.wal_kb = Some(i as u64 * 1000); // 1 kB per operation
    }
    assert_eq!(leak::evaluate(&ok, &limits)[0].passed, Some(true));
    for (i, s) in ok.iter_mut().enumerate() {
        s.wal_kb = Some(i as u64 * 5000); // 5 kB per operation
    }
    let bad = leak::evaluate(&ok, &limits);
    assert_eq!(bad[0].passed, Some(false));
    assert!(bad[0].first_breach_at_s.is_some());
}

#[test]
fn the_csv_has_one_line_per_sample_and_empty_fields_for_missing_series() {
    let csv = leak::csv(&flat(3, 15, |_| 5));
    let lines: Vec<&str> = csv.lines().collect();
    assert_eq!(lines.len(), 4);
    assert!(lines[0].starts_with("at_s,rss_kb,"));
    assert_eq!(lines[1], "0,5,120,12,,,,,0,0");
}

#[test]
fn the_soak_profile_and_budgets_parse() {
    let profile = load_profile();
    assert_eq!(profile.name, "soak");
    assert_eq!(profile.sessions, 100);
    assert_eq!(profile.steady_seconds, 7200);
    let posting_milli =
        profile.target_milli_tps() * u64::from(profile.mix.create_submit) / profile.mix.total();
    assert_eq!(posting_milli, 5_000, "5 tx/s");
    let limits = load_limits();
    assert_eq!(limits.rss_kb_per_10min, Some(1024));
    assert_eq!(limits.outbox_lag_p99_ms, Some(5000));
    assert!(Limits::from_toml("[leak]\nrss = 1\n").is_err());
    assert!(run_budgets().for_profile("soak").len() == 2);
}

#[test]
fn events_are_due_one_period_after_the_start_and_after_each_firing_scaled_with_the_run() {
    // Full scale: nothing is due in the first 10 minutes; the metadata change and the cross-site
    // check are due at 600 s; the wave at 900 s; backup, report and reload at 1800 s.
    let none = [None; 6];
    assert!(due(&none, 599, 1000).is_empty());
    let at_600 = due(&none, 600, 1000);
    assert_eq!(at_600, [Event::MetadataChange, Event::CrossSiteCheck]);
    assert_eq!(due(&none, 900, 1000).len(), 3);
    assert_eq!(due(&none, 1800, 1000).len(), 6);
    let mut last = none;
    last[0] = Some(600);
    assert!(!due(&last, 700, 1000).contains(&Event::MetadataChange));
    assert!(due(&last, 1200, 1000).contains(&Event::MetadataChange));
    // A 0.025 run: periods of 15, 15, 22, 45, 45, 45 s, never under 10 s.
    assert_eq!(scaled_period(600, 25), 15);
    assert_eq!(scaled_period(1800, 25), 45);
    assert_eq!(scaled_period(600, 1), 10);
}

#[test]
fn metrics_text_is_read_without_floats() {
    let text = "# HELP x y\nspot_lease_held{lease=\"scheduler\"} 1\n\
        spot_lease_events_total{lease=\"scheduler\",event=\"acquired\"} 3\n\
        spot_lease_events_total{lease=\"consumer\",event=\"lost\"} 2\n\
        spot_lease_events_total{lease=\"scheduler\",event=\"lost\"} 1\n\
        spot_jobs_pending{queue=\"a\"} 4.0\nspot_jobs_pending{queue=\"b\"} 6\n\
        spot_jobs_pending_total 99\n";
    assert_eq!(lease_events(text, "lost"), 3);
    assert_eq!(lease_events(text, "fence_refused"), 0);
    assert_eq!(metric_sum(text, "spot_jobs_pending"), Some(10));
    assert_eq!(metric_sum(text, "spot_nothing"), None);
}
