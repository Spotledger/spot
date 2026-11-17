//! A plain-thread worker: where the blocking stores live.
//!
//! The PostgreSQL stores of the edge (`spot-pg-bridge`, ADR-082 addendum 1) are synchronous, are
//! built on a plain thread and refuse to be called from an async task (`pg_bridge.wrong_thread`).
//! The loop is async. This is the one seam between them: a [`Worker`] owns one value on its own
//! OS thread (built there, so it need not be `Send`), runs the closures it is sent one at a time
//! and hands the answers back through a oneshot channel.
//!
//! A job that started always finishes: the loop cancelling its await (a stop, a cut connection)
//! does not abort the closure, so a bundle transaction is never torn down half way. A factory
//! that failed answers every job with that error.

use std::marker::PhantomData;
use std::sync::Mutex;
use std::sync::mpsc;
use std::thread::JoinHandle;

use tokio::sync::oneshot;

use crate::error::{Error, Result};

type Job<T> = Box<dyn FnOnce(&mut std::result::Result<T, Error>) + Send>;

/// One value living on its own thread.
pub struct Worker<T: 'static> {
    jobs: Mutex<Option<mpsc::Sender<Job<T>>>>,
    thread: Mutex<Option<JoinHandle<()>>>,
    _value: PhantomData<fn() -> T>,
}

impl<T: 'static> std::fmt::Debug for Worker<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Worker").finish_non_exhaustive()
    }
}

impl<T: 'static> Worker<T> {
    /// Starts the thread; `factory` runs on it and builds the value.
    ///
    /// # Errors
    ///
    /// `sync_node_host.store_failed` when the OS refuses the thread.
    pub fn spawn(name: &str, factory: impl FnOnce() -> Result<T> + Send + 'static) -> Result<Self> {
        let (sender, receiver) = mpsc::channel::<Job<T>>();
        let thread = std::thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || {
                let mut state = factory();
                while let Ok(job) = receiver.recv() {
                    job(&mut state);
                }
            })
            .map_err(|_| Error::store_failed("the worker thread did not start"))?;
        Ok(Self {
            jobs: Mutex::new(Some(sender)),
            thread: Mutex::new(Some(thread)),
            _value: PhantomData,
        })
    }

    /// Runs `work` on the worker's thread and returns its answer.
    ///
    /// # Errors
    ///
    /// The factory's error, `work`'s own error, or `sync_node_host.store_failed` when the worker
    /// has stopped.
    pub async fn call<R: Send + 'static>(
        &self,
        work: impl FnOnce(&mut T) -> Result<R> + Send + 'static,
    ) -> Result<R> {
        let (reply, answer) = oneshot::channel::<Result<R>>();
        let job: Job<T> = Box::new(move |state| {
            let out = match state {
                Ok(value) => work(value),
                Err(error) => Err(error.clone()),
            };
            let _ = reply.send(out);
        });
        {
            let jobs = self
                .jobs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let Some(sender) = jobs.as_ref() else {
                return Err(Error::store_failed("the worker has stopped"));
            };
            sender
                .send(job)
                .map_err(|_| Error::store_failed("the worker has stopped"))?;
        }
        answer
            .await
            .map_err(|_| Error::store_failed("the worker stopped before it answered"))?
    }

    /// Stops accepting jobs, lets the one in flight finish, and waits for the thread.
    pub async fn shutdown(&self) {
        let sender = self
            .jobs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        drop(sender);
        let thread = self
            .thread
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(thread) = thread {
            let _ = tokio::task::spawn_blocking(move || thread.join()).await;
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::rc::Rc;

    use super::*;

    #[tokio::test]
    async fn a_non_send_value_lives_on_the_thread_and_jobs_run_in_order() {
        // `Rc` is not `Send`: it is built on the worker thread and never leaves it.
        let worker = Worker::<Rc<std::cell::Cell<u32>>>::spawn("test-worker", || {
            Ok(Rc::new(std::cell::Cell::new(0)))
        })
        .unwrap();
        for expected in 1..=3 {
            let seen = worker
                .call(|cell| {
                    cell.set(cell.get() + 1);
                    Ok(cell.get())
                })
                .await
                .unwrap();
            assert_eq!(seen, expected);
        }
        worker.shutdown().await;
        assert!(worker.call(|_| Ok(())).await.is_err());
    }

    #[tokio::test]
    async fn a_failed_factory_answers_every_job_with_its_error() {
        let worker =
            Worker::<u8>::spawn("test-worker", || Err(Error::store_failed("no database"))).unwrap();
        let error = worker.call(|_| Ok(())).await.unwrap_err();
        assert_eq!(error.code, crate::error::STORE_FAILED);
        worker.shutdown().await;
    }
}
