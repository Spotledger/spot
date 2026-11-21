//! The [`AsyncTaskStore`] port: the same six operations as [`TaskStore`], as futures, so the task
//! inbox can be driven from a tokio runtime thread (WP-BPM-05.T16).
//!
//! [`TaskStore`] is synchronous. A PostgreSQL store behind it goes through a blocking bridge that
//! refuses a call made from a runtime thread, and the inbox is a request-path feature: a user
//! claims and completes tasks in an HTTP handler, which runs on the runtime. This trait is the
//! path that runs there. A store behind it must never block a runtime worker: it awaits its
//! backend, and every call carries a deadline of its own.
//!
//! The futures are boxed and `Send` ([`TaskFuture`], the type the service already uses), so a
//! host holds the store as an `Arc<dyn AsyncTaskStore>`. No new dependency and no `async-trait`.
//!
//! [`AsyncInMemoryTaskStore`] is the reference. It does **not** repeat the rules of
//! [`InMemoryTaskStore`]: it wraps one and answers from it, so the idempotent create, the version
//! check and the event sequence are the same code. Its futures are ready at the first poll (the
//! mutex is held for a map operation, never across an await).
//!
//! An async `TaskService` (or a wrapper) over this trait is a separate task; this one delivers
//! the store.

use crate::error::TasksResult;
use crate::event::TaskEvent;
use crate::service::TaskFuture;
use crate::store::{InMemoryTaskStore, TaskStore};
use crate::task::{Task, TaskFilter, TaskId};

/// Durable storage of tasks and their events, reached without blocking a thread.
///
/// The contract of every method is the one of the same-named [`TaskStore`] method; the
/// differences are only that the answer is a future and that a backend call may fail with
/// [`TasksError::StoreFailed`](crate::TasksError::StoreFailed) when it misses its deadline (a
/// write that timed out may or may not have been applied: read it back before retrying).
pub trait AsyncTaskStore: Send + Sync {
    /// Stores a fresh task and its `created` event. Idempotent for a `(case, node)` pair: when a
    /// task already exists for the pair, the stored task is returned and nothing is written.
    ///
    /// # Errors
    /// [`TasksError::StoreFailed`](crate::TasksError::StoreFailed) when the backend fails.
    fn create(&self, task: Task, event: TaskEvent) -> TaskFuture<'_, Task>;

    /// The task with this id, when it exists.
    ///
    /// # Errors
    /// [`TasksError::StoreFailed`](crate::TasksError::StoreFailed) when the backend fails.
    fn load<'a>(&'a self, id: &'a TaskId) -> TaskFuture<'a, Option<Task>>;

    /// The task of a `(case, node)` pair, when one exists.
    ///
    /// # Errors
    /// [`TasksError::StoreFailed`](crate::TasksError::StoreFailed) when the backend fails.
    fn find_by_node<'a>(&'a self, case: &'a str, node: &'a str) -> TaskFuture<'a, Option<Task>>;

    /// Applies `task` against `expected_version`, appending `event` when one is given, and
    /// returns the stored task.
    ///
    /// # Errors
    /// [`TasksError::TaskNotFound`](crate::TasksError::TaskNotFound) when no task has the id,
    /// [`TasksError::VersionConflict`](crate::TasksError::VersionConflict) when the stored
    /// version differs, or [`TasksError::StoreFailed`](crate::TasksError::StoreFailed).
    fn commit<'a>(
        &'a self,
        id: &'a TaskId,
        expected_version: u64,
        task: Task,
        event: Option<TaskEvent>,
    ) -> TaskFuture<'a, Task>;

    /// The task's events, in sequence order.
    ///
    /// # Errors
    /// [`TasksError::StoreFailed`](crate::TasksError::StoreFailed) when the backend fails.
    fn events<'a>(&'a self, id: &'a TaskId) -> TaskFuture<'a, Vec<TaskEvent>>;

    /// The tasks matching `filter`, sorted by id.
    ///
    /// # Errors
    /// [`TasksError::StoreFailed`](crate::TasksError::StoreFailed) when the backend fails.
    fn list<'a>(&'a self, filter: &'a TaskFilter) -> TaskFuture<'a, Vec<Task>>;
}

/// The in-memory reference [`AsyncTaskStore`]: an [`InMemoryTaskStore`] behind ready futures.
/// Clones share the state, like the store they wrap.
#[derive(Debug, Clone, Default)]
pub struct AsyncInMemoryTaskStore {
    inner: InMemoryTaskStore,
}

impl AsyncInMemoryTaskStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// An async view of an existing in-memory store; both see the same tasks.
    #[must_use]
    pub fn over(inner: InMemoryTaskStore) -> Self {
        Self { inner }
    }

    /// The synchronous store this one answers from.
    #[must_use]
    pub fn sync_store(&self) -> &InMemoryTaskStore {
        &self.inner
    }
}

/// A future that is ready with `value`: the in-memory store has nothing to wait for.
fn ready<'a, T: Send + 'a>(value: TasksResult<T>) -> TaskFuture<'a, T> {
    Box::pin(std::future::ready(value))
}

impl AsyncTaskStore for AsyncInMemoryTaskStore {
    fn create(&self, task: Task, event: TaskEvent) -> TaskFuture<'_, Task> {
        ready(self.inner.create(task, event))
    }

    fn load<'a>(&'a self, id: &'a TaskId) -> TaskFuture<'a, Option<Task>> {
        ready(self.inner.load(id))
    }

    fn find_by_node<'a>(&'a self, case: &'a str, node: &'a str) -> TaskFuture<'a, Option<Task>> {
        ready(self.inner.find_by_node(case, node))
    }

    fn commit<'a>(
        &'a self,
        id: &'a TaskId,
        expected_version: u64,
        task: Task,
        event: Option<TaskEvent>,
    ) -> TaskFuture<'a, Task> {
        ready(self.inner.commit(id, expected_version, task, event))
    }

    fn events<'a>(&'a self, id: &'a TaskId) -> TaskFuture<'a, Vec<TaskEvent>> {
        ready(self.inner.events(id))
    }

    fn list<'a>(&'a self, filter: &'a TaskFilter) -> TaskFuture<'a, Vec<Task>> {
        ready(self.inner.list(filter))
    }
}
