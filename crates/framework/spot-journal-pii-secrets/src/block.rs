//! Runs a provider future to completion on the calling thread.
//!
//! `SubjectKeys` is synchronous and `KeyProvider` is async. The providers in `spot-secrets`
//! answer without waiting on a reactor, so parking the thread until the waker fires is enough
//! and no runtime is pulled in. A provider that needs a Tokio reactor must be wrapped by the
//! host into one that does not.

use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread};

struct Unpark(Thread);

impl Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

pub(crate) fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let waker = Waker::from(Arc::new(Unpark(thread::current())));
    let mut cx = Context::from_waker(&waker);
    loop {
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(value) => return value,
            Poll::Pending => thread::park(),
        }
    }
}
