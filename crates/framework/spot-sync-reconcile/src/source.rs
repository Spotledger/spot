//! Reading a node's storage as a scope snapshot.
//!
//! A [`DigestSource`] is one node's read of a scope. The real implementations read a node's
//! storage through the storage contract at a consistent point (a `RepeatableRead` transaction
//! on the primary, at a safe bookmark, ADR-053/ADR-046); [`MemorySource`] is the in-memory
//! reference, and the crate's tests include one over the storage fake
//! (`spot_storage_fake::FakeBackend`) that shows the read against a real backend.
//!
//! The trait is the only thing the reconciler needs, so policy (which entities a scope covers)
//! and storage (where the records live) stay outside this crate.

use std::future::Future;

use spot_storage_contract::FeedPosition;

use crate::digest::{RecordFacts, ScopeSnapshot};
use crate::error::{ReconcileError, ReconcileResult};
use crate::scope::Scope;

/// One node's storage, read as a consistent scope snapshot.
///
/// The returned snapshot must be a **consistent point**: it is the scope as of its
/// [`bookmark`](ScopeSnapshot::bookmark), so writes that commit after the bookmark never leak
/// into it and never cause false drift. A `RepeatableRead` transaction taken before the read
/// gives that, and the safe bookmark (ADR-053) names the point.
pub trait DigestSource: Send + Sync {
    /// Reads `scope` as one consistent snapshot.
    fn snapshot(
        &self,
        scope: &Scope,
    ) -> impl Future<Output = ReconcileResult<ScopeSnapshot>> + Send;
}

/// The in-memory reference source: a fixed snapshot, for tests and for replaying a recorded
/// read.
#[derive(Clone, Debug)]
pub struct MemorySource {
    scope: Scope,
    bookmark: FeedPosition,
    records: Vec<RecordFacts>,
}

impl MemorySource {
    /// A source holding `records` as the scope `scope` at `bookmark`.
    pub fn new(scope: Scope, bookmark: FeedPosition, records: Vec<RecordFacts>) -> Self {
        Self {
            scope,
            bookmark,
            records,
        }
    }

    /// The scope this source holds.
    pub fn scope(&self) -> &Scope {
        &self.scope
    }

    /// The snapshot, without the async wrapper: the reference read is already in memory.
    pub fn snapshot_now(&self, scope: &Scope) -> ScopeSnapshot {
        ScopeSnapshot::new(scope.clone(), self.bookmark, self.records.clone())
    }
}

impl DigestSource for MemorySource {
    fn snapshot(
        &self,
        scope: &Scope,
    ) -> impl Future<Output = ReconcileResult<ScopeSnapshot>> + Send {
        let result = if scope == &self.scope {
            Ok(self.snapshot_now(scope))
        } else {
            Err(ReconcileError::InvalidScope {
                reason: format!(
                    "the source holds {} but the cycle asked for {}",
                    self.scope.label(),
                    scope.label()
                ),
            })
        };
        async move { result }
    }
}
