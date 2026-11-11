//! The harness proves itself at N=200 in the normal CI: the real server is started, 200 idle
//! connections of every kind are opened and held beside busy clients, the report is built, and the
//! run is shut down. Memory and latency are measured and reported but not judged here (200
//! connections and a one-second hold are too small and too noisy for a per-connection budget); the
//! 10,000 run in `idle_10k.rs` judges them.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use spot_idle_connections::{Budgets, Outcome, RunConfig, run};

fn small() -> RunConfig {
    RunConfig {
        connections: 200,
        hold: Duration::from_secs(2),
        busy_clients: 10,
        busy_rps: 50,
        shutdown_grace: Duration::from_secs(5),
        ..RunConfig::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_hundred_idle_connections_are_opened_held_reported_and_closed() {
    let config = small();
    let outcome = run(&config, &Budgets::shipped().unwrap()).await.unwrap();
    let report = match outcome {
        Outcome::Completed(report) => report,
        Outcome::Skipped(reason) => {
            println!("SKIPPED: {reason}");
            return;
        }
    };
    println!("{}", report.to_json());

    // The connection counts: every requested connection of every kind, and the server counted them.
    assert_eq!(
        (
            report.setup.http1,
            report.setup.http2,
            report.setup.websocket
        ),
        (100, 50, 50)
    );
    assert_eq!(report.accept.established, 200);
    assert_eq!(
        report.accept.connect_errors, 0,
        "{:?}",
        report.accept.first_connect_error
    );
    assert!(
        report.accept.server_open_connections >= 150,
        "HTTP/1.1 and HTTP/2 are counted by spot-http"
    );
    assert!(
        report.accept.server_ws_sessions >= 50,
        "WebSockets are counted by the bus"
    );
    assert_eq!(report.accept.accept_errors, 0);
    assert!(
        report.accept.accept_errors_counted,
        "the accept-error counter must be live"
    );

    // The busy clients ran in both phases without a failure and the percentiles are ordered.
    assert_eq!(report.latency.busy_errors, 0);
    for p in [report.latency.empty, report.latency.loaded] {
        assert!(p.count >= 50, "{p:?}");
        assert!(p.p50_us <= p.p95_us && p.p95_us <= p.p99_us && p.p99_us <= p.max_us);
    }

    // Shutdown closed everything and nothing leaked.
    let judged: Vec<_> = report
        .checks
        .iter()
        .filter(|c| !matches!(c.name, "memory" | "latency"))
        .filter(|c| !c.ok)
        .collect();
    assert!(judged.is_empty(), "{judged:?}");
    assert_eq!(report.shutdown.clients_still_open, 0);
    assert!(report.shutdown.tasks_after <= report.shutdown.tasks_before);

    // The measurements exist and the report is machine-readable.
    assert!(report.checks.iter().any(|c| c.name == "memory"));
    assert!(report.checks.iter().any(|c| c.name == "latency"));
    if cfg!(target_os = "linux") {
        assert!(report.memory.baseline_bytes.is_some() && report.memory.held_bytes.is_some());
        assert!(report.descriptors.held.unwrap() > report.descriptors.baseline.unwrap());
    }
    let json: serde_json::Value = serde_json::from_str(&report.to_json()).unwrap();
    for key in [
        "setup",
        "memory",
        "descriptors",
        "accept",
        "latency",
        "shutdown",
        "checks",
        "passed",
    ] {
        assert!(json.get(key).is_some(), "the report has no `{key}`");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_descriptor_limit_below_the_need_skips_with_a_clear_message() {
    // Needs more descriptors than any runner grants; the harness must say so and open nothing.
    let config = RunConfig {
        connections: 50_000_000,
        ..small()
    };
    match run(&config, &Budgets::shipped().unwrap()).await.unwrap() {
        Outcome::Skipped(reason) => {
            assert!(
                reason.contains("RLIMIT_NOFILE") && reason.contains("ulimit -n"),
                "{reason}"
            );
        }
        Outcome::Completed(_) => {
            // No descriptor limit applies on this system (for example Windows): nothing to skip.
            if cfg!(unix) {
                panic!("a unix system with a descriptor limit must skip");
            }
        }
    }
}

#[tokio::test]
async fn invalid_settings_are_refused_before_anything_starts() {
    let budgets = Budgets::shipped().unwrap();
    for bad in [
        RunConfig {
            connections: 0,
            ..small()
        },
        RunConfig {
            busy_rps: 0,
            ..small()
        },
        RunConfig {
            hold: Duration::ZERO,
            ..small()
        },
    ] {
        assert!(run(&bad, &budgets).await.is_err());
    }
}
