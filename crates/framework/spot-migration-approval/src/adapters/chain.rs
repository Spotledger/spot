//! [`ChainAuditSink`]: each [`AuditRecord`] becomes one entry of a `spot-audit-chain` chain.

use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, Thread};

use spot_audit_chain::{AuditChain, AuditRecord, AuditStore};

use crate::error::ApprovalError;
use crate::service::AuditSink;

/// Writes every approval action to an [`AuditChain`].
///
/// [`AuditSink::record`] is synchronous and `AuditChain::append` is async, so the sink drives the
/// append future on the calling thread until it completes. That is sound for any store whose
/// futures do not need another task on the same thread to make progress (the in-memory store and
/// a database store with its own pool). Call the service from `spawn_blocking` (or a plain
/// thread) when the store's futures need the caller's async runtime.
///
/// A failed append is [`ApprovalError::AuditUnavailable`]: the action is not taken.
pub struct ChainAuditSink<S: AuditStore> {
    chain: Arc<AuditChain<S>>,
}

impl<S: AuditStore> ChainAuditSink<S> {
    /// A sink over `chain`.
    #[must_use]
    pub fn new(chain: Arc<AuditChain<S>>) -> Self {
        Self { chain }
    }

    /// The chain written to (for verification).
    #[must_use]
    pub fn chain(&self) -> &Arc<AuditChain<S>> {
        &self.chain
    }
}

struct ThreadWaker(Thread);

impl Wake for ThreadWaker {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let waker = Waker::from(Arc::new(ThreadWaker(thread::current())));
    let mut cx = Context::from_waker(&waker);
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
            return value;
        }
        thread::park();
    }
}

impl<S: AuditStore> AuditSink for ChainAuditSink<S> {
    fn record(&self, record: AuditRecord) -> Result<(), ApprovalError> {
        block_on(self.chain.append(record))
            .map(|_| ())
            .map_err(|error| ApprovalError::AuditUnavailable {
                // The chain's error names a code and a reason, never the record's content.
                message: format!("the audit chain refused the record: {error}"),
            })
    }
}
