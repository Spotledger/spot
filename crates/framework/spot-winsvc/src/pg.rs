//! The PostgreSQL side of the ordering state machine: a small [`PgControl`] seam over
//! `spot-pg-supervisor`'s [`Supervisor`](spot_pg_supervisor::Supervisor), so the
//! orchestrator ([`crate::orchestrator`]) reacts to PostgreSQL's lifecycle (ready, crash,
//! restart, budget exhausted) without depending on its concrete type. Tests exercise a
//! [`FakePg`] that publishes [`PgLifecycle`] transitions by hand — no real PostgreSQL
//! binary is needed to prove the orchestrator's reaction to a crash.

use spot_pg_supervisor::{Handle, Supervisor, SupervisorError, SupervisorState};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, watch};

/// PostgreSQL's lifecycle as the orchestrator sees it (a simplified, owned mirror of
/// [`SupervisorState`] — owned so [`FakePg`] in tests never needs the real crate's
/// internal construction rules).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PgLifecycle {
    /// Not running.
    Down,
    /// Starting, not yet accepting connections.
    Starting,
    /// Accepting connections.
    Ready,
    /// Crash-restarting (1-based attempt).
    Restarting {
        /// 1-based restart attempt.
        attempt: u32,
    },
    /// Terminal failure (start timeout, live postmaster conflict, restart budget
    /// exhausted).
    Failed {
        /// Human-readable cause; never a secret.
        reason: String,
    },
}

impl From<SupervisorState> for PgLifecycle {
    fn from(state: SupervisorState) -> Self {
        match state {
            SupervisorState::Stopped => PgLifecycle::Down,
            SupervisorState::Starting => PgLifecycle::Starting,
            SupervisorState::Ready => PgLifecycle::Ready,
            SupervisorState::Stopping => PgLifecycle::Down,
            SupervisorState::Restarting { attempt } => PgLifecycle::Restarting { attempt },
            SupervisorState::Failed { reason } => PgLifecycle::Failed { reason },
        }
    }
}

/// What the orchestrator needs from PostgreSQL: start it, watch it, stop it. Plain
/// `async fn` in the trait (no `async-trait` crate): every orchestrator in this crate is
/// generic over `PgControl`, never a `dyn` object, so the trait does not need to be
/// object-safe.
#[allow(async_fn_in_trait)]
pub trait PgControl: Send + Sync {
    /// Watches lifecycle transitions.
    fn subscribe(&self) -> watch::Receiver<PgLifecycle>;
    /// The lifecycle right now.
    fn state(&self) -> PgLifecycle;
    /// Starts (or resumes watching) PostgreSQL.
    async fn start(&self) -> Result<(), crate::error::WinsvcError>;
    /// Stops PostgreSQL gracefully within `grace`, then forces it.
    async fn stop(&self, grace: Duration) -> Result<(), crate::error::WinsvcError>;
}

/// The real adapter over a bundled [`Supervisor`].
pub struct RealPg {
    supervisor: Arc<Supervisor>,
    handle: Mutex<Option<Handle>>,
}

impl RealPg {
    /// Wraps an already-built supervisor (the caller already called
    /// [`Supervisor::ensure_initialised`]).
    #[must_use]
    pub fn new(supervisor: Arc<Supervisor>) -> Self {
        RealPg {
            supervisor,
            handle: Mutex::new(None),
        }
    }
}

impl PgControl for RealPg {
    fn subscribe(&self) -> watch::Receiver<PgLifecycle> {
        let mut rx = self.supervisor.subscribe();
        let (tx, out_rx) = watch::channel(PgLifecycle::from(rx.borrow_and_update().clone()));
        tokio::spawn(async move {
            while rx.changed().await.is_ok() {
                let state = rx.borrow_and_update().clone();
                if tx.send(PgLifecycle::from(state)).is_err() {
                    break;
                }
            }
        });
        out_rx
    }

    fn state(&self) -> PgLifecycle {
        PgLifecycle::from(self.supervisor.state())
    }

    async fn start(&self) -> Result<(), crate::error::WinsvcError> {
        let new_handle = self
            .supervisor
            .start()
            .await
            .map_err(crate::error::WinsvcError::from)?;
        *self.handle.lock().await = Some(new_handle);
        Ok(())
    }

    async fn stop(&self, grace: Duration) -> Result<(), crate::error::WinsvcError> {
        let handle = self.handle.lock().await.take();
        match handle {
            Some(handle) => {
                handle
                    .stop(grace)
                    .await
                    .map_err(crate::error::WinsvcError::from)?;
                Ok(())
            }
            None => Ok(()),
        }
    }
}

/// `SupervisorError` already implements `std::error::Error`; this keeps the chain visible
/// in logs without ever reformatting a secret (the supervisor's own errors never carry
/// one — see `spot-pg-supervisor`'s `InitdbFailed`).
pub fn describe(err: &SupervisorError) -> String {
    err.to_string()
}

#[cfg(test)]
pub mod fake {
    //! An in-memory [`PgControl`] for orchestrator tests: the test drives
    //! [`FakePg::set_state`] directly to simulate a crash, a restart, or the budget
    //! running out, with no real PostgreSQL binary involved.
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A [`PgControl`] whose state is set by the test.
    pub struct FakePg {
        tx: watch::Sender<PgLifecycle>,
        starts: AtomicU32,
        stops: AtomicU32,
        fail_start: std::sync::atomic::AtomicBool,
    }

    impl FakePg {
        /// A fake starting `Down`.
        #[must_use]
        pub fn new() -> Arc<Self> {
            let (tx, _rx) = watch::channel(PgLifecycle::Down);
            Arc::new(FakePg {
                tx,
                starts: AtomicU32::new(0),
                stops: AtomicU32::new(0),
                fail_start: std::sync::atomic::AtomicBool::new(false),
            })
        }

        /// Publishes a new lifecycle state, as the real supervisor's monitor task would.
        pub fn set_state(&self, state: PgLifecycle) {
            let _ = self.tx.send(state);
        }

        /// How many times [`PgControl::start`] was called.
        #[must_use]
        pub fn start_count(&self) -> u32 {
            self.starts.load(Ordering::SeqCst)
        }

        /// How many times [`PgControl::stop`] was called.
        #[must_use]
        pub fn stop_count(&self) -> u32 {
            self.stops.load(Ordering::SeqCst)
        }

        /// Makes the next [`PgControl::start`] fail (simulates a prerequisite missing).
        pub fn fail_next_start(&self) {
            self.fail_start.store(true, Ordering::SeqCst);
        }
    }

    impl PgControl for FakePg {
        fn subscribe(&self) -> watch::Receiver<PgLifecycle> {
            self.tx.subscribe()
        }

        fn state(&self) -> PgLifecycle {
            self.tx.borrow().clone()
        }

        async fn start(&self) -> Result<(), crate::error::WinsvcError> {
            self.starts.fetch_add(1, Ordering::SeqCst);
            if self.fail_start.swap(false, Ordering::SeqCst) {
                return Err(crate::error::WinsvcError::failed("fake start failure"));
            }
            self.set_state(PgLifecycle::Starting);
            self.set_state(PgLifecycle::Ready);
            Ok(())
        }

        async fn stop(&self, _grace: Duration) -> Result<(), crate::error::WinsvcError> {
            self.stops.fetch_add(1, Ordering::SeqCst);
            self.set_state(PgLifecycle::Down);
            Ok(())
        }
    }
}
