//! The command lifecycle: states, the store port with an in-memory implementation, and the
//! per-site operation lock.
//!
//! The store never holds parameters, signatures or nonces, only what the status endpoint shows.
//! The PostgreSQL implementation lives in the host module task (WP-ADM-01.T5).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::Value;

use crate::ports::BoxFuture;

/// Result JSON is capped at 64 KiB.
pub const MAX_RESULT_BYTES: usize = 64 * 1024;
/// Progress lines kept per command (older ones are dropped).
pub const MAX_PROGRESS_LINES: usize = 32;
/// Finished commands are kept this long (seconds) by the in-memory store.
pub const DEFAULT_RETENTION_SECS: i64 = 7 * 24 * 3600;
/// Concurrent commands per site.
pub const MAX_CONCURRENT_COMMANDS: usize = 4;

/// The state of a command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandState {
    /// Accepted, not yet started (`queued` on the wire).
    Accepted,
    /// Running.
    Running,
    /// Finished successfully.
    Succeeded,
    /// Finished with an error.
    Failed,
    /// Stopped at a cancel request.
    Cancelled,
}

impl CommandState {
    /// The wire name from the contract's `MgmtCommandStatus.state`.
    #[must_use]
    pub fn wire(self) -> &'static str {
        match self {
            Self::Accepted => "queued",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// Whether no further transition happens.
    #[must_use]
    pub fn is_final(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
}

/// One progress line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProgressLine {
    /// The step name (at most 120 characters).
    pub step: String,
    /// 0 to 100.
    pub percent: u8,
    /// Unix seconds.
    pub at: i64,
}

/// A stored command.
#[derive(Clone, Debug, PartialEq)]
pub struct CommandRecord {
    /// The command id.
    pub id: String,
    /// The catalogue operation id.
    pub operation: String,
    /// The principal that issued it.
    pub principal: String,
    /// The state.
    pub state: CommandState,
    /// Bounded progress lines, oldest first.
    pub progress: Vec<ProgressLine>,
    /// The redacted result (at most [`MAX_RESULT_BYTES`]).
    pub result: Option<Value>,
    /// The error code of a failed command.
    pub error_code: Option<String>,
    /// The audit reference of the acceptance record.
    pub audit_ref: String,
    /// Unix seconds of acceptance.
    pub created_at: i64,
    /// Unix seconds of the end.
    pub finished_at: Option<i64>,
    /// The `Idempotency-Key` of the request that created it.
    pub idempotency_key: String,
    /// SHA-256 (hex) of the request body, to tell a retransmission from a different request.
    pub body_digest: String,
    /// Whether the handler declared cooperative cancel.
    pub cancellable: bool,
}

/// A store failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    /// A command with this id exists.
    #[error("a command with this id already exists")]
    Duplicate,
    /// The store cannot answer.
    #[error("the command store is unavailable")]
    Unavailable,
}

/// Where commands live. Implementations must make `insert` atomic on the id.
pub trait CommandStore: Send + Sync {
    /// Adds a new command (state `Accepted`), refusing a duplicate id.
    ///
    /// # Errors
    /// [`StoreError::Duplicate`] or [`StoreError::Unavailable`].
    fn insert(&self, record: CommandRecord, now: i64) -> BoxFuture<'_, Result<(), StoreError>>;
    /// One command.
    ///
    /// # Errors
    /// [`StoreError::Unavailable`].
    fn get<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<Option<CommandRecord>, StoreError>>;
    /// The command a principal created with this idempotency key.
    ///
    /// # Errors
    /// [`StoreError::Unavailable`].
    fn find_by_idempotency<'a>(
        &'a self,
        principal: &'a str,
        key: &'a str,
    ) -> BoxFuture<'a, Result<Option<CommandRecord>, StoreError>>;
    /// Marks the command running and records the audit reference.
    ///
    /// # Errors
    /// [`StoreError::Unavailable`].
    fn start<'a>(
        &'a self,
        id: &'a str,
        audit_ref: &'a str,
    ) -> BoxFuture<'a, Result<(), StoreError>>;
    /// Adds a progress line (bounded).
    ///
    /// # Errors
    /// [`StoreError::Unavailable`].
    fn progress<'a>(
        &'a self,
        id: &'a str,
        line: ProgressLine,
    ) -> BoxFuture<'a, Result<(), StoreError>>;
    /// Moves the command to a final state; a final command is unchanged.
    ///
    /// # Errors
    /// [`StoreError::Unavailable`].
    fn finish<'a>(
        &'a self,
        id: &'a str,
        state: CommandState,
        result: Option<Value>,
        error_code: Option<String>,
        now: i64,
    ) -> BoxFuture<'a, Result<(), StoreError>>;
    /// Drops finished commands that ended before `older_than`; returns how many.
    ///
    /// # Errors
    /// [`StoreError::Unavailable`].
    fn purge(&self, older_than: i64) -> BoxFuture<'_, Result<usize, StoreError>>;
}

/// The in-memory store: one process, for tests and the development profile.
#[derive(Debug)]
pub struct MemoryCommandStore {
    inner: Mutex<HashMap<String, CommandRecord>>,
    retention_secs: i64,
}

impl Default for MemoryCommandStore {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryCommandStore {
    /// A store with the default retention.
    #[must_use]
    pub fn new() -> Self {
        Self::with_retention(DEFAULT_RETENTION_SECS)
    }

    /// A store that drops finished commands after `retention_secs`.
    #[must_use]
    pub fn with_retention(retention_secs: i64) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            retention_secs,
        }
    }

    fn lock(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, HashMap<String, CommandRecord>>, StoreError> {
        self.inner.lock().map_err(|_| StoreError::Unavailable)
    }

    fn insert_inner(&self, record: CommandRecord, now: i64) -> Result<(), StoreError> {
        let mut map = self.lock()?;
        Self::purge_locked(&mut map, now - self.retention_secs);
        if map.contains_key(&record.id) {
            return Err(StoreError::Duplicate);
        }
        map.insert(record.id.clone(), record);
        Ok(())
    }

    fn purge_locked(map: &mut HashMap<String, CommandRecord>, older_than: i64) -> usize {
        let before = map.len();
        map.retain(|_, record| match record.finished_at {
            Some(at) => at >= older_than,
            None => true,
        });
        before - map.len()
    }
}

fn ready<T: Send + 'static>(value: T) -> BoxFuture<'static, T> {
    Box::pin(std::future::ready(value))
}

impl CommandStore for MemoryCommandStore {
    fn insert(&self, record: CommandRecord, now: i64) -> BoxFuture<'_, Result<(), StoreError>> {
        let result = self.insert_inner(record, now);
        ready(result)
    }

    fn get<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<Option<CommandRecord>, StoreError>> {
        ready(self.lock().map(|map| map.get(id).cloned()))
    }

    fn find_by_idempotency<'a>(
        &'a self,
        principal: &'a str,
        key: &'a str,
    ) -> BoxFuture<'a, Result<Option<CommandRecord>, StoreError>> {
        ready(self.lock().map(|map| {
            map.values()
                .find(|r| r.principal == principal && r.idempotency_key == key)
                .cloned()
        }))
    }

    fn start<'a>(
        &'a self,
        id: &'a str,
        audit_ref: &'a str,
    ) -> BoxFuture<'a, Result<(), StoreError>> {
        ready(self.lock().map(|mut map| {
            if let Some(record) = map.get_mut(id) {
                record.audit_ref = audit_ref.to_owned();
                if record.state == CommandState::Accepted {
                    record.state = CommandState::Running;
                }
            }
        }))
    }

    fn progress<'a>(
        &'a self,
        id: &'a str,
        line: ProgressLine,
    ) -> BoxFuture<'a, Result<(), StoreError>> {
        ready(self.lock().map(|mut map| {
            if let Some(record) = map.get_mut(id)
                && !record.state.is_final()
            {
                record.progress.push(line);
                let excess = record.progress.len().saturating_sub(MAX_PROGRESS_LINES);
                if excess > 0 {
                    record.progress.drain(..excess);
                }
            }
        }))
    }

    fn finish<'a>(
        &'a self,
        id: &'a str,
        state: CommandState,
        result: Option<Value>,
        error_code: Option<String>,
        now: i64,
    ) -> BoxFuture<'a, Result<(), StoreError>> {
        ready(self.lock().map(|mut map| {
            if let Some(record) = map.get_mut(id)
                && !record.state.is_final()
            {
                record.state = state;
                record.result = result;
                record.error_code = error_code;
                record.finished_at = Some(now);
            }
        }))
    }

    fn purge(&self, older_than: i64) -> BoxFuture<'_, Result<usize, StoreError>> {
        ready(
            self.lock()
                .map(|mut map| Self::purge_locked(&mut map, older_than)),
        )
    }
}

/// Why a lock could not be taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockDenied {
    /// An exclusive command holds the site, or an exclusive one wants it and others run.
    Conflict,
    /// Four commands already run.
    Saturated,
}

#[derive(Debug, Default)]
struct LockState {
    running: usize,
    exclusive: bool,
}

/// The per-site operation lock: up to [`MAX_CONCURRENT_COMMANDS`] shared holders, or one
/// exclusive holder (destructive operations).
#[derive(Debug, Default)]
pub struct OperationLocks {
    state: Mutex<LockState>,
}

impl OperationLocks {
    /// A fresh lock.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Takes a shared or exclusive hold; it is released when the guard drops.
    ///
    /// # Errors
    /// [`LockDenied`] when the hold is not available (never blocks).
    pub fn try_acquire(self: &Arc<Self>, exclusive: bool) -> Result<LockGuard, LockDenied> {
        let mut state = self.state.lock().map_err(|_| LockDenied::Conflict)?;
        if state.exclusive {
            return Err(LockDenied::Conflict);
        }
        if exclusive {
            if state.running > 0 {
                return Err(LockDenied::Conflict);
            }
            state.exclusive = true;
        } else {
            if state.running >= MAX_CONCURRENT_COMMANDS {
                return Err(LockDenied::Saturated);
            }
            state.running += 1;
        }
        Ok(LockGuard {
            locks: Arc::clone(self),
            exclusive,
        })
    }
}

/// A held lock; dropping it releases.
#[derive(Debug)]
pub struct LockGuard {
    locks: Arc<OperationLocks>,
    exclusive: bool,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        if let Ok(mut state) = self.locks.state.lock() {
            if self.exclusive {
                state.exclusive = false;
            } else {
                state.running = state.running.saturating_sub(1);
            }
        }
    }
}
