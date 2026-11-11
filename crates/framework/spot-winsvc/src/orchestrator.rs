//! The ordering state machine (brief: `run` starts PostgreSQL, waits for it to be ready,
//! then starts `spot serve`, and reports `SERVICE_RUNNING` only once `/api/health/ready`
//! answers; on stop it reverses the order, and a PostgreSQL crash restarts it with
//! backoff and degrades readiness meanwhile).
//!
//! [`Orchestrator`] is generic over [`PgControl`] and [`AppControl`] so the Windows
//! service entry point wires in the real supervisor and child process, while every test
//! here wires in [`pg::fake::FakePg`] and [`app::fake::FakeApp`] and drives their state by
//! hand — no real PostgreSQL binary or HTTP listener is needed to prove the ordering, the
//! crash-degrade-restore cycle, or the stop sequence.

use crate::app::{AppControl, AppLifecycle};
use crate::error::WinsvcError;
use crate::pg::{PgControl, PgLifecycle};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;

/// How long a failed-start rollback waits for `spot serve` to stop before this
/// orchestrator moves on to stopping PostgreSQL. Short, because a process that never
/// finished starting has nothing in-flight worth draining.
const ROLLBACK_APP_STOP_GRACE: Duration = Duration::from_secs(5);

/// How long a failed-start rollback waits for PostgreSQL's fast shutdown before this
/// orchestrator gives up and returns the original start error anyway.
const ROLLBACK_PG_STOP_GRACE: Duration = Duration::from_secs(10);

/// What the service reports to the Service Control Manager (brief: `SERVICE_RUNNING`
/// only once health answers; a PostgreSQL crash degrades readiness, never the reported
/// `SERVICE_STOPPED`/`SERVICE_RUNNING` pair itself — the service stays `SERVICE_RUNNING`
/// while PostgreSQL recovers, exactly as a desktop database blip should not bounce the
/// whole service).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Readiness {
    /// Startup in progress; the SCM should still see `SERVICE_START_PENDING`.
    Starting,
    /// Both PostgreSQL and `spot serve` are ready. Report `SERVICE_RUNNING`.
    Ready,
    /// `spot serve` was ready, but PostgreSQL is currently down/restarting: requests will
    /// fail until it recovers. The SCM still sees `SERVICE_RUNNING` — this is an
    /// application-level degradation, not a service stop.
    Degraded,
    /// A graceful stop is in progress.
    Stopping,
    /// Stopped.
    Stopped,
    /// Startup failed and will not be retried automatically.
    Failed {
        /// Human-readable cause; never a secret.
        reason: String,
    },
}

/// Drives PostgreSQL and `spot serve` through the brief's start/stop ordering and keeps
/// [`Readiness`] accurate while PostgreSQL misbehaves.
pub struct Orchestrator<P: PgControl, A: AppControl> {
    pg: Arc<P>,
    app: Arc<A>,
    readiness_tx: watch::Sender<Readiness>,
}

impl<P: PgControl + 'static, A: AppControl + 'static> Orchestrator<P, A> {
    /// Builds an orchestrator over the given PostgreSQL and app controls, `Readiness`
    /// starting at [`Readiness::Starting`].
    #[must_use]
    pub fn new(pg: Arc<P>, app: Arc<A>) -> Self {
        let (readiness_tx, _rx) = watch::channel(Readiness::Starting);
        Orchestrator {
            pg,
            app,
            readiness_tx,
        }
    }

    /// Watches [`Readiness`] transitions.
    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<Readiness> {
        self.readiness_tx.subscribe()
    }

    /// The readiness right now.
    #[must_use]
    pub fn readiness(&self) -> Readiness {
        self.readiness_tx.borrow().clone()
    }

    /// Runs the start order: PostgreSQL first, then `spot serve`, reporting
    /// [`Readiness::Ready`] only once `/api/health/ready` answers. Also starts the
    /// background watch that degrades/restores readiness as PostgreSQL's own lifecycle
    /// changes (its crash-restart backoff is `spot-pg-supervisor`'s own job; this
    /// orchestrator only reflects it).
    ///
    /// Fails closed: once PostgreSQL has started, any later failure (`spot serve` never
    /// starting, or never reaching `/api/health/ready` within `app_ready_timeout`) stops
    /// whatever was already started — the app first (if it was spawned), then PostgreSQL
    /// — before returning the error, so a partial start never leaves PostgreSQL (or the
    /// `spot serve` child) running unsupervised.
    ///
    /// # Errors
    /// Whatever [`PgControl::start`] or [`AppControl::start`]/[`AppControl::wait_ready`]
    /// return; [`Readiness`] becomes [`Readiness::Failed`] first.
    pub async fn start(&self, app_ready_timeout: Duration) -> Result<(), WinsvcError> {
        self.readiness_tx.send_replace(Readiness::Starting);

        if let Err(err) = self.pg.start().await {
            self.readiness_tx.send_replace(Readiness::Failed {
                reason: err.to_string(),
            });
            return Err(err);
        }

        if let Err(err) = self.app.start().await {
            self.readiness_tx.send_replace(Readiness::Failed {
                reason: err.to_string(),
            });
            self.rollback_after_partial_start(false).await;
            return Err(err);
        }

        if let Err(err) = self.app.wait_ready(app_ready_timeout).await {
            self.readiness_tx.send_replace(Readiness::Failed {
                reason: err.to_string(),
            });
            self.rollback_after_partial_start(true).await;
            return Err(err);
        }

        self.readiness_tx.send_replace(Readiness::Ready);
        self.spawn_pg_watch();
        Ok(())
    }

    /// Stops whatever a failed [`Orchestrator::start`] already started, in reverse
    /// order (app, then PostgreSQL), so a partial start never leaves either running.
    /// Errors from this rollback are intentionally swallowed: the caller is already
    /// returning the original start failure, and a rollback failure must never hide it
    /// or stop the other half of the rollback from running.
    async fn rollback_after_partial_start(&self, app_was_started: bool) {
        if app_was_started {
            let _ = self.app.stop(ROLLBACK_APP_STOP_GRACE).await;
        }
        let _ = self.pg.stop(ROLLBACK_PG_STOP_GRACE).await;
    }

    /// Reflects PostgreSQL's lifecycle onto [`Readiness`] for as long as the orchestrator
    /// lives: ready and running app => `Ready`; anything else (`Down`, `Starting`,
    /// `Restarting`, `Failed`) while the app was already serving => `Degraded`, never a
    /// service stop. `Readiness::Stopping`/`Stopped` set by [`Orchestrator::stop`] are
    /// never overwritten by a late PostgreSQL event (checked before every send).
    fn spawn_pg_watch(&self) {
        let mut pg_rx = self.pg.subscribe();
        let readiness_tx = self.readiness_tx.clone();
        tokio::spawn(async move {
            loop {
                let state = pg_rx.borrow_and_update().clone();
                apply_pg_state(&readiness_tx, &state);
                if pg_rx.changed().await.is_err() {
                    break;
                }
            }
        });
    }

    /// Runs the stop order (brief): send the stop signal to `spot serve`, wait up to
    /// `app_grace`, then stop PostgreSQL gracefully (fast shutdown) within `pg_grace`.
    ///
    /// # Errors
    /// The first failure from either stop; the other stop still runs (a failed app stop
    /// must never leave PostgreSQL running forever).
    pub async fn stop(&self, app_grace: Duration, pg_grace: Duration) -> Result<(), WinsvcError> {
        self.readiness_tx.send_replace(Readiness::Stopping);
        let app_result = self.app.stop(app_grace).await;
        let pg_result = self.pg.stop(pg_grace).await;
        self.readiness_tx.send_replace(Readiness::Stopped);
        app_result?;
        pg_result?;
        Ok(())
    }
}

/// Pure mapping from a PostgreSQL lifecycle transition to the orchestrator's reported
/// readiness, never overwriting a terminal `Stopping`/`Stopped`. Free function (not a
/// method) so the state-machine rule is unit-tested without an `Orchestrator` at all.
fn apply_pg_state(readiness_tx: &watch::Sender<Readiness>, pg_state: &PgLifecycle) {
    let current = readiness_tx.borrow().clone();
    if matches!(current, Readiness::Stopping | Readiness::Stopped) {
        return;
    }
    let next = match pg_state {
        PgLifecycle::Ready => Readiness::Ready,
        PgLifecycle::Down | PgLifecycle::Starting | PgLifecycle::Restarting { .. } => {
            Readiness::Degraded
        }
        PgLifecycle::Failed { reason } => Readiness::Failed {
            reason: reason.clone(),
        },
    };
    if next != current {
        readiness_tx.send_replace(next);
    }
}

/// Pure mapping from the app's own lifecycle to whether the orchestrator should treat it
/// as still serving (used by a future supervising loop; kept here so the pairing with
/// [`apply_pg_state`] is obvious and both are tested the same way).
#[must_use]
pub fn app_is_serving(state: &AppLifecycle) -> bool {
    matches!(state, AppLifecycle::Ready)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::app::fake::FakeApp;
    use crate::pg::fake::FakePg;
    use std::time::Duration;

    #[tokio::test]
    async fn start_runs_postgresql_before_the_app_and_reports_ready_after_health() {
        let pg = FakePg::new();
        let app = FakeApp::new(0);
        let orchestrator = Orchestrator::new(pg.clone(), app.clone());

        orchestrator
            .start(Duration::from_secs(1))
            .await
            .expect("start succeeds");

        assert_eq!(pg.start_count(), 1);
        assert_eq!(app.start_count(), 1);
        assert_eq!(orchestrator.readiness(), Readiness::Ready);
    }

    #[tokio::test]
    async fn a_failing_postgresql_start_never_starts_the_app() {
        let pg = FakePg::new();
        pg.fail_next_start();
        let app = FakeApp::new(0);
        let orchestrator = Orchestrator::new(pg.clone(), app.clone());

        let err = orchestrator
            .start(Duration::from_secs(1))
            .await
            .expect_err("postgresql failed to start");

        assert_eq!(app.start_count(), 0);
        assert!(matches!(orchestrator.readiness(), Readiness::Failed { .. }));
        assert_eq!(err.exit_code(), 3);
    }

    #[tokio::test]
    async fn a_failing_app_start_stops_postgresql_to_fail_closed() {
        let pg = FakePg::new();
        let app = FakeApp::new(0);
        app.fail_next_start();
        let orchestrator = Orchestrator::new(pg.clone(), app.clone());

        let err = orchestrator
            .start(Duration::from_secs(1))
            .await
            .expect_err("app failed to start");

        assert!(matches!(orchestrator.readiness(), Readiness::Failed { .. }));
        assert_eq!(pg.start_count(), 1);
        assert_eq!(
            pg.stop_count(),
            1,
            "a partial start (postgresql up, app failed) must stop postgresql"
        );
        // The app itself never started, so it is never asked to stop.
        assert_eq!(app.stop_count(), 0);
        assert_eq!(err.exit_code(), 3);
    }

    #[tokio::test]
    async fn a_failing_wait_ready_stops_both_the_app_and_postgresql() {
        let pg = FakePg::new();
        let app = FakeApp::new(0);
        app.fail_next_wait_ready();
        let orchestrator = Orchestrator::new(pg.clone(), app.clone());

        let err = orchestrator
            .start(Duration::from_secs(1))
            .await
            .expect_err("app never became ready");

        assert!(matches!(orchestrator.readiness(), Readiness::Failed { .. }));
        assert_eq!(pg.start_count(), 1);
        assert_eq!(
            pg.stop_count(),
            1,
            "a partial start (app started but never ready) must stop postgresql"
        );
        assert_eq!(
            app.stop_count(),
            1,
            "the started-but-never-ready app must be stopped too"
        );
        assert_eq!(err.exit_code(), 3);
    }

    #[tokio::test]
    async fn stop_stops_the_app_before_postgresql() {
        let pg = FakePg::new();
        let app = FakeApp::new(0);
        let orchestrator = Orchestrator::new(pg.clone(), app.clone());
        orchestrator
            .start(Duration::from_secs(1))
            .await
            .expect("start succeeds");

        orchestrator
            .stop(Duration::from_secs(1), Duration::from_secs(1))
            .await
            .expect("stop succeeds");

        assert_eq!(app.stop_count(), 1);
        assert_eq!(pg.stop_count(), 1);
        assert_eq!(orchestrator.readiness(), Readiness::Stopped);
    }

    #[tokio::test]
    async fn a_postgresql_crash_degrades_readiness_without_stopping_the_service() {
        let pg = FakePg::new();
        let app = FakeApp::new(0);
        let orchestrator = Orchestrator::new(pg.clone(), app.clone());
        orchestrator
            .start(Duration::from_secs(1))
            .await
            .expect("start succeeds");
        assert_eq!(orchestrator.readiness(), Readiness::Ready);

        // The real supervisor publishes this sequence on a crash: Restarting then Ready
        // again, all inside its own backoff budget (spot-pg-supervisor's job, not
        // re-tested here).
        pg.set_state(PgLifecycle::Restarting { attempt: 1 });
        wait_until(|| orchestrator.readiness() == Readiness::Degraded).await;

        pg.set_state(PgLifecycle::Ready);
        wait_until(|| orchestrator.readiness() == Readiness::Ready).await;
    }

    #[tokio::test]
    async fn a_postgresql_restart_budget_exhausted_is_reported_failed() {
        let pg = FakePg::new();
        let app = FakeApp::new(0);
        let orchestrator = Orchestrator::new(pg.clone(), app.clone());
        orchestrator
            .start(Duration::from_secs(1))
            .await
            .expect("start succeeds");

        pg.set_state(PgLifecycle::Failed {
            reason: "restart budget exhausted".to_string(),
        });
        wait_until(|| matches!(orchestrator.readiness(), Readiness::Failed { .. })).await;
    }

    #[tokio::test]
    async fn a_late_postgresql_event_after_stop_never_resurrects_readiness() {
        let pg = FakePg::new();
        let app = FakeApp::new(0);
        let orchestrator = Orchestrator::new(pg.clone(), app.clone());
        orchestrator
            .start(Duration::from_secs(1))
            .await
            .expect("start succeeds");
        orchestrator
            .stop(Duration::from_secs(1), Duration::from_secs(1))
            .await
            .expect("stop succeeds");

        pg.set_state(PgLifecycle::Ready);
        // Give the background watch a chance to run; it must not override `Stopped`.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(orchestrator.readiness(), Readiness::Stopped);
    }

    #[tokio::test]
    async fn app_is_serving_is_true_only_when_ready() {
        assert!(app_is_serving(&AppLifecycle::Ready));
        assert!(!app_is_serving(&AppLifecycle::Down));
        assert!(!app_is_serving(&AppLifecycle::Starting));
        assert!(!app_is_serving(&AppLifecycle::Exited { code: Some(1) }));
    }

    async fn wait_until(mut condition: impl FnMut() -> bool) {
        for _ in 0..200 {
            if condition() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(condition(), "condition never became true");
    }
}
