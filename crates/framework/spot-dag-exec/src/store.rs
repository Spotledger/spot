//! The [`ExecStore`] trait: where runs live, and the [`ExecStoreFactory`] the contract suite
//! drives.

use std::future::Future;
use std::pin::Pin;

use serde::{Deserialize, Serialize};
use spot_dag::NodeId;

use crate::error::ExecResult;
use crate::state::{Run, RunId, RunPlan, Transition};

/// A boxed, `Send` future: the return type of every [`ExecStore`] method, so the trait stays
/// object-safe and a backend can be used through `Box<dyn ExecStore>`.
pub type ExecFuture<'a, T> = Pin<Box<dyn Future<Output = ExecResult<T>> + Send + 'a>>;

/// One run with ready work: what [`ExecStore::list_ready`] returns.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadyRun {
    /// The run that has ready nodes.
    pub run: RunId,
    /// Its ready node ids, sorted by id.
    #[serde(deserialize_with = "crate::state::serde_nodes::list")]
    pub ready: Vec<NodeId>,
}

/// Durable storage of runs and node states.
///
/// Every method is `&self` so the trait is object-safe; a store uses interior mutability (the
/// in-memory reference) or a connection pool (PostgreSQL). [`ExecStore::apply`] checks the
/// `(run, node, attempt)` idempotency key first, then the optimistic version: a retried
/// completion is a no-op, and a transition against a stale version fails with
/// [`ExecError::VersionConflict`](crate::ExecError::VersionConflict). The backend-independent
/// behaviour every implementation must have is [`crate::contract::run`].
///
/// [`ExecStore::apply`] is the store's own transaction. When the caller needs the transition to
/// commit with its document work, it uses [`Unit::commit_with`](crate::Unit::commit_with)
/// instead.
pub trait ExecStore: Send + Sync {
    /// Stores a fresh run. The run id and the business key must both be unused.
    fn create_run<'a>(&'a self, plan: RunPlan) -> ExecFuture<'a, Run>;

    /// The run with this id, when it exists.
    fn load_run<'a>(&'a self, id: &'a RunId) -> ExecFuture<'a, Option<Run>>;

    /// Applies a transition against `expected_version`, returning the stored run.
    ///
    /// Idempotent for a repeated `(run, node, attempt)`: when the step's key has already been
    /// applied, the current run is returned unchanged and the version is not moved.
    fn apply<'a>(
        &'a self,
        transition: &'a Transition,
        expected_version: u64,
    ) -> ExecFuture<'a, Run>;

    /// Runs that have at least one ready node, at most `limit` of them, sorted by run id.
    fn list_ready<'a>(&'a self, limit: u32) -> ExecFuture<'a, Vec<ReadyRun>>;

    /// The run a business key names, when one exists.
    fn find_by_key<'a>(&'a self, key: &'a str) -> ExecFuture<'a, Option<Run>>;
}

/// A factory for [`ExecStore`]s that share one durable backend.
///
/// [`crate::contract::run`] drives a factory: each `open` returns a fresh store over the same
/// data, so a test can drop one store, open another and prove that state survived. The
/// in-memory factory hands back clones of one shared store; a PostgreSQL factory hands back
/// stores over the same database.
pub trait ExecStoreFactory: Send + Sync {
    /// A store over the factory's data.
    fn open(&self) -> Box<dyn ExecStore>;
}
