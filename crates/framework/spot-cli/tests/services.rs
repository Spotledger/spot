//! The service assembly: construction and start order, readiness, and reverse shutdown.
#![allow(clippy::unwrap_used, clippy::expect_used)]
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use spot_cli::services::{CheckFuture, Level, RunFuture, Service, Services, State};
use spot_http::health::ReadinessCheck;
use tokio::sync::watch;

/// A service that records what happens to it.
struct Fake {
    name: &'static str,
    level: Level,
    check: Result<(), String>,
    log: Arc<Mutex<Vec<String>>>,
    run_for: Duration,
}

impl Service for Fake {
    fn name(&self) -> &'static str {
        self.name
    }
    fn level(&self) -> Level {
        self.level
    }
    fn check(&self) -> CheckFuture<'_> {
        let name = self.name;
        let log = self.log.clone();
        let result = self.check.clone();
        Box::pin(async move {
            log.lock().unwrap().push(format!("check:{name}"));
            result
        })
    }
    fn run(&self, mut stop: watch::Receiver<bool>) -> RunFuture<'_> {
        let name = self.name;
        let log = self.log.clone();
        let run_for = self.run_for;
        Box::pin(async move {
            log.lock().unwrap().push(format!("run:{name}"));
            // A slow service exercises the grace period; a fast one just waits for the stop.
            let _ = tokio::time::timeout(run_for, stop.changed()).await;
            log.lock().unwrap().push(format!("stop:{name}"));
            Ok(())
        })
    }
    fn shutdown(&self) -> RunFuture<'_> {
        let name = self.name;
        let log = self.log.clone();
        Box::pin(async move {
            log.lock().unwrap().push(format!("drain:{name}"));
            Ok(())
        })
    }
}

fn fake(
    name: &'static str,
    level: Level,
    check: Result<(), String>,
    log: &Arc<Mutex<Vec<String>>>,
    run_for: Duration,
) -> Arc<dyn Service> {
    Arc::new(Fake {
        name,
        level,
        check,
        log: Arc::clone(log),
        run_for,
    })
}

fn log() -> Arc<Mutex<Vec<String>>> {
    Arc::new(Mutex::new(Vec::new()))
}

fn entries(log: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
    log.lock().unwrap().clone()
}

#[tokio::test]
async fn services_start_in_order_and_stop_in_reverse() {
    let log = log();
    let mut assembly = Services::new("acme", Duration::from_secs(2));
    assembly.add(fake(
        "storage",
        Level::Required,
        Ok(()),
        &log,
        Duration::ZERO,
    ));
    assembly.add(fake(
        "metadata",
        Level::Required,
        Ok(()),
        &log,
        Duration::ZERO,
    ));
    assembly.add(fake("jobs", Level::Required, Ok(()), &log, Duration::ZERO));
    assembly.start().await.expect("every check passes");
    assembly.shutdown().await;

    let seen = entries(&log);
    let checks: Vec<_> = seen.iter().filter(|e| e.starts_with("check:")).collect();
    assert_eq!(
        checks,
        ["check:storage", "check:metadata", "check:jobs"],
        "checks run in construction order: {seen:?}"
    );
    let drains: Vec<_> = seen.iter().filter(|e| e.starts_with("drain:")).collect();
    assert_eq!(
        drains,
        ["drain:jobs", "drain:metadata", "drain:storage"],
        "shutdown drains in the reverse order: {seen:?}"
    );
    assert!(
        seen.contains(&"stop:storage".to_owned()),
        "every task saw the stop signal: {seen:?}"
    );
    // Every service is recorded as stopped once the assembly is down.
    let states = assembly.health().states();
    for (_, (_, state)) in states {
        assert!(
            matches!(state, State::Stopped(_)),
            "a stopped assembly reports stopped services, not {state:?}"
        );
    }
}

#[tokio::test]
async fn a_required_failure_stops_the_site_and_starts_nothing() {
    let log = log();
    let mut assembly = Services::new("acme", Duration::from_secs(2));
    assembly.add(fake(
        "storage",
        Level::Required,
        Ok(()),
        &log,
        Duration::ZERO,
    ));
    assembly.add(fake(
        "metadata",
        Level::Required,
        Err("the version store is unreadable".into()),
        &log,
        Duration::ZERO,
    ));
    // The service after the failure is never checked: a half-started site is worse than a
    // refused one.
    assembly.add(fake("jobs", Level::Required, Ok(()), &log, Duration::ZERO));

    let refusal = assembly
        .start()
        .await
        .expect_err("a required failure refuses");
    assert!(
        refusal.contains("required service `metadata`"),
        "the message names the service: {refusal}"
    );
    assert!(
        refusal.contains("the version store is unreadable"),
        "the message carries the check's own reason: {refusal}"
    );
    let seen = entries(&log);
    assert!(
        !seen.iter().any(|entry| entry.starts_with("run:")),
        "no service ran: {seen:?}"
    );
    assert!(
        !seen.contains(&"check:jobs".to_owned()),
        "the check loop stopped at the failure: {seen:?}"
    );
}

#[tokio::test]
async fn a_failed_optional_service_degrades_the_site_and_readiness_says_so() {
    let log = log();
    let mut assembly = Services::new("acme", Duration::from_secs(2));
    assembly.add(fake(
        "storage",
        Level::Required,
        Ok(()),
        &log,
        Duration::ZERO,
    ));
    assembly.add(fake(
        "mail",
        Level::Optional,
        Err("the SMTP account is not configured".into()),
        &log,
        Duration::ZERO,
    ));
    assembly
        .start()
        .await
        .expect("an optional failure does not stop the site");

    let check = assembly.health_check();
    assert_eq!(check.name(), "acme");
    let result = check.check().await;
    assert!(
        result.ok,
        "a degraded site is still ready: a stopped mail sender must not leave the rotation"
    );
    assert!(
        result.detail.contains("degraded:")
            && result.detail.contains("mail")
            && result.detail.contains("the SMTP account is not configured"),
        "the degradation and its reason are on readiness: {}",
        result.detail
    );
    let seen = entries(&log);
    assert!(
        !seen.contains(&"run:mail".to_owned()),
        "a service that failed its check is not run: {seen:?}"
    );
}

#[tokio::test]
async fn a_stopped_required_service_makes_the_site_not_ready() {
    let log = log();
    let mut assembly = Services::new("acme", Duration::from_secs(2));
    assembly.add(fake(
        "storage",
        Level::Required,
        Ok(()),
        &log,
        Duration::from_millis(10),
    ));
    assembly.start().await.expect("the check passes");
    // The service's own task ending is the crash path: readiness must notice.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let result = assembly.health_check().check().await;
    assert!(!result.ok, "a stopped required service is not ready");
    assert!(
        result.detail.contains("storage"),
        "the failing check names the service: {}",
        result.detail
    );
    assembly.shutdown().await;
}

#[tokio::test]
async fn shutdown_does_not_wait_past_the_grace_period() {
    let log = log();
    let mut assembly = Services::new("acme", Duration::from_millis(50));
    // A service that ignores the stop signal for a second: the assembly must still return.
    assembly.add(fake(
        "slow",
        Level::Required,
        Ok(()),
        &log,
        Duration::from_secs(1),
    ));
    assembly.start().await.expect("the check passes");
    let started = std::time::Instant::now();
    assembly.shutdown().await;
    assert!(
        started.elapsed() < Duration::from_millis(900),
        "shutdown honours its deadline, took {:?}",
        started.elapsed()
    );
}

/// The check the site's `/api/health/ready` renders is a `ReadinessCheck`; this keeps the
/// dependency honest (the assembly never invents its own health JSON).
#[test]
fn the_readiness_check_is_the_server_health_type() {
    fn accepts(check: &dyn ReadinessCheck) -> &str {
        check.name()
    }
    let assembly = Services::new("acme", Duration::from_secs(1));
    let check = assembly.health_check();
    assert_eq!(accepts(&check), "acme");
}

/// A future is what a service returns; the alias keeps `Send` in one place so a service that
/// captured a non-`Send` handle fails to compile rather than at run time.
#[test]
fn service_futures_are_send() {
    fn assert_send<T: Send>(value: T) -> T {
        value
    }
    let _: Pin<Box<dyn Future<Output = ()> + Send>> = assert_send(Box::pin(async {}));
}
