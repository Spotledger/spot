//! The `tier_m` profile (WP-TST-15.T2): 200 sessions and 10 tx/s of posting against a real
//! `spot serve` on a real PostgreSQL server, with a second site on the same host loaded at the
//! same time to prove isolation under load.
//!
//! Needs `SPOT_TEST_PG_URL`; without it the PostgreSQL tests print why and return, they are
//! never faked. The run is scaled by `L9_DURATION_SCALE` (a decimal, default `0.02`, so the
//! 30 minute steady phase becomes 36 s); the full profile is the nightly run (WP-TST-15.T8).
//!
//! The budgets of `budgets/tier_m.toml` are the README targets for the large self-hosted
//! runner, so they are enforced only when `L9_RUNNER_LABEL=large`. On any other machine the
//! verdicts are printed and the test asserts what holds everywhere: the budget evaluation
//! ran with the real server, no request failed with a 5xx, no number or id was issued twice,
//! and the other site's data never appeared in a response.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use l9_scale::budget::Budgets;
use l9_scale::http::{Conn, Target};
use l9_scale::launcher::{self, LaunchOptions, Launched};
use l9_scale::profile::{Op, Profile, parse_decimal};
use l9_scale::report::Report;
use l9_scale::{Mode, RunOptions, execute};
use serde_json::{Value as Json, json};

const SCALE_ENV: &str = "L9_DURATION_SCALE";
const RUNNER_ENV: &str = "L9_RUNNER_LABEL";
/// The runner label the README budgets apply to.
const REFERENCE_RUNNER: &str = "large";
/// The default scale: 0.02 of the profile's ramp and steady phase.
const DEFAULT_PERMILLE: u64 = 20;
const TIMEOUT: Duration = Duration::from_secs(30);
/// Rows read per page by the post-run invariant query.
const INVARIANT_PAGE: u64 = 200;
/// The most `time_entry` rows the invariant query reads (a bound, not a sample: a full-scale
/// run posts about 18 000 entries in its steady phase).
const INVARIANT_MAX_ROWS: u64 = 60_000;

fn crate_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn load_profile() -> Profile {
    Profile::load(&crate_dir().join("profiles/tier_m.toml")).unwrap()
}

fn load_budgets() -> Budgets {
    Budgets::load(&crate_dir().join("budgets/tier_m.toml")).unwrap()
}

/// The duration scale in thousandths: `0.02` is 20. Unset, empty or unparsable means the
/// default; the value is kept within 1 to 1000 (a full run).
fn scale_permille() -> u64 {
    std::env::var(SCALE_ENV)
        .ok()
        .and_then(|v| parse_decimal(&v, 3))
        .map_or(DEFAULT_PERMILLE, |n| n.clamp(1, 1000))
}

/// Scales the ramp and the steady phase of `profile` by `permille` thousandths (integers
/// only), never below one second for a phase that was longer than zero.
fn scale_durations(profile: &mut Profile, permille: u64) {
    let scale = |seconds: u64| {
        if seconds == 0 {
            0
        } else {
            (seconds.saturating_mul(permille) / 1000).max(1)
        }
    };
    profile.ramp_seconds = scale(profile.ramp_seconds);
    profile.steady_seconds = scale(profile.steady_seconds);
    profile.sample_every_seconds = profile
        .sample_every_seconds
        .min(profile.steady_seconds.div_ceil(2).max(1));
}

fn enforce_budgets() -> bool {
    std::env::var(RUNNER_ENV).is_ok_and(|v| v.trim() == REFERENCE_RUNNER)
}

/// The header lines a baseline report carries: the runner label, CPU count and model, RAM.
/// Read from `/proc` where it exists (Linux); other platforms print `unknown`.
fn runner_header() -> String {
    let label = std::env::var(RUNNER_ENV).unwrap_or_else(|_| "unlabelled".to_owned());
    let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let cpus = cpuinfo
        .lines()
        .filter(|l| l.starts_with("processor"))
        .count();
    let model = cpuinfo
        .lines()
        .find_map(|l| l.strip_prefix("model name"))
        .and_then(|l| l.split_once(':'))
        .map_or("unknown", |(_, v)| v.trim())
        .to_owned();
    let meminfo = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let mem = meminfo
        .lines()
        .find_map(|l| l.strip_prefix("MemTotal:"))
        .map_or("unknown", str::trim)
        .to_owned();
    format!("runner={label} cpus={cpus} cpu_model=\"{model}\" mem_total={mem}")
}

async fn sign_in(conn: &mut Conn, target: &Target, email: &str, password: &str) -> String {
    let body = json!({ "user": email, "password": password }).to_string();
    let response = conn
        .request(
            target,
            "POST",
            "/api/v1/auth/login",
            &[("Content-Type", "application/json")],
            Some(body.as_bytes()),
            TIMEOUT,
        )
        .await
        .unwrap_or_else(|e| panic!("sign-in failed: {e:?}"));
    assert_eq!(response.status, 200, "sign-in answered {}", response.status);
    let token = response
        .json()
        .pointer("/data/token")
        .and_then(Json::as_str)
        .expect("a sign-in answer always carries a token")
        .to_owned();
    format!("Bearer {token}")
}

async fn call(
    conn: &mut Conn,
    target: &Target,
    bearer: &str,
    method: &str,
    path: &str,
    body: Option<&Json>,
) -> (u16, Vec<u8>) {
    let bytes = body.map(|b| b.to_string().into_bytes());
    let response = conn
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
        .unwrap_or_else(|e| panic!("{method} {path} failed: {e:?}"));
    (response.status, response.body)
}

/// One signed-in site of the isolation check: its target, a bearer and a marker that only this
/// site's data carries.
struct Site {
    launched: Launched,
    bearer: String,
    marker: String,
}

impl Site {
    async fn start(users: u32) -> Option<Site> {
        let run_id = launcher::random_hex(12);
        let launched = Launched::start(&LaunchOptions {
            repo: launcher::default_repo(),
            spot_bin: None,
            users,
            run_id,
            keep: false,
        })
        .await
        .unwrap_or_else(|e| panic!("cannot start a scratch deployment: {e}"))?;
        let user = launched
            .users
            .first()
            .expect("the launcher made users")
            .clone();
        let mut conn = Conn::new();
        let bearer = sign_in(
            &mut conn,
            &launched.target,
            &user.email,
            user.password.expose(),
        )
        .await;
        let marker = format!("L9ISO{}", launcher::random_hex(16));
        let (status, body) = call(
            &mut conn,
            &launched.target,
            &bearer,
            "GET",
            "/api/v1/auth/session",
            None,
        )
        .await;
        assert_eq!(status, 200, "the session lookup answered {status}");
        let owner = serde_json::from_slice::<Json>(&body)
            .ok()
            .and_then(|j| {
                j.pointer("/data/user/id")
                    .and_then(Json::as_str)
                    .map(str::to_owned)
            })
            .expect("the session answer names a user id");
        // A project that only this site holds, named by the marker: any response of the other
        // site that contains the marker is a leak.
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
        .await;
        assert_eq!(status, 201, "the marker project answered {status}");
        Some(Site {
            launched,
            bearer,
            marker,
        })
    }
}

/// True when any response of `site` (its project list) contains `other_marker`; also checks
/// that the site still shows its own marker.
async fn leaks_into(site: &Site, other_marker: &str, conn: &mut Conn) -> bool {
    let (status, body) = call(
        conn,
        &site.launched.target,
        &site.bearer,
        "POST",
        "/api/v1/resource/project/query",
        Some(&json!({ "limit": 1000 })),
    )
    .await;
    assert_eq!(status, 200, "the project list answered {status}");
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains(&site.marker),
        "a site lost sight of its own marker project"
    );
    text.contains(other_marker)
}

/// Polls both sites until `stop` is set (and once more after it), counting probes; returns the
/// number of probes made. Panics on the first leak.
async fn probe_isolation(a: &Site, b: &Site, stop: Arc<AtomicBool>) -> u64 {
    let mut conn_a = Conn::new();
    let mut conn_b = Conn::new();
    let mut probes = 0;
    loop {
        let last = stop.load(Ordering::SeqCst);
        assert!(
            !leaks_into(a, &b.marker, &mut conn_a).await,
            "site A answered with site B's data"
        );
        assert!(
            !leaks_into(b, &a.marker, &mut conn_b).await,
            "site B answered with site A's data"
        );
        probes += 1;
        if last {
            return probes;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// A token minted by one site must not open the other site, even with the same user name.
async fn tokens_do_not_cross(a: &Site, b: &Site) {
    let mut conn = Conn::new();
    for (from, to) in [(a, b), (b, a)] {
        let (status, body) = call(
            &mut conn,
            &to.launched.target,
            &from.bearer,
            "GET",
            "/api/v1/auth/session",
            None,
        )
        .await;
        assert!(
            status == 401 || status == 403,
            "a token of one site answered {status} on the other site"
        );
        assert!(
            !String::from_utf8_lossy(&body).contains(&from.marker),
            "the refusal carried the other site's data"
        );
    }
}

/// The post-run invariant query on the loaded site: every `time_entry` id and series number
/// is unique, there are at least as many entries as the report counted ok posts, and the
/// 1 000-row page of the list answers. Returns the number of entries read.
async fn series_invariants(site: &Site, posted_ok: u64) -> u64 {
    let mut conn = Conn::new();
    let target = &site.launched.target;
    let mut ids = BTreeSet::new();
    let mut numbers = BTreeSet::new();
    let mut seen_numbers = 0u64;
    let mut cursor: Option<String> = None;
    let mut rows_read = 0u64;
    while rows_read < INVARIANT_MAX_ROWS {
        let mut body = json!({ "limit": INVARIANT_PAGE });
        if let Some(after) = &cursor {
            body["cursor"] = Json::String(after.clone());
        }
        let (status, raw) = call(
            &mut conn,
            target,
            &site.bearer,
            "POST",
            "/api/v1/resource/time_entry/query",
            Some(&body),
        )
        .await;
        assert_eq!(status, 200, "the time_entry list answered {status}");
        let page: Json = serde_json::from_slice(&raw).expect("the list answer is JSON");
        let rows = page
            .get("data")
            .and_then(Json::as_array)
            .cloned()
            .unwrap_or_default();
        for row in &rows {
            rows_read += 1;
            if let Some(id) = row.get("id").and_then(Json::as_str) {
                assert!(ids.insert(id.to_owned()), "id {id} was issued twice");
            }
            if let Some(number) = row.get("entry_no").and_then(Json::as_str)
                && !number.is_empty()
            {
                seen_numbers += 1;
                assert!(
                    numbers.insert(number.to_owned()),
                    "number series value {number} was issued twice"
                );
            }
        }
        cursor = page
            .get("next_cursor")
            .and_then(Json::as_str)
            .map(str::to_owned);
        if cursor.is_none() || rows.is_empty() {
            break;
        }
    }
    if rows_read < INVARIANT_MAX_ROWS {
        assert!(
            rows_read >= posted_ok,
            "{posted_ok} posts were acknowledged but only {rows_read} entries exist"
        );
    }
    println!(
        "l9 tier_m invariants: {rows_read} time_entry rows, {seen_numbers} with a series number, \
         no duplicate id or number"
    );
    // The 1 000-row page the brief names for lists and reports.
    let (status, _) = call(
        &mut conn,
        target,
        &site.bearer,
        "POST",
        "/api/v1/resource/task/query",
        Some(&json!({ "limit": 1000 })),
    )
    .await;
    assert_eq!(status, 200, "the 1000-row task list answered {status}");
    rows_read
}

fn count_of(report: &Report, name: &str) -> (u64, u64) {
    report
        .operations
        .iter()
        .find(|o| o.name == name)
        .map_or((0, 0), |o| (o.count, o.ok))
}

fn assert_no_server_failures(report: &Report) {
    for operation in &report.operations {
        assert_eq!(
            operation.server_errors,
            0,
            "{} answered a 5xx under load (a deadlock or a duplicate number surfaces as one):\n{}",
            operation.name,
            report.to_markdown()
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn tier_m_under_load_with_a_second_site_loaded_beside_it() {
    println!("l9 tier_m: {}", runner_header());
    let permille = scale_permille();
    let mut profile = load_profile();
    let sessions = profile.sessions;
    scale_durations(&mut profile, permille);

    let Some(a) = Site::start(sessions).await else {
        println!("SKIPPED: the l9-scale tier_m run needs SPOT_TEST_PG_URL");
        return;
    };
    let b = Site::start(20)
        .await
        .expect("the first site started, so PostgreSQL is configured");
    tokens_do_not_cross(&a, &b).await;

    // The second site carries its own, lighter load at the same time on the same host.
    let mut side = profile.clone();
    side.name = "tier_m_side".to_owned();
    side.sessions = 20;
    side.target_tps = l9_scale::profile::Decimal::Int(20);
    side.seed = profile.seed.wrapping_add(1);
    let side_budgets = Budgets::from_toml(
        "[[budget]]\nprofile = \"tier_m_side\"\noperation = \"all\"\nmax_error_rate = 1\n",
    )
    .unwrap();

    let run_a = RunOptions {
        profile: profile.clone(),
        budgets: load_budgets(),
        mode: Mode::Attach {
            target: a.launched.target.clone(),
            users: a.launched.users.clone(),
        },
        run_id: launcher::random_hex(12),
        out_dir: None,
    };
    let run_b = RunOptions {
        profile: side,
        budgets: side_budgets,
        mode: Mode::Attach {
            target: b.launched.target.clone(),
            users: b.launched.users.clone(),
        },
        run_id: launcher::random_hex(12),
        out_dir: None,
    };

    let stop = Arc::new(AtomicBool::new(false));
    let loads = {
        let stop = stop.clone();
        async move {
            let (ra, rb) = tokio::join!(execute(run_a), execute(run_b));
            stop.store(true, Ordering::SeqCst);
            (ra, rb)
        }
    };
    let ((ra, rb), probes) = tokio::join!(loads, probe_isolation(&a, &b, stop.clone()));
    let report = ra
        .unwrap_or_else(|e| panic!("the tier_m run failed to start: {e}"))
        .expect("Mode::Attach never returns None")
        .report;
    let side_report = rb
        .unwrap_or_else(|e| panic!("the side run failed to start: {e}"))
        .expect("Mode::Attach never returns None")
        .report;
    println!("{}", report.to_markdown());
    println!("l9 tier_m: {probes} isolation probes during the load, no leak");

    // What holds on every machine.
    assert!(
        !report.budgets.is_empty(),
        "the budget evaluation produced no verdict"
    );
    for operation in [Op::OpenForm, Op::CreateSubmit] {
        let (count, _) = count_of(&report, operation.name());
        assert!(
            count > 0,
            "{operation} never ran:\n{}",
            report.to_markdown()
        );
    }
    assert_no_server_failures(&report);
    assert_no_server_failures(&side_report);
    let (_, posted_ok) = count_of(&report, Op::CreateSubmit.name());
    series_invariants(&a, posted_ok).await;
    tokens_do_not_cross(&a, &b).await;

    // What holds only on the reference runner: the README budgets.
    if enforce_budgets() {
        assert!(report.passed, "budgets breached:\n{}", report.to_markdown());
        assert_eq!(
            report.shed_arrivals, 0,
            "the arrival rate outran the 200 sessions"
        );
    } else {
        for verdict in report.budgets.iter().filter(|v| !v.passed) {
            println!(
                "l9 tier_m (not enforced, runner is not `{REFERENCE_RUNNER}`): {} {} limit {} measured {}",
                verdict.operation, verdict.metric, verdict.limit, verdict.measured
            );
        }
    }

    a.launched.shutdown().await;
    b.launched.shutdown().await;
}

#[test]
fn the_tier_m_profile_is_200_sessions_and_10_tx_per_second_with_a_read_heavy_mix() {
    let profile = load_profile();
    assert_eq!(profile.name, "tier_m");
    assert_eq!(profile.tier, "M");
    assert_eq!(profile.sessions, 200);
    assert_eq!(profile.ramp_seconds, 120);
    assert_eq!(profile.steady_seconds, 1800);
    // Posting tx/s = arrival rate x share of create_submit.
    let total = profile.mix.total();
    let posting_milli =
        profile.target_milli_tps() * u64::from(profile.mix.create_submit) / total.max(1);
    assert_eq!(posting_milli, 10_000, "the posting rate is 10 tx/s");
    // About 20 reads for each write: between 15:1 and 25:1, and no other write operation.
    assert_eq!(profile.mix.update, 0);
    assert_eq!(profile.mix.stale_update, 0);
    let writes = u64::from(profile.mix.create_submit);
    let reads = total - writes;
    assert!(
        (15 * writes..=25 * writes).contains(&reads),
        "{reads} reads to {writes} writes"
    );
    assert!(profile.mix.list > 0 && profile.mix.report_run > 0 && profile.mix.open_form > 0);
}

#[test]
fn the_tier_m_budgets_are_the_readme_targets() {
    let budgets = load_budgets();
    let line = |op: &str| {
        budgets
            .for_profile("tier_m")
            .into_iter()
            .find(|b| b.operation == op)
            .unwrap_or_else(|| panic!("no tier_m budget for {op}"))
            .clone()
    };
    assert_eq!(line("open_form").p95_ms, Some(300));
    let post = line("create_submit");
    assert_eq!(post.p95_ms, Some(1000));
    assert_eq!(post.p99_ms, Some(2500));
    assert_eq!(post.min_throughput_milli, Some(9_500));
    // 0.1 percent.
    assert_eq!(line("all").max_error_ppm, Some(1_000));
}

#[test]
fn durations_scale_in_integers_and_never_to_zero() {
    let mut profile = load_profile();
    scale_durations(&mut profile, 20);
    assert_eq!(profile.steady_seconds, 36);
    assert_eq!(profile.ramp_seconds, 2);
    assert!(profile.sample_every_seconds >= 1);
    let mut tiny = load_profile();
    scale_durations(&mut tiny, 1);
    assert_eq!(tiny.steady_seconds, 1);
    assert_eq!(tiny.ramp_seconds, 1);
    let mut full = load_profile();
    scale_durations(&mut full, 1000);
    assert_eq!(full.steady_seconds, 1800);
    assert_eq!(full.ramp_seconds, 120);
    assert_eq!(parse_decimal("0.02", 3), Some(20));
}
