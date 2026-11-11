//! The engine: admission, the bounded queue, the slot semaphore, deadlines and shutdown.
//!
//! It is generic over a [`Source`] of connections so the whole policy (busy, timeout, wrong
//! thread, overload, shutdown, metrics) is tested without a database; `pg.rs` supplies the
//! deadpool source the public [`crate::Bridge`] uses.

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{RecvTimeoutError, sync_channel};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use tokio::runtime::{Builder, Handle, Runtime};
use tokio::sync::{Semaphore, watch};
use tokio::time::{Instant as TokioInstant, timeout, timeout_at};

use crate::config::BridgeConfig;
use crate::error::BridgeError;
use crate::metrics::Emitter;

/// A boxed, sendable future, as the closure of a call returns it.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The shortest deadline a call can have; a smaller one is raised to it.
const MIN_DEADLINE: Duration = Duration::from_millis(1);
/// How long past its deadline a caller waits for the worker's own answer before giving up.
const BACKSTOP: Duration = Duration::from_secs(2);
/// The most time the bridge spends sending one cancel request.
const CANCEL_BUDGET: Duration = Duration::from_secs(2);
/// After the grace period of a shutdown, how long to let the cancels go out.
const SETTLE: Duration = Duration::from_millis(250);

/// Why a source could not hand out a connection.
pub(crate) enum Failure {
    /// The wait for a connection timed out.
    Timeout,
    /// The pool is closed.
    Closed,
    /// The connection could not be made.
    Connect,
}

/// What the engine does to a call it gives up on.
pub(crate) trait Abandon: Send {
    /// Marks the connection so it is never reused, then asks the server to cancel the statement.
    /// The marking happens before the future is returned, so a dropped future still marks.
    fn abandon(self: Box<Self>) -> BoxFuture<'static, ()>;
}

/// A connection and the means to abandon it.
pub(crate) struct Checked<C> {
    pub(crate) conn: C,
    pub(crate) abandon: Box<dyn Abandon>,
}

/// Where connections come from.
pub(crate) trait Source: Send + Sync + 'static {
    type Conn: Send + 'static;
    fn checkout(&self) -> BoxFuture<'static, Result<Checked<Self::Conn>, Failure>>;
}

struct Shared<S> {
    source: S,
    cfg: BridgeConfig,
    metrics: Emitter,
    slots: Semaphore,
    admitted: AtomicUsize,
    in_flight: AtomicUsize,
    closed: AtomicBool,
    stop: watch::Sender<bool>,
    gauges: Mutex<()>,
    idle: Mutex<()>,
    idle_cv: Condvar,
}

impl<S> Shared<S> {
    /// Publishes both gauges from one consistent read. The lock makes the read and the write
    /// one step, so a slower publisher cannot overwrite a newer value with an older one; the last
    /// publish after the last change always reads the final counts.
    fn publish_gauges(&self) {
        let _step = self.gauges.lock().unwrap_or_else(PoisonError::into_inner);
        let admitted = self.admitted.load(Ordering::SeqCst);
        let running = self.in_flight.load(Ordering::SeqCst);
        self.metrics.in_flight(running);
        self.metrics.queue_depth(admitted.saturating_sub(running));
    }
}

/// Counts a call as admitted until dropped.
struct Admitted<S>(Arc<Shared<S>>);

impl<S> Drop for Admitted<S> {
    fn drop(&mut self) {
        let before = self.0.admitted.fetch_sub(1, Ordering::SeqCst);
        self.0.publish_gauges();
        if before == 1 {
            let _guard = self.0.idle.lock().unwrap_or_else(PoisonError::into_inner);
            self.0.idle_cv.notify_all();
        }
    }
}

/// Counts a call as running until dropped.
struct Running<'a, S>(&'a Shared<S>);

impl<'a, S> Running<'a, S> {
    fn start(shared: &'a Shared<S>) -> Self {
        shared.in_flight.fetch_add(1, Ordering::SeqCst);
        shared.publish_gauges();
        Self(shared)
    }
}

impl<S> Drop for Running<'_, S> {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
        self.0.publish_gauges();
    }
}

/// The engine behind [`crate::Bridge`].
pub(crate) struct Core<S: Source> {
    shared: Arc<Shared<S>>,
    handle: Handle,
    runtime: Mutex<Option<Runtime>>,
}

impl<S: Source> Core<S> {
    /// Starts the runtime. `pool_size` has been validated by the caller against the config.
    pub(crate) fn start(cfg: BridgeConfig, source: S) -> Result<Self, BridgeError<Infallible>> {
        let runtime = Builder::new_multi_thread()
            .worker_threads(cfg.workers)
            .thread_name("spot-pg-bridge")
            .enable_all()
            .build()
            .map_err(|_| BridgeError::Config {
                setting: "workers",
                reason: "the bridge runtime could not be started",
            })?;
        let (stop, _) = watch::channel(false);
        let shared = Arc::new(Shared {
            slots: Semaphore::new(cfg.max_in_flight),
            metrics: Emitter::new(&cfg.name),
            source,
            cfg,
            admitted: AtomicUsize::new(0),
            in_flight: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
            stop,
            gauges: Mutex::new(()),
            idle: Mutex::new(()),
            idle_cv: Condvar::new(),
        });
        Ok(Self {
            handle: runtime.handle().clone(),
            runtime: Mutex::new(Some(runtime)),
            shared,
        })
    }

    pub(crate) fn config(&self) -> &BridgeConfig {
        &self.shared.cfg
    }

    /// The source this engine takes connections from. A test hook: the bridge's public surface
    /// has no reason to hand it out.
    #[cfg(test)]
    pub(crate) fn source(&self) -> &S {
        &self.shared.source
    }

    pub(crate) fn run<T, E, F>(&self, deadline: Duration, op: F) -> Result<T, BridgeError<E>>
    where
        T: Send + 'static,
        E: Send + 'static,
        F: FnOnce(S::Conn) -> BoxFuture<'static, Result<T, E>> + Send + 'static,
    {
        let sh = &self.shared;
        // First, before anything is queued or touched: a runtime thread must never block here.
        if Handle::try_current().is_ok() {
            sh.metrics.wrong_thread();
            return Err(BridgeError::WrongThread);
        }
        if sh.closed.load(Ordering::SeqCst) {
            return Err(BridgeError::Closed);
        }
        let deadline = deadline.clamp(MIN_DEADLINE, sh.cfg.max_deadline);
        // Admission: queue and slots together. Full means refused now, nothing queued.
        let cap = sh.cfg.max_in_flight + sh.cfg.queue_capacity;
        loop {
            let seen = sh.admitted.load(Ordering::SeqCst);
            if seen >= cap {
                sh.metrics.busy();
                return Err(BridgeError::Busy);
            }
            if sh
                .admitted
                .compare_exchange(seen, seen + 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                break;
            }
        }
        let admitted = Admitted(Arc::clone(sh));
        sh.publish_gauges();
        let (tx, rx) = sync_channel(1);
        let started = Instant::now();
        let deadline_at = TokioInstant::now() + deadline;
        let task_shared = Arc::clone(sh);
        self.handle.spawn(async move {
            let result = run_job(&task_shared, started, deadline_at, op).await;
            // Settle the counts and gauges before the caller is told, so a caller that has its
            // answer never sees its own call still counted.
            drop(admitted);
            let _ = tx.send(result);
        });
        match rx.recv_timeout(deadline + BACKSTOP) {
            Ok(result) => result,
            Err(RecvTimeoutError::Timeout) => {
                sh.metrics.timeout();
                Err(BridgeError::Timeout)
            }
            Err(RecvTimeoutError::Disconnected) => Err(BridgeError::Closed),
        }
    }

    pub(crate) fn shutdown(&self, grace: Duration) -> Result<(), BridgeError<Infallible>> {
        if Handle::try_current().is_ok() {
            self.shared.metrics.wrong_thread();
            return Err(BridgeError::WrongThread);
        }
        let sh = &self.shared;
        sh.closed.store(true, Ordering::SeqCst);
        if !self.wait_idle(grace) {
            let _ = sh.stop.send(true);
            self.wait_idle(SETTLE);
        }
        self.drop_runtime();
        Ok(())
    }

    /// Waits until nothing is admitted or `limit` passes; true if idle.
    fn wait_idle(&self, limit: Duration) -> bool {
        let end = Instant::now() + limit;
        let mut guard = self
            .shared
            .idle
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        while self.shared.admitted.load(Ordering::SeqCst) > 0 {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            guard = self
                .shared
                .idle_cv
                .wait_timeout(guard, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        true
    }

    /// Releases the runtime without waiting for its tasks and without panicking in any context.
    fn drop_runtime(&self) {
        let taken = self
            .runtime
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(runtime) = taken {
            runtime.shutdown_background();
        }
    }
}

impl<S: Source> Drop for Core<S> {
    /// Cancels what is running and returns; it never waits, so it cannot hang.
    fn drop(&mut self) {
        self.shared.closed.store(true, Ordering::SeqCst);
        let _ = self.shared.stop.send(true);
        self.drop_runtime();
    }
}

async fn stopped(rx: &mut watch::Receiver<bool>) {
    loop {
        if *rx.borrow_and_update() {
            return;
        }
        if rx.changed().await.is_err() {
            return;
        }
    }
}

async fn run_job<S, T, E, F>(
    sh: &Shared<S>,
    started: Instant,
    deadline_at: TokioInstant,
    op: F,
) -> Result<T, BridgeError<E>>
where
    S: Source,
    F: FnOnce(S::Conn) -> BoxFuture<'static, Result<T, E>>,
{
    let mut stop = sh.stop.subscribe();
    let permit = tokio::select! {
        got = timeout_at(deadline_at, sh.slots.acquire()) => match got {
            Ok(Ok(permit)) => permit,
            Ok(Err(_)) => return Err(BridgeError::Closed),
            Err(_) => {
                sh.metrics.timeout();
                return Err(BridgeError::Timeout);
            }
        },
        () = stopped(&mut stop) => return Err(BridgeError::Closed),
    };
    let _running = Running::start(sh);
    let checked = tokio::select! {
        got = timeout_at(deadline_at, sh.source.checkout()) => match got {
            Ok(Ok(checked)) => checked,
            Ok(Err(Failure::Closed)) => return Err(BridgeError::Closed),
            Ok(Err(Failure::Connect)) => return Err(BridgeError::Connect),
            Ok(Err(Failure::Timeout)) | Err(_) => {
                sh.metrics.timeout();
                return Err(BridgeError::Timeout);
            }
        },
        () = stopped(&mut stop) => return Err(BridgeError::Closed),
    };
    sh.metrics.waited(started.elapsed());
    let ran_from = Instant::now();
    let Checked { conn, abandon } = checked;
    let mut fut = op(conn);
    let outcome = tokio::select! {
        done = timeout_at(deadline_at, &mut fut) => Some(done),
        () = stopped(&mut stop) => None,
    };
    let result = match outcome {
        Some(Ok(answer)) => {
            sh.metrics.ran(ran_from.elapsed());
            answer.map_err(BridgeError::Store)
        }
        Some(Err(_)) => {
            sh.metrics.cancelled();
            sh.metrics.timeout();
            let _ = timeout(CANCEL_BUDGET, abandon.abandon()).await;
            Err(BridgeError::Timeout)
        }
        None => {
            sh.metrics.cancelled();
            let _ = timeout(CANCEL_BUDGET, abandon.abandon()).await;
            Err(BridgeError::Closed)
        }
    };
    // The future (and the connection it owns) is dropped only now, after the connection was
    // marked and the cancel sent, so a marked connection is discarded, never pooled.
    drop(fut);
    drop(permit);
    result
}
