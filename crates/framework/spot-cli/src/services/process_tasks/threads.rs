//! Threads: the plain-thread driver and the hop back to the runtime.

use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll, Wake, Waker};

use tokio::runtime::Handle;

struct ThreadWaker(std::thread::Thread);

impl Wake for ThreadWaker {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

/// Drives `future` to completion on the calling (plain) thread. The task service's futures only
/// await synchronous store calls, `Hop` results and channels, none of which need a runtime.
pub(super) fn drive<F: Future>(future: F) -> F::Output {
    let waker = Waker::from(Arc::new(ThreadWaker(std::thread::current())));
    let mut context = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::park(),
        }
    }
}

/// Runs `work` on a fresh plain thread and answers with its result, or `None` when the thread
/// could not start or died. The caller awaits on the runtime without blocking it.
pub(super) fn plain<T, F>(work: F) -> impl Future<Output = Option<T>> + Send
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (tx, rx) = tokio::sync::oneshot::channel();
    let spawned = std::thread::Builder::new()
        .name("process-tasks".to_owned())
        .spawn(move || {
            let _ = tx.send(work());
        });
    async move {
        if spawned.is_err() {
            return None;
        }
        rx.await.ok()
    }
}

/// The way a plain thread reaches the host's runtime: the work is spawned there and the answer
/// comes back over a channel. `None` means the runtime is gone (or the call was made on a runtime
/// thread, which it refuses): callers fail closed.
#[derive(Clone)]
pub struct Hop(Handle);

impl Hop {
    /// A hop onto `runtime`.
    #[must_use]
    pub fn new(runtime: Handle) -> Self {
        Self(runtime)
    }

    /// Runs `future` on the runtime and blocks this plain thread for its answer.
    pub(super) fn call<T, F>(&self, future: F) -> Option<T>
    where
        T: Send + 'static,
        F: Future<Output = T> + Send + 'static,
    {
        if Handle::try_current().is_ok() {
            // Blocking here would stall a runtime worker; refuse instead.
            return None;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        self.0.spawn(async move {
            let _ = tx.send(future.await);
        });
        rx.recv().ok()
    }

    /// Runs `future` on the runtime and awaits its answer from any executor.
    pub(super) async fn call_async<T, F>(&self, future: F) -> Option<T>
    where
        T: Send + 'static,
        F: Future<Output = T> + Send + 'static,
    {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.0.spawn(async move {
            let _ = tx.send(future.await);
        });
        rx.await.ok()
    }
}

pub(super) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}
