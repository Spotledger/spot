//! A tiny `block_on` for the registry's async methods.
//!
//! `Registry::publish` and the blob store are `async` but never wait on anything outside the
//! thread (the filesystem and memory adapters do blocking I/O inside the future). A worker
//! thread therefore drives them with this parker-based executor; no tokio runtime is involved,
//! which matters because the PostgreSQL bridge refuses any call from inside a runtime.

use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

struct ThreadWake(std::thread::Thread);

impl Wake for ThreadWake {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

/// Runs `future` to completion on the calling thread.
pub(crate) fn block_on<F: Future>(future: F) -> F::Output {
    let waker = Waker::from(Arc::new(ThreadWake(std::thread::current())));
    let mut context = Context::from_waker(&waker);
    let mut future = pin!(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::park(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::block_on;

    #[test]
    fn it_completes_a_ready_future() {
        assert_eq!(block_on(async { 4 + 5 }), 9);
    }
}
