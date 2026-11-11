//! The snapshot seam: the taken snapshot the engine restores from.
//!
//! ADR-22 restores the whole site from a snapshot when a run fails after apply, before users
//! return. The engine never talks to `spot-snapshot` itself: it requires a [`SnapshotId`] that
//! was taken and restores through [`SnapshotPort`]. The concrete implementation (T2) wraps
//! `spot_snapshot::SnapshotProvider`, takes the snapshot before the run and maps its manifest to
//! a [`SnapshotId`]; the in-memory [`RecordingSnapshotPort`] is the reference and the `testing`
//! feature's site-aware fake drives the end-state tests.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::BoxFuture;
use crate::error::{Error, Result};

/// The id of a taken snapshot.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SnapshotId(String);

impl SnapshotId {
    /// Validates and wraps a snapshot id: non-empty and printable.
    ///
    /// # Errors
    /// [`Error::InvalidInput`] when the id is empty or carries a control character.
    pub fn new(id: impl Into<String>) -> Result<Self> {
        let id = id.into();
        if id.is_empty() {
            return Err(Error::InvalidInput {
                what: "snapshot id".to_owned(),
                reason: "must not be empty".to_owned(),
            });
        }
        if id.chars().any(char::is_control) {
            return Err(Error::InvalidInput {
                what: "snapshot id".to_owned(),
                reason: "must be printable".to_owned(),
            });
        }
        Ok(Self(id))
    }

    /// The id as a string.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SnapshotId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// What a restore did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreOutcome {
    /// The snapshot that was restored.
    pub snapshot: SnapshotId,
    /// Where the data landed (a database name or a data-directory path; never a secret).
    pub location: String,
    /// Unix microseconds the restore finished at.
    pub at: i64,
}

/// The snapshot the engine restores from.
pub trait SnapshotPort: Send + Sync {
    /// Takes a snapshot labelled `label` and returns its id.
    ///
    /// # Errors
    /// A backend error when the snapshot cannot be taken.
    fn take<'a>(&'a self, label: &'a str, now: i64) -> BoxFuture<'a, Result<SnapshotId>>;

    /// Whether the snapshot `id` was taken and is complete.
    ///
    /// # Errors
    /// A backend error when the snapshot cannot be checked.
    fn exists<'a>(&'a self, id: &'a SnapshotId) -> BoxFuture<'a, Result<bool>>;

    /// Restores the site from `id`.
    ///
    /// # Errors
    /// A backend error when the restore fails.
    fn restore<'a>(&'a self, id: &'a SnapshotId, now: i64)
    -> BoxFuture<'a, Result<RestoreOutcome>>;
}

/// A snapshot port that records the ids it took and counts the restores, without moving data.
///
/// It is the reference implementation and the double a host uses before the real provider is
/// wired. It never mutates a site: a host that needs the site reset uses the `testing` feature's
/// site-aware fake or the real `spot-snapshot`-backed port.
#[derive(Debug, Default)]
pub struct RecordingSnapshotPort {
    taken: Mutex<BTreeMap<String, String>>,
    restores: AtomicU64,
    next: AtomicU64,
}

impl RecordingSnapshotPort {
    /// An empty port.
    #[must_use]
    pub fn new() -> Self {
        Self {
            taken: Mutex::new(BTreeMap::new()),
            restores: AtomicU64::new(0),
            next: AtomicU64::new(1),
        }
    }

    /// How many restores were asked for.
    #[must_use]
    pub fn restores(&self) -> u64 {
        self.restores.load(Ordering::SeqCst)
    }

    /// The ids taken so far, in order.
    #[must_use]
    pub fn taken(&self) -> Vec<String> {
        self.taken
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .keys()
            .cloned()
            .collect()
    }
}

impl SnapshotPort for RecordingSnapshotPort {
    fn take<'a>(&'a self, label: &'a str, _now: i64) -> BoxFuture<'a, Result<SnapshotId>> {
        Box::pin(async move {
            let n = self.next.fetch_add(1, Ordering::SeqCst);
            let id = format!("snap-{n}");
            self.taken
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(id.clone(), label.to_owned());
            SnapshotId::new(id)
        })
    }

    fn exists<'a>(&'a self, id: &'a SnapshotId) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            Ok(self
                .taken
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .contains_key(id.as_str()))
        })
    }

    fn restore<'a>(
        &'a self,
        id: &'a SnapshotId,
        now: i64,
    ) -> BoxFuture<'a, Result<RestoreOutcome>> {
        Box::pin(async move {
            let known = self
                .taken
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(id.as_str())
                .cloned();
            let Some(label) = known else {
                return Err(Error::SnapshotFailed {
                    message: format!("snapshot {id} was not taken"),
                });
            };
            self.restores.fetch_add(1, Ordering::SeqCst);
            Ok(RestoreOutcome {
                snapshot: id.clone(),
                location: label,
                at: now,
            })
        })
    }
}
