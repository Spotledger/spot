//! The short mode on a real PostgreSQL (`SPOT_TEST_PG_URL`), and the ignored full run.
//!
//! The short run proves the harness: three phases, the report, the budget comparison, the cap on
//! the `ai` backends, cancellation and the clean-up of the role and the schema. Latency budgets
//! are not asserted here (a shared CI machine is too noisy for 5-second phases); the full run
//! asserts them.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use spot_ai_load::budget::HumanBudget;
use spot_ai_load::{Budgets, RunConfig, evaluate, run};

/// Latency budgets wide enough never to flake on a 5-second run; every other budget is the
/// default.
fn lenient_latency() -> Budgets {
    let mut budgets = Budgets::defaults().expect("the default budgets parse");
    budgets.human = HumanBudget {
        p95_ratio_permille: 100_000,
        p95_slack_ms: 60_000,
        p99_ratio_permille: 100_000,
        p99_slack_ms: 60_000,
        ..budgets.human
    };
    budgets
}

async fn role_exists_on_server(name: &str) -> bool {
    let url = std::env::var("SPOT_TEST_PG_URL").expect("the run was not skipped");
    let admin = spot_postgres::PgConfig::from_url(&url).expect("a usable URL");
    let client = spot_ai_load::fixture::connect(&admin)
        .await
        .expect("connect");
    client
        .query_one("SELECT count(*) FROM pg_roles WHERE rolname = $1", &[&name])
        .await
        .expect("query")
        .get::<_, i64>(0)
        > 0
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_short_run_measures_every_phase_and_cleans_up() {
    let config = RunConfig::short();
    let budgets = lenient_latency();
    let Some(report) = run(&config, &budgets).await.expect("the run completes") else {
        println!("SKIPPED: the short harness run needs SPOT_TEST_PG_URL");
        return;
    };
    println!("{}", report.to_json());

    let names: Vec<&str> = report.phases.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["baseline", "with_agents", "overload"]);
    for phase in &report.phases {
        assert!(phase.human.count > 0, "{}: no human operations", phase.name);
        assert!(phase.human.p95_us >= phase.human.p50_us);
        assert!(phase.human.p99_us >= phase.human.p95_us);
        assert!(
            phase.human_by_op.contains_key("read") && phase.human_by_op.contains_key("list"),
            "{}: {:?}",
            phase.name,
            phase.human_by_op.keys().collect::<Vec<_>>()
        );
        assert_eq!(phase.human_errors, 0, "{}: human errors", phase.name);
        assert!(!phase.aimd_limit.is_empty(), "{}: no samples", phase.name);
    }

    let baseline = report.phase("baseline").expect("baseline");
    assert_eq!(baseline.agent_calls.admitted, 0);
    assert_eq!(baseline.ai_active_backends_max, 0);

    let with_agents = report.phase("with_agents").expect("with_agents");
    assert_eq!(with_agents.agents, 5);
    assert!(
        with_agents.agent_calls.admitted > 0,
        "{:?}",
        with_agents.agent_calls
    );
    assert!(with_agents.agent_calls.completed > 0);
    assert!(
        with_agents.agent_calls.timed_out > 0,
        "an overdue call must time out and be cancelled: {:?}",
        with_agents.agent_calls
    );

    let overload = report.phase("overload").expect("overload");
    assert_eq!(overload.agents, 12);
    assert!(overload.agent_calls.admitted > with_agents.agent_calls.completed / 4);
    assert!(overload.agent_calls.timed_out > 0);
    assert!(
        overload.agent_calls.queued + overload.agent_calls.shed + overload.agent_calls.circuit_open
            > 0,
        "12 agents on a pool of 4 must be queued, shed or stopped by a breaker: {:?}",
        overload.agent_calls
    );
    assert!(
        overload.breakers.iter().any(|b| b.open + b.half_open > 0),
        "the runaway agents must trip their breakers: {:?}",
        overload.breakers
    );

    // The cap, as PostgreSQL sees it.
    for phase in &report.phases {
        assert!(
            phase.ai_active_backends_max <= config.ai_pool.pool_size,
            "{}: {} backends running for the ai role",
            phase.name,
            phase.ai_active_backends_max
        );
        assert!(
            phase
                .ai_open_backends
                .iter()
                .all(|n| *n <= config.ai_pool.pool_size),
            "{}: {:?}",
            phase.name,
            phase.ai_open_backends
        );
    }

    // Nothing leaks, cancellation worked, the role is read-only, the run cleaned up.
    assert_eq!(report.leaks.permits, 0);
    assert_eq!(report.leaks.connections, 0);
    assert_eq!(report.leaks.running_after_cancel, 0);
    assert!(!report.leaks.ai_write_allowed);
    assert!(!report.leaks.left_behind);
    assert!(
        report.recovery.recovered_after_ms.is_some(),
        "{:?}",
        report.recovery
    );
    assert_eq!(report.recovery.configured_limit, config.ai_pool.pool_size);
    assert!(report.breaches.is_empty(), "{:?}", report.breaches);
    assert!(report.passed);
    // The verdict is a pure function of the report: running it again gives the same answer.
    assert_eq!(evaluate(&report, &budgets), report.breaches);
    // The report is JSON a dashboard can read.
    let json: serde_json::Value = serde_json::from_str(&report.to_json()).expect("valid JSON");
    assert_eq!(json["mode"], "short");
    assert_eq!(json["passed"], true);
    assert_eq!(report.rows, 10_000);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_impossible_budget_is_reported_as_a_breach_not_an_error() {
    let mut config = RunConfig::short();
    config.phase_duration = Duration::from_secs(2);
    config.settle = Duration::from_secs(2);
    let mut budgets = lenient_latency();
    // Not even the idle server can be this fast: the run must report it, and still clean up.
    budgets.human = HumanBudget {
        p95_ratio_permille: 1,
        p95_slack_ms: 0,
        p99_ratio_permille: 1,
        p99_slack_ms: 0,
        max_errors: 0,
    };
    let Some(report) = run(&config, &budgets).await.expect("the run completes") else {
        println!("SKIPPED: the short harness run needs SPOT_TEST_PG_URL");
        return;
    };
    assert!(!report.passed);
    assert!(
        report
            .breaches
            .iter()
            .any(|b| b.check.contains("human p95")),
        "{:?}",
        report.breaches
    );
    assert!(!report.leaks.left_behind);
    assert!(!role_exists_on_server("spot_ail_nonexistent").await);
}

/// The full run: 60-second phases, 50 human users, 20 then 60 agents, 200,000 rows. Run it with
/// `cargo test -p spot-ai-load --release -- --ignored --nocapture`; the nightly job runs it.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "a five-minute run against a real PostgreSQL; the nightly job runs it"]
async fn the_full_run_keeps_humans_within_budget() {
    let budgets = Budgets::defaults().expect("the default budgets parse");
    let mut config = RunConfig::full();
    if let Ok(list) = std::env::var("SPOT_AI_LOAD_ROLE_SETTINGS") {
        config = config.with_role_settings(&list);
    }
    let Some(report) = run(&config, &budgets).await.expect("the run completes") else {
        println!("SKIPPED: the full harness run needs SPOT_TEST_PG_URL");
        return;
    };
    println!("{}", report.to_json());
    assert!(report.passed, "budget breaches: {:?}", report.breaches);
}
