//! The caps on concurrent report runs, the settings that set them and the metrics a run records
//! (WP-PLT-14.T6).
//!
//! The gate is the site's real [`ReportGate`], the object `Shared::run` runs every report through
//! (the shipped path and the report-source path alike). The bodies are gated futures that block
//! until released, so a cap is hit deterministically. The statement-timeout and the
//! `/metrics`-over-HTTP cases need PostgreSQL and run under `tests/report.rs`'s harness.
//!
//! The metrics recorder is process-global: the one test that reads it takes a lock and compares
//! counters before and after.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use spot_cli::services::report::{
    BUSY_RETRY_AFTER_SECS, DEFAULT_MAX_CONCURRENT, DEFAULT_MAX_CONCURRENT_PER_USER, ReportGate,
    RunResult, Settings,
};
use spot_http::api::registry::Code;
use spot_http::api::service::ApiError;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;

type Out = Result<Vec<u8>, ApiError>;

/// The gate records process-global metrics: the tests run one at a time so the metrics test can
/// compare counters.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[allow(clippy::ptr_arg)]
fn rows(v: &Vec<u8>) -> usize {
    v.len()
}

/// A run that blocks until `release` has a permit, then answers `n` rows.
fn blocked(
    gate: &Arc<ReportGate>,
    user: &'static str,
    release: &Arc<Semaphore>,
    n: usize,
) -> JoinHandle<Out> {
    let (gate, release) = (Arc::clone(gate), Arc::clone(release));
    tokio::spawn(async move {
        gate.run(user, rows, async {
            let permit = release.acquire().await.unwrap();
            permit.forget();
            Ok(vec![0u8; n])
        })
        .await
    })
}

async fn in_flight(gate: &ReportGate, n: usize) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while gate.in_flight() != n {
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("in flight never reached {n}, is {}", gate.in_flight()));
}

/// A run that must never start: it records that its body was polled.
async fn refused(gate: &ReportGate, user: &str) -> (ApiError, bool) {
    let polled = AtomicBool::new(false);
    let error = gate
        .run(user, rows, async {
            polled.store(true, Ordering::SeqCst);
            Ok(vec![1u8])
        })
        .await
        .expect_err("the gate refuses");
    (error, polled.load(Ordering::SeqCst))
}

fn assert_busy(error: &ApiError) {
    assert_eq!(error.code.as_str(), "api.rate_limited", "{error:?}");
    assert_eq!(error.status().as_u16(), 429);
    assert_eq!(
        error.details.get("retry_after"),
        Some(&serde_json::Value::from(BUSY_RETRY_AFTER_SECS))
    );
    assert_eq!(BUSY_RETRY_AFTER_SECS, 2);
    assert!(error.retryable());
    let _ = Code::ApiRateLimited;
}

#[tokio::test]
async fn the_third_concurrent_run_is_refused_at_once_while_two_block_and_runs_after_one_ends() {
    let _serial = SERIAL.lock().await;
    let gate = Arc::new(ReportGate::new(2, 2));
    let release = Arc::new(Semaphore::new(0));
    let a = blocked(&gate, "u1", &release, 1);
    let b = blocked(&gate, "u2", &release, 1);
    in_flight(&gate, 2).await;
    assert_eq!(gate.status(), "in flight 2/2");

    // A third user is refused although it has no run of its own: the site is full.
    let (error, polled) = refused(&gate, "u3").await;
    assert_busy(&error);
    assert!(!polled, "a refused run must not start its body");
    assert_eq!(gate.in_flight(), 2, "a refusal takes no place");

    release.add_permits(1);
    let first = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if a.is_finished() || b.is_finished() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await;
    first.unwrap();
    in_flight(&gate, 1).await;

    // One place is free again: the run goes through.
    let ok = gate
        .run("u3", rows, async { Ok(vec![0u8; 3]) })
        .await
        .expect("runs once a place is free");
    assert_eq!(ok.len(), 3);

    release.add_permits(1);
    a.await.unwrap().unwrap();
    b.await.unwrap().unwrap();
    in_flight(&gate, 0).await;
}

#[tokio::test]
async fn the_per_user_cap_refuses_a_users_third_while_another_user_still_runs() {
    let _serial = SERIAL.lock().await;
    let gate = Arc::new(ReportGate::new(10, 2));
    let release = Arc::new(Semaphore::new(0));
    let a = blocked(&gate, "alice", &release, 1);
    let b = blocked(&gate, "alice", &release, 1);
    in_flight(&gate, 2).await;

    let (error, polled) = refused(&gate, "alice").await;
    assert_busy(&error);
    assert!(!polled);

    // Bob is not affected by alice's cap.
    let c = blocked(&gate, "bob", &release, 1);
    in_flight(&gate, 3).await;
    // And bob's own cap is separate from alice's count.
    let d = blocked(&gate, "bob", &release, 1);
    in_flight(&gate, 4).await;
    let (error, _) = refused(&gate, "bob").await;
    assert_busy(&error);

    release.add_permits(4);
    for h in [a, b, c, d] {
        h.await.unwrap().unwrap();
    }
    in_flight(&gate, 0).await;

    // The per-user count went back to zero with the runs: alice can start two again.
    let again = Arc::new(Semaphore::new(2));
    let e = blocked(&gate, "alice", &again, 1);
    let f = blocked(&gate, "alice", &again, 1);
    e.await.unwrap().unwrap();
    f.await.unwrap().unwrap();
}

#[tokio::test]
async fn dropping_a_blocked_request_releases_its_place() {
    let _serial = SERIAL.lock().await;
    let gate = Arc::new(ReportGate::new(1, 1));
    let release = Arc::new(Semaphore::new(0));
    let run = blocked(&gate, "alice", &release, 1);
    in_flight(&gate, 1).await;
    let (error, _) = refused(&gate, "bob").await;
    assert_busy(&error);

    // The client goes away: the task and with it the future are dropped mid-run.
    run.abort();
    assert!(run.await.unwrap_err().is_cancelled());
    in_flight(&gate, 0).await;

    let ok = gate
        .run("bob", rows, async { Ok(vec![0u8; 1]) })
        .await
        .expect("the place came back");
    assert_eq!(ok.len(), 1);
}

#[tokio::test]
async fn a_failing_body_frees_its_place_and_keeps_its_own_error() {
    let _serial = SERIAL.lock().await;
    let gate = ReportGate::new(1, 1);
    let error = gate
        .run("alice", rows, async {
            Err::<Vec<u8>, _>(ApiError::from_code(Code::PermissionDenied))
        })
        .await
        .expect_err("fails");
    assert_eq!(error.code.as_str(), "permission.denied");
    assert_eq!(gate.in_flight(), 0);
    assert!(gate.try_acquire("alice").is_ok());
}

#[test]
fn the_gate_takes_no_place_for_a_refusal_and_a_dropped_permit_is_counted_out() {
    let _serial = SERIAL.blocking_lock();
    let gate = ReportGate::new(1, 1);
    let permit = gate.try_acquire("a").unwrap();
    assert!(gate.try_acquire("a").is_err());
    assert!(gate.try_acquire("b").is_err());
    assert_eq!(gate.in_flight(), 1);
    drop(permit);
    assert_eq!(gate.in_flight(), 0);
    assert!(gate.try_acquire("b").is_ok());
}

#[test]
fn the_result_classes_follow_the_error_code() {
    let of = |code: Code| RunResult::of_error(&ApiError::from_code(code));
    assert_eq!(of(Code::ServerUnavailable), RunResult::Unavailable);
    assert_eq!(of(Code::ApiRateLimited), RunResult::RefusedBusy);
    assert_eq!(of(Code::PermissionDenied), RunResult::Denied);
    assert_eq!(of(Code::PermissionFieldDenied), RunResult::Denied);
    assert_eq!(of(Code::ReportNotFound), RunResult::Denied);
    assert_eq!(of(Code::ReportBadArgument), RunResult::Failed);
    assert_eq!(
        RunResult::of_error(&ApiError::app("report.timeout", "x")),
        RunResult::Timeout
    );
    assert_eq!(
        RunResult::of_error(&ApiError::app("report.failed", "x")),
        RunResult::Failed
    );
    let labels: Vec<&str> = [
        RunResult::Ok,
        RunResult::Timeout,
        RunResult::Unavailable,
        RunResult::RefusedBusy,
        RunResult::Denied,
        RunResult::Failed,
    ]
    .iter()
    .map(|r| r.as_str())
    .collect();
    assert_eq!(
        labels,
        [
            "ok",
            "timeout",
            "unavailable",
            "refused_busy",
            "denied",
            "failed"
        ]
    );
}

// ---- settings ----

#[test]
fn the_defaults_are_four_and_two() {
    let s = Settings::default();
    assert_eq!((s.max_concurrent, s.max_concurrent_per_user), (4, 2));
    assert_eq!(DEFAULT_MAX_CONCURRENT, 4);
    assert_eq!(DEFAULT_MAX_CONCURRENT_PER_USER, 2);
    assert!(s.enabled);
    s.validate().unwrap();
    let parsed: Settings = toml::from_str("").unwrap();
    assert_eq!(parsed.max_concurrent, 4);
}

#[test]
fn the_limits_are_read_from_the_services_table() {
    let s: Settings = toml::from_str("max_concurrent = 8\nmax_concurrent_per_user = 3").unwrap();
    assert_eq!((s.max_concurrent, s.max_concurrent_per_user), (8, 3));
    s.validate().unwrap();
}

#[test]
fn a_zero_limit_is_a_start_up_error_naming_the_key() {
    let s: Settings = toml::from_str("max_concurrent = 0").unwrap();
    let e = s.validate().unwrap_err().to_string();
    assert!(e.contains("max_concurrent"), "{e}");
    assert!(!e.contains("max_concurrent_per_user"), "{e}");

    let s: Settings = toml::from_str("max_concurrent_per_user = 0").unwrap();
    let e = s.validate().unwrap_err().to_string();
    assert!(e.contains("max_concurrent_per_user"), "{e}");
}

#[test]
fn an_unknown_key_is_still_refused() {
    let e = toml::from_str::<Settings>("max_concurent = 2").unwrap_err();
    assert!(e.to_string().contains("max_concurent"), "{e}");
}

// ---- metrics ----

fn value(key: &str) -> f64 {
    spot_telemetry::render_prometheus()
        .lines()
        .find(|l| l.starts_with(key))
        .and_then(|l| l.rsplit(' ').next())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0)
}

/// One test: the recorder is process-global, so the counters are compared before and after.
#[tokio::test]
async fn the_metrics_show_the_mix_of_outcomes_and_carry_no_report_or_user_label() {
    let _serial = SERIAL.lock().await;
    spot_telemetry::metrics::install().unwrap();
    let result = |r: &str| format!(r#"spot_report_runs_total{{result="{r}"}}"#);
    let classes = [
        "ok",
        "timeout",
        "unavailable",
        "refused_busy",
        "denied",
        "failed",
    ];
    let before: Vec<f64> = classes.iter().map(|c| value(&result(c))).collect();
    let rows_before = value("spot_report_rows_returned_total");
    let seconds_before = value("spot_report_run_seconds_count");

    let gate = Arc::new(ReportGate::new(1, 1));
    let ok = gate
        .run("user-zebra-17", rows, async { Ok(vec![0u8; 7]) })
        .await
        .unwrap();
    assert_eq!(ok.len(), 7);
    for error in [
        ApiError::app("report.timeout", "x"),
        ApiError::from_code(Code::ServerUnavailable),
        ApiError::from_code(Code::ReportNotFound),
        ApiError::app("report.failed", "x"),
    ] {
        gate.run("user-zebra-17", rows, async { Err::<Vec<u8>, _>(error) })
            .await
            .unwrap_err();
    }
    let release = Arc::new(Semaphore::new(0));
    let held = blocked(&gate, "user-zebra-17", &release, 0);
    in_flight(&gate, 1).await;
    assert_eq!(value("spot_report_in_flight"), 1.0);
    refused(&gate, "other").await;
    release.add_permits(1);
    held.await.unwrap().unwrap();
    in_flight(&gate, 0).await;
    assert_eq!(value("spot_report_in_flight"), 0.0);

    let after: Vec<f64> = classes.iter().map(|c| value(&result(c))).collect();
    let delta: Vec<f64> = after.iter().zip(&before).map(|(a, b)| a - b).collect();
    // ok: the first run and the held one; one each of the others.
    assert_eq!(delta, vec![2.0, 1.0, 1.0, 1.0, 1.0, 1.0]);
    assert_eq!(value("spot_report_rows_returned_total") - rows_before, 7.0);
    // Every admitted run is timed (6), a refusal is not.
    assert_eq!(value("spot_report_run_seconds_count") - seconds_before, 6.0);

    let text = spot_telemetry::render_prometheus();
    assert!(
        !text
            .lines()
            .any(|l| l.contains("spot_report") && (l.contains("zebra") || l.contains("report=")))
    );
}
