//! [`PgMigrationStore`]: the handle the three stores share, and the tamper alert.

use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use spot_migration_ops::{LifecycleError, PlanHash};
use spot_pg_bridge::Bridge;

use crate::schema::{self, PROTECT};

/// The deadline of a single-row read.
pub(crate) const READ_DEADLINE: Duration = Duration::from_secs(5);
/// The deadline of a write.
pub(crate) const WRITE_DEADLINE: Duration = Duration::from_secs(10);
/// The deadline of a listing.
pub(crate) const LIST_DEADLINE: Duration = Duration::from_secs(20);
/// The deadline of the protection step.
const PROTECT_DEADLINE: Duration = Duration::from_secs(30);
/// The advisory lock that serialises protection runs (transaction-scoped).
const PROTECT_LOCK: i64 = 0x004d_4947_4f50_5301;

/// Told when a stored plan no longer matches its hash. The host bridges it to the audit chain;
/// the load itself already fails closed with `migration.plan_changed`.
pub trait TamperAlert: Send + Sync {
    /// The plan stored under `hash` failed its digest check.
    fn plan_tampered(&self, hash: &PlanHash);
}

/// A [`TamperAlert`] that remembers what it was told (tests, and a staging buffer).
#[derive(Debug, Default)]
pub struct RecordingAlert {
    seen: Mutex<Vec<String>>,
}

impl RecordingAlert {
    /// An empty recorder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The hashes alerted so far.
    #[must_use]
    pub fn hashes(&self) -> Vec<String> {
        self.seen.lock().map(|g| g.clone()).unwrap_or_default()
    }
}

impl TamperAlert for RecordingAlert {
    fn plan_tampered(&self, hash: &PlanHash) {
        if let Ok(mut seen) = self.seen.lock() {
            seen.push(hash.as_str().to_owned());
        }
    }
}

/// The plan store, approval store and rehearsal ledger over PostgreSQL. Use it from a
/// control-plane process or a CLI command, never from a request handler.
#[derive(Clone)]
pub struct PgMigrationStore {
    pub(crate) bridge: Arc<Bridge>,
    pub(crate) alert: Arc<dyn TamperAlert>,
}

impl PgMigrationStore {
    /// A store that reaches the database through `bridge`, raising `alert` on tamper evidence.
    #[must_use]
    pub fn new(bridge: Arc<Bridge>, alert: Arc<dyn TamperAlert>) -> Self {
        Self { bridge, alert }
    }

    /// Checks the tables exist with the expected columns (created by `spot migrate`).
    ///
    /// # Errors
    /// `migration.store_failed`.
    pub fn ensure_present(&self) -> Result<(), LifecycleError> {
        schema::ensure_present(&self.bridge)
    }

    /// Installs the application role, its grants and the guard triggers. Idempotent and safe
    /// from several processes at once; the tables must exist.
    ///
    /// # Errors
    /// `migration.store_failed`.
    pub fn protect(&self) -> Result<(), LifecycleError> {
        self.bridge
            .run(PROTECT_DEADLINE, |mut conn| {
                Box::pin(async move {
                    let tx = conn.transaction().await?;
                    tx.execute("SELECT pg_advisory_xact_lock($1)", &[&PROTECT_LOCK])
                        .await?;
                    tx.batch_execute(PROTECT).await?;
                    tx.commit().await
                })
            })
            .map_err(schema::failed)
    }
}

impl fmt::Debug for PgMigrationStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgMigrationStore").finish_non_exhaustive()
    }
}
