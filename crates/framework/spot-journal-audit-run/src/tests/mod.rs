//! Tests of the audit runner. The fixture builds a real journal (sealed segments, a signed
//! manifest, a signed snapshot) in a `MemoryStore` from a script of events; the live side is an
//! in-memory side built from the same script, so a clean run must match it exactly.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod fixture;
mod property_tests;
mod run_tests;

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
