//! WP-ADM-02.T6: the scheduled self-update check. A fake action stands in for the update
//! code, so the tests prove the schedule, the tolerance of failure and the safe-point
//! restart; what an update must pass to be installed is `tests/update.rs`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Write;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use semver::Version;
use spot_agent::config::{AgentConfig, LogLevel};
use spot_agent::logging::Logger;
use spot_agent::run::Shutdown;
use spot_agent::update::error::UpdateError;
use spot_agent::update::runner::{UpdateReport, UpdateStatus};
use spot_agent::update::schedule::{
    ReportSink, UpdateAction, UpdateLoop, UpdateLoopEnd, run_update_loop,
};
use spot_agent::update::{CODE_FAILED, CODE_REFUSED};

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn report(status: UpdateStatus, code: Option<&'static str>, restart: bool) -> UpdateReport {
    UpdateReport {
        detail: "x".to_owned(),
        status,
        code,
        needs_restart: restart,
    }
}

/// An action that plays a script of answers, then answers "up to date".
struct Scripted {
    script: Mutex<Vec<UpdateReport>>,
    checks: AtomicUsize,
    restarts: AtomicUsize,
    boot: Option<UpdateReport>,
}

impl Scripted {
    fn new(script: Vec<UpdateReport>) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script),
            checks: AtomicUsize::new(0),
            restarts: AtomicUsize::new(0),
            boot: None,
        })
    }
}

impl UpdateAction for Scripted {
    fn boot(&self, _now: i64) -> Option<UpdateReport> {
        self.boot.clone()
    }

    fn check(&self, _now: i64) -> UpdateReport {
        self.checks.fetch_add(1, Ordering::SeqCst);
        let mut script = self.script.lock().unwrap();
        if script.is_empty() {
            report(UpdateStatus::UpToDate, None, false)
        } else {
            script.remove(0)
        }
    }

    fn restart(&self) -> Result<(), UpdateError> {
        self.restarts.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct Harness {
    ctx: UpdateLoop,
    reports: Arc<Mutex<Vec<UpdateReport>>>,
    log: Capture,
}

fn harness(action: Arc<dyn UpdateAction>, interval_ms: u64, max_checks: Option<u64>) -> Harness {
    let log = Capture::default();
    let reports: Arc<Mutex<Vec<UpdateReport>>> = Arc::new(Mutex::new(Vec::new()));
    let sink_reports = reports.clone();
    let on_report: ReportSink =
        Arc::new(move |r, _at| sink_reports.lock().unwrap().push(r.clone()));
    let ctx = UpdateLoop {
        action,
        interval: Duration::from_millis(interval_ms),
        logger: Logger::new(LogLevel::Debug, Box::new(log.clone())),
        shutdown: Shutdown::new(),
        now: Arc::new(|| 1_000),
        jitter: Arc::new(|| 0),
        safe_point: Arc::new(|| true),
        on_report,
        max_checks,
        safe_point_poll: Duration::from_millis(5),
    };
    Harness { ctx, reports, log }
}

#[tokio::test]
async fn checks_run_on_the_interval_and_are_recorded() {
    let action = Scripted::new(vec![]);
    let h = harness(action.clone(), 10, Some(3));
    let end = run_update_loop(h.ctx).await;
    assert_eq!(end, UpdateLoopEnd::CheckLimit);
    assert_eq!(action.checks.load(Ordering::SeqCst), 3);
    assert_eq!(h.reports.lock().unwrap().len(), 3);
    assert_eq!(action.restarts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn failure_refusal_and_rollback_never_stop_the_agent() {
    let failed = report(UpdateStatus::Failed("net".into()), Some(CODE_FAILED), false);
    let refused = report(
        UpdateStatus::Refused("sig".into()),
        Some(CODE_REFUSED),
        false,
    );
    let rolled = report(
        UpdateStatus::RolledBack {
            step: "probe".into(),
            reason: "unhealthy".into(),
        },
        Some(spot_agent::update::CODE_ROLLED_BACK),
        false,
    );
    let action = Scripted::new(vec![failed, refused, rolled]);
    let h = harness(action.clone(), 5, Some(5));
    let end = run_update_loop(h.ctx).await;
    assert_eq!(end, UpdateLoopEnd::CheckLimit, "the loop ran to its limit");
    let seen = h.reports.lock().unwrap().clone();
    assert_eq!(seen.len(), 5);
    assert_eq!(seen[0].code, Some(CODE_FAILED));
    assert_eq!(seen[1].code, Some(CODE_REFUSED));
    assert_eq!(seen[3].code, None, "later checks went on normally");
    assert_eq!(action.restarts.load(Ordering::SeqCst), 0);
    let log = String::from_utf8(h.log.0.lock().unwrap().clone()).unwrap();
    assert!(log.contains("agent.update_failed"));
}

/// An action whose first check panics.
struct Panics {
    checks: AtomicU64,
}

impl UpdateAction for Panics {
    fn check(&self, _now: i64) -> UpdateReport {
        assert!(self.checks.fetch_add(1, Ordering::SeqCst) != 0, "boom");
        report(UpdateStatus::UpToDate, None, false)
    }

    fn restart(&self) -> Result<(), UpdateError> {
        Ok(())
    }
}

#[tokio::test]
async fn a_panicking_check_is_a_failed_check_not_a_dead_agent() {
    let action = Arc::new(Panics {
        checks: AtomicU64::new(0),
    });
    let h = harness(action, 5, Some(3));
    let end = run_update_loop(h.ctx).await;
    assert_eq!(end, UpdateLoopEnd::CheckLimit);
    let seen = h.reports.lock().unwrap().clone();
    assert_eq!(seen.len(), 3);
    assert_eq!(seen[0].code, Some(CODE_FAILED));
    assert_eq!(seen[1].code, None);
}

#[tokio::test]
async fn an_applied_update_restarts_only_at_a_safe_point() {
    let applied = report(UpdateStatus::Applied(Version::new(1, 2, 3)), None, true);
    let action = Scripted::new(vec![applied]);
    let mut h = harness(action.clone(), 5, None);
    // Busy for the first looks, then idle.
    let looks = Arc::new(AtomicUsize::new(0));
    let seen_looks = looks.clone();
    let watched = action.clone();
    h.ctx.safe_point = Arc::new(move || {
        assert_eq!(
            watched.restarts.load(Ordering::SeqCst),
            0,
            "no restart before the safe point"
        );
        seen_looks.fetch_add(1, Ordering::SeqCst) >= 3
    });
    let end = run_update_loop(h.ctx).await;
    assert_eq!(end, UpdateLoopEnd::Restarting);
    assert_eq!(action.restarts.load(Ordering::SeqCst), 1);
    assert!(looks.load(Ordering::SeqCst) >= 4);
}

#[tokio::test]
async fn shutdown_ends_the_loop_before_a_long_interval_elapses() {
    let action = Scripted::new(vec![]);
    let mut h = harness(action.clone(), 3_600_000, None);
    h.ctx.jitter = Arc::new(|| 1000);
    let trigger = h.ctx.shutdown.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
        trigger.trigger();
    });
    let end = run_update_loop(h.ctx).await;
    assert_eq!(end, UpdateLoopEnd::Shutdown);
    assert_eq!(action.checks.load(Ordering::SeqCst), 0);
}

#[test]
fn the_interval_setting_defaults_to_six_hours_and_is_validated() {
    let cfg = AgentConfig::parse("gateway = \"https://g.example.test\"\n").unwrap();
    assert_eq!(cfg.update_check_hours, 6);
    assert_eq!(cfg.install_root.as_deref(), None);
    let off = AgentConfig::parse("gateway = \"https://g.example.test\"\nupdate_check_hours = 0\n")
        .unwrap();
    assert_eq!(off.update_check_hours, 0);
    assert!(
        AgentConfig::parse("gateway = \"https://g.example.test\"\nupdate_check_hours = 169\n")
            .is_err()
    );
    assert!(
        AgentConfig::parse("gateway = \"https://g.example.test\"\ninstall_root = \"\"\n").is_err()
    );
}
