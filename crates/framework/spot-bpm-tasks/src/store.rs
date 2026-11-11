//! The [`TaskStore`] port: where tasks and their audit trails live, and the in-memory reference.
//!
//! A store holds the current [`Task`] and its append-only [`TaskEvent`] trail. [`TaskStore::commit`]
//! applies a task change and its event together against an expected version, so two people racing
//! to complete the same task cannot both win: the loser's version check fails and it sees
//! [`TasksError::VersionConflict`](crate::TasksError::VersionConflict). [`TaskStore::create`] is
//! idempotent for a `(case, node)` pair, so a Decision node replayed after a crash finds the task
//! it already made instead of creating a second one.
//!
//! [`InMemoryTaskStore`] is the reference implementation the tests use. A PostgreSQL store is the
//! host's to provide; it implements this trait and passes the same contract.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::error::{TasksError, TasksResult};
use crate::event::TaskEvent;
use crate::task::{Task, TaskFilter, TaskId};

/// Durable storage of tasks and their events.
pub trait TaskStore: Send + Sync {
    /// Stores a fresh task and its `created` event. Idempotent for a `(case, node)` pair: when a
    /// task already exists for the pair, the stored task is returned and nothing is written.
    ///
    /// # Errors
    /// [`TasksError::StoreFailed`] when the backend fails.
    fn create(&self, task: Task, event: TaskEvent) -> TasksResult<Task>;

    /// The task with this id, when it exists.
    ///
    /// # Errors
    /// [`TasksError::StoreFailed`] when the backend fails.
    fn load(&self, id: &TaskId) -> TasksResult<Option<Task>>;

    /// The task of a `(case, node)` pair, when one exists.
    ///
    /// # Errors
    /// [`TasksError::StoreFailed`] when the backend fails.
    fn find_by_node(&self, case: &str, node: &str) -> TasksResult<Option<Task>>;

    /// Applies `task` against `expected_version`, appending `event` when one is given, and
    /// returns the stored task.
    ///
    /// # Errors
    /// [`TasksError::TaskNotFound`] when no task has the id, [`TasksError::VersionConflict`]
    /// when the stored version differs, or [`TasksError::StoreFailed`].
    fn commit(
        &self,
        id: &TaskId,
        expected_version: u64,
        task: Task,
        event: Option<TaskEvent>,
    ) -> TasksResult<Task>;

    /// The task's events, in sequence order.
    ///
    /// # Errors
    /// [`TasksError::StoreFailed`] when the backend fails.
    fn events(&self, id: &TaskId) -> TasksResult<Vec<TaskEvent>>;

    /// The tasks matching `filter`, sorted by id.
    ///
    /// # Errors
    /// [`TasksError::StoreFailed`] when the backend fails.
    fn list(&self, filter: &TaskFilter) -> TasksResult<Vec<Task>>;
}

/// The in-memory state behind a lock; clones share it, so a "restarted" store sees the same data.
#[derive(Debug, Default)]
struct State {
    tasks: BTreeMap<TaskId, Task>,
    by_node: BTreeMap<(String, String), TaskId>,
    events: BTreeMap<TaskId, Vec<TaskEvent>>,
}

/// The in-memory reference [`TaskStore`].
#[derive(Debug, Clone, Default)]
pub struct InMemoryTaskStore {
    state: Arc<Mutex<State>>,
}

impl InMemoryTaskStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl TaskStore for InMemoryTaskStore {
    fn create(&self, task: Task, event: TaskEvent) -> TasksResult<Task> {
        let mut state = self.lock();
        let key = (task.case.clone(), task.node.clone());
        if let Some(existing) = state.by_node.get(&key)
            && let Some(stored) = state.tasks.get(existing)
        {
            return Ok(stored.clone());
        }
        let mut stored = task;
        stored.version = 1;
        let mut stored_event = event;
        stored_event.seq = 1;
        state.by_node.insert(key, stored.id.clone());
        state.events.insert(stored.id.clone(), vec![stored_event]);
        state.tasks.insert(stored.id.clone(), stored.clone());
        Ok(stored)
    }

    fn load(&self, id: &TaskId) -> TasksResult<Option<Task>> {
        Ok(self.lock().tasks.get(id).cloned())
    }

    fn find_by_node(&self, case: &str, node: &str) -> TasksResult<Option<Task>> {
        let state = self.lock();
        let key = (case.to_owned(), node.to_owned());
        Ok(state
            .by_node
            .get(&key)
            .and_then(|id| state.tasks.get(id))
            .cloned())
    }

    fn commit(
        &self,
        id: &TaskId,
        expected_version: u64,
        task: Task,
        event: Option<TaskEvent>,
    ) -> TasksResult<Task> {
        let mut state = self.lock();
        let current = state
            .tasks
            .get(id)
            .ok_or_else(|| TasksError::TaskNotFound { id: id.to_string() })?;
        if current.version != expected_version {
            return Err(TasksError::VersionConflict {
                id: id.to_string(),
                current: current.version,
                expected: expected_version,
            });
        }
        let mut stored = task;
        stored.version = expected_version.saturating_add(1);
        if let Some(mut stored_event) = event {
            let trail = state.events.entry(id.clone()).or_default();
            stored_event.seq = trail.len() as u64 + 1;
            trail.push(stored_event);
        }
        state.tasks.insert(id.clone(), stored.clone());
        Ok(stored)
    }

    fn events(&self, id: &TaskId) -> TasksResult<Vec<TaskEvent>> {
        Ok(self.lock().events.get(id).cloned().unwrap_or_default())
    }

    fn list(&self, filter: &TaskFilter) -> TasksResult<Vec<Task>> {
        Ok(self
            .lock()
            .tasks
            .values()
            .filter(|task| filter.matches(task))
            .cloned()
            .collect())
    }
}
