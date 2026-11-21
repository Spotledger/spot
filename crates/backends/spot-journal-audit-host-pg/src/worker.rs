//! A thread that owns a runtime and a pool, so synchronous code can ask the database.
//!
//! The audit job's providers are synchronous and run on the blocking pool, which still belongs
//! to a tokio runtime, where `spot-pg-bridge` refuses a call (`wrong_thread`). This worker is
//! the same answer `spot-journal-audit-pg`'s held session gives: a **plain thread the side
//! owns**. It runs a current-thread runtime for its whole life (so the connections it opens keep
//! being driven) and executes one task at a time. Tasks come from any thread; the caller blocks
//! on a plain channel for the answer.

use std::future::Future;
use std::pin::Pin;
use std::sync::mpsc;
use std::thread::JoinHandle;

use deadpool_postgres::Pool;
use spot_postgres::{PgBackend, PgConfig};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

use crate::error::HostError;

type Task = Box<dyn FnOnce(Pool) -> Pin<Box<dyn Future<Output = ()>>> + Send>;

/// One database, one thread.
pub struct SyncPg {
    tx: Option<UnboundedSender<Task>>,
    thread: Option<JoinHandle<()>>,
}

impl SyncPg {
    /// Starts the worker over a fresh pool built from `config` (it connects inside the worker's
    /// own runtime, so every connection is driven for as long as the worker lives).
    ///
    /// # Errors
    /// [`HostError::Worker`] when the thread or the runtime cannot start or the first
    /// connection fails.
    pub fn start(config: PgConfig) -> Result<Self, HostError> {
        let (tx, mut rx) = unbounded_channel::<Task>();
        let (ready_tx, ready_rx) = mpsc::channel::<bool>();
        let thread = std::thread::Builder::new()
            .name("journal-audit-db".to_owned())
            .spawn(move || {
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    let _ = ready_tx.send(false);
                    return;
                };
                runtime.block_on(async move {
                    let Ok(backend) = PgBackend::connect(config).await else {
                        let _ = ready_tx.send(false);
                        return;
                    };
                    let pool = backend.pool();
                    let _ = ready_tx.send(true);
                    while let Some(task) = rx.recv().await {
                        task(pool.clone()).await;
                    }
                });
            })
            .map_err(|_| HostError::Worker)?;
        match ready_rx.recv() {
            Ok(true) => Ok(Self {
                tx: Some(tx),
                thread: Some(thread),
            }),
            _ => Err(HostError::Worker),
        }
    }

    /// Runs `work` on the worker's runtime with its pool and blocks until it answers.
    ///
    /// # Errors
    /// [`HostError::Worker`] when the worker is gone, otherwise whatever `work` returns.
    pub fn run<T, F, Fut>(&self, work: F) -> Result<T, HostError>
    where
        T: Send + 'static,
        F: FnOnce(Pool) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T, HostError>> + 'static,
    {
        let (reply_tx, reply_rx) = mpsc::channel();
        let task: Task = Box::new(move |pool| {
            Box::pin(async move {
                let out = work(pool).await;
                let _ = reply_tx.send(out);
            })
        });
        self.tx
            .as_ref()
            .ok_or(HostError::Worker)?
            .send(task)
            .map_err(|_| HostError::Worker)?;
        reply_rx.recv().map_err(|_| HostError::Worker)?
    }
}

impl Drop for SyncPg {
    fn drop(&mut self) {
        // Closing the channel ends the worker's loop; wait for it so no connection outlives us.
        self.tx = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
