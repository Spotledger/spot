//! The crate's tests: the contract suite against both backends, the layout's rules and its
//! round-trip corpus, and the crash between a staged write and its rename.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod contract_suite;
mod corpus;
mod crash;
mod layout_rules;

use std::future::Future;
use std::task::{Context, Poll, Waker};

use crate::folder::FolderStore;

/// Drives a boxed store future to completion without an async runtime.
///
/// Every store future in this crate finishes on its first poll, so a plain loop is enough; the
/// loop still exists so a mistake that leaves a future pending fails the test by hanging rather
/// than by a wrong answer.
fn block_on<F: Future>(future: F) -> F::Output {
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

/// A folder store over `dir`, for a test that already owns a temporary directory.
fn folder_at(dir: &tempfile::TempDir) -> FolderStore {
    FolderStore::new(dir.path()).unwrap()
}
