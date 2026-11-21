//! The settings value store with history (WP-ADM-07.T2).
//!
//! [`SettingsBackend`] is the async seam the settings management service (`spot-cli`,
//! `services/settings_api.rs`) reads and writes through. [`PgSettingsStore`] keeps the values
//! and an append-only history in PostgreSQL; [`MemorySettingsStore`] is the in-memory twin the
//! service tests use.
//!
//! * A value is addressed by its scope (`ScopeRef::canonical`) and key. Every row carries a
//!   **version**; a write may name the version it expects (`0` means "absent"), and a mismatch is
//!   `settings_store.version_conflict` with nothing written: two racing writers cannot both win.
//! * **Secrets are references.** A secret setting holds the *name* of an entry in the secret
//!   store, never a value: the store keeps and returns that name only.
//! * **History** records who, when, through which channel, the version and the SHA-256 digests of
//!   the old and the new value. It never records a value, so reading it cannot disclose one.
//! * The store reads no clock: time is an argument. Errors carry a stable code and, for a driver
//!   error, only its SQLSTATE.

#![warn(missing_docs)]
#![forbid(unsafe_code)]

mod digest;
mod memory;
mod pg;

use std::future::Future;
use std::pin::Pin;

use spot_settings::{ScopeRef, StoredValue};

pub use memory::MemorySettingsStore;
pub use pg::PgSettingsStore;

/// A boxed future, so the trait is object safe.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A write lost a version race.
pub const VERSION_CONFLICT: &str = "settings_store.version_conflict";
/// A statement or a connection failed.
pub const STORAGE_FAILED: &str = "settings_store.storage_failed";
/// A stored row cannot be read back.
pub const CORRUPT_ROW: &str = "settings_store.corrupt_row";

/// A store failure: a stable code and a short text that holds no value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoreError {
    /// The stable code.
    pub code: &'static str,
    /// What failed, without any value.
    pub message: String,
}

impl StoreError {
    pub(crate) fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for StoreError {}

/// A stored value and its version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stored {
    /// The value; a secret holds the **reference name**.
    pub value: StoredValue,
    /// The version, at least 1.
    pub version: u64,
}

/// One write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetRequest {
    /// The scope.
    pub scope: ScopeRef,
    /// The setting key.
    pub key: String,
    /// The new value, or `None` to remove the value at this scope.
    pub value: Option<StoredValue>,
    /// The version the caller read (`0` for absent); `None` writes unconditionally.
    pub expected: Option<u64>,
    /// The principal that made the change.
    pub actor: String,
    /// The channel it came through (for example `mgmt`).
    pub channel: String,
    /// When, in Unix seconds (an argument: the store reads no clock).
    pub at: i64,
}

/// One line of the history. There is no field for a value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryRow {
    /// The version the write produced.
    pub version: u64,
    /// Who.
    pub actor: String,
    /// The channel.
    pub channel: String,
    /// When, in Unix seconds.
    pub at: i64,
    /// SHA-256 (hex) of the old value, `None` when there was none.
    pub old_digest: Option<String>,
    /// SHA-256 (hex) of the new value, `None` when removed.
    pub new_digest: Option<String>,
}

/// The settings value store.
pub trait SettingsBackend: Send + Sync {
    /// The value at `scope` for `key`.
    fn get<'a>(
        &'a self,
        scope: &'a ScopeRef,
        key: &'a str,
    ) -> BoxFuture<'a, Result<Option<Stored>, StoreError>>;

    /// Every value at `scope`, by key.
    fn list<'a>(
        &'a self,
        scope: &'a ScopeRef,
    ) -> BoxFuture<'a, Result<Vec<(String, Stored)>, StoreError>>;

    /// Applies one write atomically with its history row; returns the new version.
    fn set<'a>(&'a self, request: &'a SetRequest) -> BoxFuture<'a, Result<u64, StoreError>>;

    /// The history of `key` at `scope`, newest first, at most `limit`.
    fn history<'a>(
        &'a self,
        scope: &'a ScopeRef,
        key: &'a str,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<HistoryRow>, StoreError>>;
}
