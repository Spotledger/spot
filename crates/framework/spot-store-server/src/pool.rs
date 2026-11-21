//! The worker pool: plain OS threads fed by a bounded queue.
//!
//! `Registry`, `PgIndexStore` and the state store are synchronous, and the shared PostgreSQL
//! bridge refuses any call made from a thread that belongs to a tokio runtime. `spawn_blocking`
//! threads belong to the runtime, so they cannot be used. A handler therefore sends a boxed
//! closure to one of these threads and awaits a oneshot reply; no handler thread ever blocks.
//! A full queue is `503` with `Retry-After`: the pool is the service's admission control.
//! This is ADR-082 class C, used by a control-plane service that is a separate deployable and
//! never the tenant request path.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, PoisonError};

use tokio::sync::oneshot;

use crate::error::ApiError;

type Job = Box<dyn FnOnce() + Send + 'static>;

/// A bounded pool of OS threads.
pub(crate) struct WorkerPool {
    sender: SyncSender<Job>,
}

impl WorkerPool {
    /// Starts `threads` workers sharing a queue of `queue` waiting jobs.
    pub fn start(name: &str, threads: usize, queue: usize) -> std::io::Result<Self> {
        let (sender, receiver) = sync_channel::<Job>(queue);
        let receiver: Arc<Mutex<Receiver<Job>>> = Arc::new(Mutex::new(receiver));
        for index in 0..threads {
            let receiver = Arc::clone(&receiver);
            std::thread::Builder::new()
                .name(format!("{name}-{index}"))
                .spawn(move || worker(&receiver))?;
        }
        Ok(Self { sender })
    }

    /// Queues `work` and returns the receiver of its result, or says the queue is full.
    fn try_run<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> Result<oneshot::Receiver<T>, ApiError> {
        let (reply, receive) = oneshot::channel();
        let job: Job = Box::new(move || {
            // The receiver is gone when the client disconnected: nothing to tell it.
            let _ = reply.send(work());
        });
        match self.sender.try_send(job) {
            Ok(()) => Ok(receive),
            Err(TrySendError::Full(_)) => Err(ApiError::unavailable(
                "the server is busy; try again shortly",
            )),
            Err(TrySendError::Disconnected(_)) => {
                Err(ApiError::unavailable("the server is shutting down"))
            }
        }
    }

    /// Runs `work` on a worker thread and returns its result. A panic inside `work` becomes a
    /// `500` and does not take the worker with it.
    pub async fn call<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> T + Send + 'static,
    ) -> Result<T, ApiError> {
        let receive = self.try_run(work)?;
        receive.await.map_err(|_| ApiError::internal())
    }
}

fn worker(receiver: &Mutex<Receiver<Job>>) {
    loop {
        let job = {
            let guard = receiver.lock().unwrap_or_else(PoisonError::into_inner);
            match guard.recv() {
                Ok(job) => job,
                Err(_) => return,
            }
        };
        // A panic drops the job's reply sender, which the caller sees as an internal error.
        let _ = catch_unwind(AssertUnwindSafe(job));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::WorkerPool;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn work_runs_on_a_plain_thread_outside_the_runtime() {
        let pool = WorkerPool::start("pool-test", 2, 4).unwrap();
        let rt = runtime();
        let in_runtime = rt
            .block_on(pool.call(|| tokio::runtime::Handle::try_current().is_ok()))
            .unwrap();
        assert!(!in_runtime, "a worker thread must not belong to a runtime");
    }

    #[test]
    fn a_full_queue_is_refused_not_waited_for() {
        let pool = WorkerPool::start("pool-full", 1, 1).unwrap();
        let rt = runtime();
        let (release, hold) = std::sync::mpsc::channel::<()>();
        let started = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&started);
        // One job occupies the only worker, one fills the queue, the third is refused.
        let first = pool
            .try_run(move || {
                counter.fetch_add(1, Ordering::SeqCst);
                let _ = hold.recv();
            })
            .unwrap();
        while started.load(Ordering::SeqCst) == 0 {
            std::thread::yield_now();
        }
        let second = pool.try_run(|| ()).unwrap();
        let third = pool.try_run(|| ());
        assert_eq!(third.err().map(|e| e.status.as_u16()), Some(503));
        release.send(()).unwrap();
        rt.block_on(async {
            first.await.unwrap();
            second.await.unwrap();
        });
    }

    #[test]
    fn a_panic_is_an_internal_error_and_the_worker_survives() {
        let pool = WorkerPool::start("pool-panic", 1, 4).unwrap();
        let rt = runtime();
        let failed = rt.block_on(pool.call(|| -> u8 { panic!("boom") }));
        assert_eq!(failed.err().map(|e| e.status.as_u16()), Some(500));
        assert_eq!(rt.block_on(pool.call(|| 5)).unwrap(), 5);
    }
}
