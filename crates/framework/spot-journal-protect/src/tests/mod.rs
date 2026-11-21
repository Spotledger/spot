//! Tests of the entry transforms. The end-to-end test builds a real journal (relay, sealed
//! segments, signed manifest, snapshot) in a `MemoryStore` and rebuilds it through the executor.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod catalogue_tests;
mod journal_tests;
mod line_tests;
mod parts_tests;
mod reveal_tests;
mod support;

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
