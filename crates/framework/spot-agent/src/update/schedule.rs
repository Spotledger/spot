//! The scheduled self-update check: the loop that calls the update code of this module tree
//! on an interval, with jitter, and never lets a failed check stop the agent.
//!
//! The loop knows nothing about manifests or signatures: it asks an [`UpdateAction`] to
//! check, logs what came back (codes and short sentences only) and goes round again. A
//! check that fails, is refused, rolls back or panics is a log line and an audit record, not
//! an error of the agent. A check that applied an update ends the loop only at a **safe
//! point** (nothing accepted and unreported, see [`UpdateLoop::safe_point`]) and only after
//! the service manager was asked to restart into the new version.
//!
//! Time and jitter are injected; the loop reads no clock and no randomness itself.

use std::sync::Arc;
use std::time::Duration;

use super::error::UpdateError;
use super::runner::{UpdateReport, UpdateStatus};
use crate::logging::Logger;
use crate::run::Shutdown;

/// How long the loop waits between looks at the safe point after an applied update.
pub const SAFE_POINT_POLL: Duration = Duration::from_secs(30);

/// The pause before the first check: a random point of the interval, so a fleet that starts
/// together spreads out. `jitter` is per mille (0..=1000, larger values are clamped).
#[must_use]
pub fn first_delay(interval: Duration, jitter: u64) -> Duration {
    scaled(interval, jitter.min(1000), 1000)
}

/// The pause between two checks: the interval plus up to 20 per cent of it. `jitter` is per
/// mille (0..=1000, larger values are clamped).
#[must_use]
pub fn next_delay(interval: Duration, jitter: u64) -> Duration {
    interval + scaled(interval, jitter.min(1000), 5000)
}

/// `interval * numerator / denominator` in whole nanoseconds, saturating (no floats).
fn scaled(interval: Duration, numerator: u64, denominator: u64) -> Duration {
    let nanos = interval.as_nanos().saturating_mul(u128::from(numerator)) / u128::from(denominator);
    Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
}

/// What the loop asks of the update code. The production implementation is
/// [`super::host::HostUpdate`]; tests use fakes.
pub trait UpdateAction: Send + Sync {
    /// Start-up work (finish or undo an interrupted update). `None` when there is nothing to
    /// report.
    fn boot(&self, _now: i64) -> Option<UpdateReport> {
        None
    }

    /// One check: look for an update and install it when everything checks out. Blocking.
    fn check(&self, now: i64) -> UpdateReport;

    /// Asks the service manager to restart into the committed version.
    ///
    /// # Errors
    /// [`UpdateError`] when the request cannot be issued; the new version then starts at the
    /// next restart of the service.
    fn restart(&self) -> Result<(), UpdateError>;
}

/// Records one finished check (the agent appends it to the local audit log).
pub type ReportSink = Arc<dyn Fn(&UpdateReport, i64) + Send + Sync>;

/// Everything the loop needs.
pub struct UpdateLoop {
    /// The update code.
    pub action: Arc<dyn UpdateAction>,
    /// The pause between checks (before jitter).
    pub interval: Duration,
    /// The logger.
    pub logger: Logger,
    /// The shutdown signal.
    pub shutdown: Shutdown,
    /// Unix seconds now.
    pub now: Arc<dyn Fn() -> i64 + Send + Sync>,
    /// Jitter in per mille (0..=1000).
    pub jitter: Arc<dyn Fn() -> u64 + Send + Sync>,
    /// True when it is safe to restart: no command is accepted but unreported.
    pub safe_point: Arc<dyn Fn() -> bool + Send + Sync>,
    /// Receives every report.
    pub on_report: ReportSink,
    /// Stop after this many checks (tests); `None` runs until shutdown.
    pub max_checks: Option<u64>,
    /// How often the safe point is looked at ([`SAFE_POINT_POLL`] in production).
    pub safe_point_poll: Duration,
}

/// How the loop ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateLoopEnd {
    /// Shutdown was requested.
    Shutdown,
    /// An update was applied and the service manager was asked to restart (or the request
    /// failed and was logged: the new version starts at the next restart).
    Restarting,
    /// `max_checks` reached.
    CheckLimit,
}

async fn sleep_or_shutdown(shutdown: &Shutdown, pause: Duration) -> bool {
    tokio::select! {
        () = shutdown.wait() => true,
        () = tokio::time::sleep(pause) => false,
    }
}

fn log_report(logger: &Logger, report: &UpdateReport) {
    let message = match report.code {
        Some(code) => format!("{code}: {}", report.detail),
        None => report.detail.clone(),
    };
    match report.status {
        UpdateStatus::NoUpdate
        | UpdateStatus::UpToDate
        | UpdateStatus::Applied(_)
        | UpdateStatus::Kept(_)
        | UpdateStatus::RestartPending => logger.info("update_check", &message),
        UpdateStatus::Refused(_)
        | UpdateStatus::StepVia(_)
        | UpdateStatus::RolledBack { .. }
        | UpdateStatus::Failed(_)
        | UpdateStatus::NeedsAttention(_) => logger.warn("update_check", &message),
    }
}

/// A check that could not even run to a report (the blocking task panicked).
fn panicked_report() -> UpdateReport {
    UpdateReport {
        status: UpdateStatus::Failed("the update check did not finish".to_owned()),
        code: Some(super::CODE_FAILED),
        detail: "the update check did not finish".to_owned(),
        needs_restart: false,
    }
}

/// Runs the loop until shutdown, a restart into an applied update, or `max_checks`.
///
/// A failing check, a refused offer, a rolled-back update and a panicking check are logged,
/// reported to `on_report` and followed by the next scheduled check; none ends the loop.
pub async fn run_update_loop(ctx: UpdateLoop) -> UpdateLoopEnd {
    // Finish or undo an update the last run did not complete, before anything else.
    {
        let action = ctx.action.clone();
        let now = (ctx.now)();
        if let Ok(Some(report)) = tokio::task::spawn_blocking(move || action.boot(now)).await {
            log_report(&ctx.logger, &report);
            (ctx.on_report)(&report, now);
        }
    }
    let mut pause = first_delay(ctx.interval, (ctx.jitter)());
    let mut checks: u64 = 0;
    loop {
        if sleep_or_shutdown(&ctx.shutdown, pause).await {
            return UpdateLoopEnd::Shutdown;
        }
        let action = ctx.action.clone();
        let now = (ctx.now)();
        let report = tokio::task::spawn_blocking(move || action.check(now))
            .await
            .unwrap_or_else(|_| panicked_report());
        log_report(&ctx.logger, &report);
        (ctx.on_report)(&report, now);
        if report.needs_restart {
            return restart_at_safe_point(&ctx).await;
        }
        checks += 1;
        if ctx.max_checks.is_some_and(|max| checks >= max) {
            return UpdateLoopEnd::CheckLimit;
        }
        pause = next_delay(ctx.interval, (ctx.jitter)());
    }
}

/// Waits for a safe point, then asks for the restart. Shutdown during the wait ends the
/// loop: the committed version starts at the next start anyway.
async fn restart_at_safe_point(ctx: &UpdateLoop) -> UpdateLoopEnd {
    while !(ctx.safe_point)() {
        if sleep_or_shutdown(&ctx.shutdown, ctx.safe_point_poll).await {
            return UpdateLoopEnd::Shutdown;
        }
    }
    match ctx.action.restart() {
        Ok(()) => ctx
            .logger
            .info("update_restart", "restarting into the updated version"),
        Err(error) => ctx.logger.warn(
            "update_restart",
            &format!("{error}; the updated version starts at the next restart"),
        ),
    }
    UpdateLoopEnd::Restarting
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delays_stay_inside_their_bounds() {
        let interval = Duration::from_secs(6 * 3600);
        assert_eq!(first_delay(interval, 0), Duration::ZERO);
        assert_eq!(first_delay(interval, 1000), interval);
        assert_eq!(first_delay(interval, 99_999), interval);
        assert_eq!(next_delay(interval, 0), interval);
        assert_eq!(
            next_delay(interval, 1000),
            interval + Duration::from_secs(6 * 3600 / 5)
        );
        assert!(next_delay(interval, 500) > interval);
        assert!(next_delay(interval, 500) < next_delay(interval, 1000));
    }
}
