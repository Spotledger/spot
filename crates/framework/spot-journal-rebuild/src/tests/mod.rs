//! Tests of the rebuild executor. The fixtures build a real journal (sealed segments, a signed
//! manifest, a signed snapshot) in a `MemoryStore` from a script of events, and a model applies
//! the same script directly, so every test compares the executor's result with the model.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod catalogue_tests;
mod fixture;
mod ordering_tests;
mod rebuild_tests;
mod safety_tests;
mod upcast_tests;

use std::future::Future;
use std::task::{Context, Poll, Waker};

pub(crate) fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}
