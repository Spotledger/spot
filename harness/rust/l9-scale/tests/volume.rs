//! The `volume` profile's PR-run (100,000-row) smoke test (WP-TST-15.T4): a real `spot serve`
//! on a real PostgreSQL server, seeded with 100,000 `task` parents and their `checklist` child
//! rows through the API (`seed.rs`), then measured at that size: an indexed list (first page
//! and the deepest page the dataset offers), a filtered count, a report run, a lookup
//! typeahead, and finally the `volume` profile's posting leg (10 tx/s for 5 minutes).
//!
//! Needs `SPOT_TEST_PG_URL`; without it this prints why and returns, never faking a result
//! (WP-TST-15.T4 "Tests" section). 10M/50M-row runs are the nightly tier and are not executed
//! here (`profiles/volume.toml`'s comments and the L9 baseline notes carry the
//! sizing for them; this environment has no large self-hosted runner to run them on).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

#[path = "../src/seed.rs"]
mod seed;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use l9_scale::budget::Budgets;
use l9_scale::clock::{Clock, MonotonicClock};
use l9_scale::http::Conn;
use l9_scale::launcher::{self, LaunchOptions};
use l9_scale::profile::Profile;
use l9_scale::{Mode, RunOptions, execute};
use serde_json::{Value as Json, json};

fn crate_dir() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

/// How many `task` parents the PR-run variant seeds. The brief's tier-M target is 10 to 50
/// million journal-equivalent rows; 100,000 is the size the brief fixes for a run inside this
/// test (small enough for CI/dev, big enough that a page at depth and a filtered count read
/// through an index rather than a nearly-empty table).
const PR_RUN_PARENTS: u64 = 100_000;

/// Opt-in scale override for the nightly/tier-M runs: `L9_VOLUME_PARENTS=10000000` (or up to
/// `MAX_PARENTS`). Unset, empty or unparsable means the 100,000-row PR-run size, so CI and a
/// plain `cargo test -p l9-scale` can never reach the multi-hour sizes by accident. The loader
/// is resumable and checkpointed under the temp directory; the run removes the checkpoint and
/// the scratch deployment when it finishes.
const SCALE_ENV: &str = "L9_VOLUME_PARENTS";
const MAX_PARENTS: u64 = 50_000_000;
/// From this many parents up the run is a tier-M run and is held to the brief's own numbers
/// (`budgets/volume.toml`, profile `volume-full`), not the PR-run floor.
const FULL_SCALE_FROM: u64 = 10_000_000;

fn parents_to_seed() -> u64 {
    std::env::var(SCALE_ENV)
        .ok()
        .and_then(|v| v.trim().replace('_', "").parse::<u64>().ok())
        .map_or(PR_RUN_PARENTS, |n| n.clamp(1_000, MAX_PARENTS))
}

/// The nightly/tier-M depth the brief names ("page 10000"). At `PR_RUN_PARENTS` rows and 25
/// rows per page there are only a few thousand pages, so this run walks to the deepest page
/// the seeded dataset actually has, printing both numbers rather than silently using a smaller
/// one.
const NIGHTLY_DEPTH_PAGES: u64 = 10_000;

const PAGE_SIZE: u64 = 25;

async fn sign_in(
    conn: &mut Conn,
    target: &l9_scale::http::Target,
    email: &str,
    password: &str,
) -> String {
    let body = json!({ "user": email, "password": password }).to_string();
    let response = conn
        .request(
            target,
            "POST",
            "/api/v1/auth/login",
            &[],
            Some(body.as_bytes()),
            Duration::from_secs(30),
        )
        .await
        .unwrap_or_else(|e| panic!("sign-in failed: {e:?}"));
    assert_eq!(response.status, 200, "sign-in answered {}", response.status);
    response
        .json()
        .pointer("/data/token")
        .and_then(Json::as_str)
        .expect("a sign-in answer always carries a token")
        .to_owned()
}

async fn post(
    conn: &mut Conn,
    target: &l9_scale::http::Target,
    bearer: &str,
    path: &str,
    body: &Json,
) -> (u16, Json) {
    let bytes = body.to_string().into_bytes();
    let response = conn
        .request(
            target,
            "POST",
            path,
            &[
                ("Authorization", bearer),
                ("Content-Type", "application/json"),
            ],
            Some(&bytes),
            Duration::from_secs(30),
        )
        .await
        .unwrap_or_else(|e| panic!("POST {path} failed: {e:?}"));
    (response.status, response.json())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn the_volume_profile_seeds_100k_rows_and_meets_its_operation_budgets() {
    let clock: Arc<dyn Clock> = Arc::new(MonotonicClock::new());
    let parents = parents_to_seed();
    let run_id = launcher::random_hex(12);
    let repo = launcher::default_repo();
    let spot_bin = launcher::locate_spot(&repo, None)
        .await
        .unwrap_or_else(|e| panic!("cannot locate the `spot` binary: {e}"));

    // The same launcher every other l9-scale test uses: a real `spot serve` on a scratch
    // PostgreSQL database and site, torn down at the end. `None` from `start` means no
    // SPOT_TEST_PG_URL is configured; the launcher has already printed why, so this test
    // just stops rather than faking a run.
    let Some(launched) = launcher::Launched::start(&LaunchOptions {
        repo: repo.clone(),
        spot_bin: Some(spot_bin.clone()),
        users: 20,
        run_id: run_id.clone(),
        keep: false,
    })
    .await
    .unwrap_or_else(|e| panic!("cannot start the scratch deployment: {e}")) else {
        println!("SKIPPED: the l9-scale volume run needs SPOT_TEST_PG_URL");
        return;
    };

    // The scratch deployment's root directory, so `spot check` can be run against the same
    // site after seeding. The launcher does not expose this path (it is not an owned file of
    // this task), so it is rebuilt from the same `run_id` the launcher used to build it; see
    // the L9 baseline notes for why and the risk that carries.
    let root = std::env::temp_dir().join(format!("l9_scale_{run_id}"));

    let user = launched
        .users
        .first()
        .expect("the launcher made at least one user")
        .clone();
    let mut conn = Conn::new();
    let token = sign_in(
        &mut conn,
        &launched.target,
        &user.email,
        user.password.expose(),
    )
    .await;
    let bearer = format!("Bearer {token}");

    // `me` for a user id `project.owner` accepts (the field links the platform's `user`
    // DocType by id, not by email).
    let me = conn
        .request(
            &launched.target,
            "GET",
            "/api/v1/auth/session",
            &[("Authorization", &bearer)],
            None,
            Duration::from_secs(30),
        )
        .await
        .unwrap_or_else(|e| panic!("GET /api/v1/auth/session failed: {e:?}"));
    assert_eq!(me.status, 200, "the session lookup answered {}", me.status);
    let owner_id = me
        .json()
        .pointer("/data/user/id")
        .and_then(Json::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| panic!("the session answer names no user id: {}", me.json()));

    let projects = seed::ensure_projects(
        &launched.target,
        &bearer,
        Duration::from_secs(30),
        &owner_id,
        5,
        20261107,
    )
    .await
    .unwrap_or_else(|e| panic!("cannot create the volume projects: {e}"));

    let checkpoint = std::env::temp_dir().join(format!("l9_volume_checkpoint_{run_id}.json"));
    let _ = std::fs::remove_file(&checkpoint);
    let mut options = seed::SeedOptions {
        target: launched.target.clone(),
        bearer: bearer.clone(),
        parents,
        children_min: 0,
        children_max: 3,
        seed: 20261107,
        workers: 64,
        request_timeout: Duration::from_secs(10),
        checkpoint: checkpoint.clone(),
        // A deadline close enough to "now" that the first call almost certainly cannot finish
        // all 100,000 rows: this exercises resumability for real, not just in its own unit
        // test. The deadline is computed off this test's own clock, never a direct wall-clock
        // read (the hard rule: time is an argument).
        deadline_ns: Some(clock.now_ns() + 50_000_000),
    };
    let first = seed::seed(&options, &projects, clock.clone())
        .await
        .unwrap_or_else(|e| panic!("seeding (first call) failed: {e}"));
    println!(
        "l9-scale volume: first seed call created {} of {parents} rows before its time \
         box (timed_out={})",
        first.created, first.timed_out
    );

    options.deadline_ns = None;
    let second = seed::seed(&options, &projects, clock.clone())
        .await
        .unwrap_or_else(|e| panic!("seeding (resuming call) failed: {e}"));
    assert_eq!(
        second.total_done, parents,
        "the resumed run must finish every row the first call left undone"
    );
    assert_eq!(
        first.created + second.created + first.failed + second.failed,
        parents,
        "every row is accounted for exactly once across both calls"
    );
    println!(
        "l9-scale volume: seeded {} task parents ({} child rows) in two calls (resumed)",
        second.total_done,
        first.child_rows + second.child_rows
    );

    // Deliverable 1's verification pass: the closest existing storage-readiness check. There
    // is no `spot check --deep` in this codebase (see the L9 baseline notes).
    let checked = seed::run_check(&spot_bin, &root)
        .await
        .unwrap_or_else(|e| panic!("`spot check` failed after seeding: {e}"));
    assert!(checked.starts_with("ok:"), "{checked}");

    // --- Operations at volume ----------------------------------------------------------------

    // Indexed list, first page: filtered on `status` (in `in_standard_filter`) and sorted by
    // `due_date`, same shape as `session.rs`'s `list` but with an explicit sort.
    let list_body = json!({
        "filter": { "field": "status", "op": "eq", "value": "Open" },
        "order": [{ "field": "due_date", "dir": "asc" }],
        "limit": PAGE_SIZE,
    });
    let query_path = format!("/api/v1/resource/{}/query", seed::DOCTYPE);
    // One untimed warm-up call: the measured budget is for a server already serving traffic
    // (the posting leg has been running sessions for a ramp and a steady phase by the time a
    // real deployment's list budget matters), not for a cold connection, a cold plan cache and
    // a cold TCP handshake, which the very first request after seeding pays for once.
    let _ = post(
        &mut conn,
        &launched.target,
        &bearer,
        &query_path,
        &list_body,
    )
    .await;
    let before_first = clock.now_ns();
    let (status, first_page) = post(
        &mut conn,
        &launched.target,
        &bearer,
        &query_path,
        &list_body,
    )
    .await;
    let first_page_ns = clock.now_ns() - before_first;
    assert_eq!(status, 200, "the first list page answered {status}");
    println!(
        "l9-scale volume: list first page in {} ms",
        first_page_ns / 1_000_000
    );

    // Walk the cursor to the deepest page the dataset offers, up to the nightly target.
    let mut cursor = first_page
        .pointer("/next_cursor")
        .and_then(Json::as_str)
        .map(str::to_owned);
    let mut pages_walked: u64 = 1;
    let mut deep_ns = first_page_ns;
    let target_pages = NIGHTLY_DEPTH_PAGES.min(parents / PAGE_SIZE);
    while let Some(after) = cursor.clone() {
        if pages_walked >= target_pages {
            break;
        }
        let mut body = list_body.clone();
        body["cursor"] = Json::String(after);
        let before = clock.now_ns();
        let (status, page) = post(&mut conn, &launched.target, &bearer, &query_path, &body).await;
        deep_ns = clock.now_ns() - before;
        assert_eq!(status, 200, "a deep list page answered {status}");
        pages_walked += 1;
        cursor = page
            .pointer("/next_cursor")
            .and_then(Json::as_str)
            .map(str::to_owned);
    }
    println!(
        "l9-scale volume: walked to page {pages_walked} (nightly target {NIGHTLY_DEPTH_PAGES}, \
         reachable at this run size: {target_pages}) in {} ms for the last page",
        deep_ns / 1_000_000
    );
    // The literal "list first page p95 300 ms" budget is enforced by `budgets/volume.toml`'s
    // `list` row against the posting leg below, which runs the same `list` operation many
    // times over a steady window (a real p95 needs many samples; this one HTTP call above is
    // one data point, fit only for the plan-regression proxy right below, not for a percentile
    // budget).
    assert!(
        deep_ns <= first_page_ns.saturating_mul(10).max(300_000_000),
        "the page at depth {pages_walked} took {} ms against a first page of {} ms: this looks \
         like the plan regressed off an index",
        deep_ns / 1_000_000,
        first_page_ns / 1_000_000
    );

    // Count with filter: the same query, asking for the total instead of (or alongside) rows.
    let count_body = json!({
        "filter": { "field": "status", "op": "eq", "value": "Open" },
        "limit": 1,
        "with_total": true,
    });
    let (status, counted) = post(
        &mut conn,
        &launched.target,
        &bearer,
        &query_path,
        &count_body,
    )
    .await;
    assert_eq!(status, 200, "the filtered count answered {status}");
    let total = counted
        .pointer("/total")
        .and_then(Json::as_u64)
        .expect("with_total=true always answers a total");
    assert!(total > 0 && total <= parents, "total {total}");
    println!("l9-scale volume: {total} open tasks of {parents} seeded");

    // Report with group-by over a month: the sample app ships no report definition yet (the
    // profile's default, `task_summary`, answers 404 `report.not_found`; `session.rs`'s own
    // `report_run` already treats that as the expected answer). This call and its budget are
    // the same shape every l9-scale run already measures; a real group-by report is an Open
    // Question in the PR, not something this task can add (reports/ is not an owned path).
    let (status, _) = post(
        &mut conn,
        &launched.target,
        &bearer,
        "/api/v1/report/task_summary/run",
        &json!({ "limit": 50 }),
    )
    .await;
    assert!(
        status == 200 || status == 404,
        "the report run answered an unexpected {status}"
    );

    // Lookup/link search with typeahead: `task.assignee` declares a lookup (ADR-068 §3) with
    // an `enabled_only` parameter (task.toml). This is the one operation of this list that is
    // not exercised anywhere else in `l9-scale`.
    let (status, lookup) = post(
        &mut conn,
        &launched.target,
        &bearer,
        "/api/v1/lookup/task/assignee",
        &json!({ "text": "l9-user-0" }),
    )
    .await;
    // The lookup endpoint itself is wired (ADR-068 §3.5); whether this particular deployment
    // has a lookup provider configured for `platform:link_search` is a separate concern this
    // task does not own (no `[fields.lookup]` override is set on `assignee`'s neighbour fields
    // either, so every link field here depends on the platform default). A server with no
    // provider configured answers `api.invalid_request` / "No lookup provider is configured."
    // — accepted here as today's documented answer (see the L9 baseline notes),
    // the same precedent as `report_run`'s accepted 404 just above.
    let no_provider = lookup
        .pointer("/error/message")
        .and_then(Json::as_str)
        .is_some_and(|m| m.contains("lookup provider"));
    assert!(
        status == 200 || (status == 400 && no_provider),
        "the assignee lookup answered an unexpected {status}: {lookup}"
    );
    if status == 200 {
        assert!(
            lookup.pointer("/data/rows").is_some(),
            "the lookup answered no page: {lookup}"
        );
    }

    drop(conn);

    // --- Posting at 10 tx/s for 5 minutes (the `volume` profile) ------------------------------
    let mut profile = Profile::load(&crate_dir().join("profiles/volume.toml")).unwrap();
    if parents >= FULL_SCALE_FROM {
        // Tier-M volume: the brief's own p95 numbers apply, not the PR-run floor.
        profile.name = "volume-full".to_owned();
    }
    let budgets = Budgets::load(&crate_dir().join("budgets/volume.toml")).unwrap();
    let out = std::env::temp_dir().join(format!("l9_volume_report_{run_id}"));
    let posting_run_id = launcher::random_hex(12);
    let posting = execute(RunOptions {
        profile,
        budgets,
        mode: Mode::Attach {
            target: launched.target.clone(),
            users: launched.users.clone(),
        },
        run_id: posting_run_id,
        out_dir: Some(out.clone()),
    })
    .await
    .unwrap_or_else(|e| panic!("the volume posting run failed to start: {e}"));
    let finished = posting.expect("Mode::Attach never returns None (only Mode::Launch does)");
    let report = finished.report;
    println!("{}", report.to_markdown());
    assert!(
        report.passed,
        "the volume profile's budgets were breached:\n{}",
        report.to_markdown()
    );
    let _ = std::fs::remove_dir_all(&out);
    let _ = std::fs::remove_file(&checkpoint);

    launched.shutdown().await;
}

#[test]
fn the_volume_budgets_parse_and_the_tier_m_profile_keeps_the_briefs_numbers() {
    let budgets = Budgets::load(&crate_dir().join("budgets/volume.toml")).unwrap();
    let limit = |profile: &str, op: &str| {
        budgets
            .for_profile(profile)
            .into_iter()
            .find(|b| b.operation == op)
            .and_then(|b| b.p95_ms)
    };
    assert_eq!(limit("volume-full", "list"), Some(300));
    assert_eq!(limit("volume-full", "create_submit"), Some(1000));
    assert!(limit("volume", "list").is_some() && limit("volume", "create_submit").is_some());
    // The opt-in override never leaves the allowed range.
    assert!((1_000..=MAX_PARENTS).contains(&parents_to_seed()));
}
