//! A minimal runner for the reference port.
//!
//! The engine's [`NodeHandler`](spot_bpm_engine::NodeHandler) contract is synchronous: a handler
//! returns its outcome, and the engine calls it outside any store transaction (WP-BPM-02.T2), so
//! a handler that must call the outside world does so through a port. The document engine's
//! public lifecycle API is asynchronous, so the reference port bridges the two.
//!
//! [`run_ready`] polls a future to completion with a no-op waker. That is enough for a service
//! whose futures are always ready — the in-memory storage fake this crate's tests use, and any
//! in-process document service that answers without I/O. A host that talks to a real database
//! supplies its own [`DocumentOps`](crate::DocumentOps) (or a runner) on its own runtime; it must
//! not use this bridge.

use std::future::Future;
use std::task::{Context, Poll, Waker};

/// Runs `future` to completion, polling with a no-op waker.
///
/// It is only safe for a future that is always ready; a future that waits on a real wakeup would
/// spin here. See the module docs.
pub(crate) fn run_ready<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}
