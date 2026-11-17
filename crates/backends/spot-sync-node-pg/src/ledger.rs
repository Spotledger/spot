//! The applied-record ledger: reads through the bridge, and the statements the apply path runs
//! inside its own transaction (so the ledger rows and the documents commit together).
//!
//! The ledger is the truth about what this node has applied. Nothing is cached ahead of it: the
//! applied journal prefix is **recomputed from the ledger** ([`PgLedger::applied_prefix`]) at
//! start and whenever it is asked for, and a checkpoint is refused when it is ahead of it.

use std::time::Duration;

use spot_sync_closure::{Position, RecordKey};

use crate::db::{Db, valid_tenant};
use crate::error::{FailureLog, PgNodeError, map, to_u64};

/// Deadline of a ledger read.
pub const LEDGER_DEADLINE: Duration = Duration::from_secs(15);

/// `put` rows: the record version applied. Param 4 is the least version asked for; zero asks
/// "is the record present now": the latest put must not be at or below a delete.
pub(crate) const HAS: &str = "SELECT EXISTS (SELECT 1 FROM spot_sync_node_applied p \
    WHERE p.tenant = $1 AND p.entity = $2 AND p.id = $3 AND p.kind = 'put' \
    AND p.version >= $4::bigint \
    AND ($4::bigint > 0 OR NOT EXISTS (SELECT 1 FROM spot_sync_node_applied d \
        WHERE d.tenant = $1 AND d.entity = $2 AND d.id = $3 AND d.kind = 'delete' \
        AND d.version >= p.version)))";
pub(crate) const HAS_TOMBSTONE: &str = "SELECT EXISTS (SELECT 1 FROM spot_sync_node_applied \
    WHERE tenant = $1 AND entity = $2 AND id = $3 AND kind = 'delete' AND version >= $4::bigint)";
pub(crate) const MAX_POSITION: &str = "SELECT max(position) FROM spot_sync_node_applied \
    WHERE tenant = $1 AND source = $2";
/// The highest applied position below the first position of every parked or repair-listed bundle
/// (the same rule as the intake's prefix tracker, with the ledger as the settled set).
pub(crate) const PREFIX: &str = "WITH floor AS (SELECT min(p) AS f FROM (\
        SELECT first_position AS p FROM spot_sync_node_parked WHERE tenant = $1 \
        UNION ALL SELECT first_position FROM spot_sync_node_repair WHERE tenant = $1) q) \
    SELECT max(a.position) FROM spot_sync_node_applied a, floor \
    WHERE a.tenant = $1 AND a.source = $2 AND (floor.f IS NULL OR a.position < floor.f)";
pub(crate) const COUNT: &str = "SELECT count(*) FROM spot_sync_node_applied WHERE tenant = $1";
/// Inside the apply transaction: one ledger row. A repeat of the same key is a no-op.
pub(crate) const INSERT: &str = "INSERT INTO spot_sync_node_applied \
    (tenant, source, entity, id, name, version, position, kind, applied_at) \
    VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) ON CONFLICT DO NOTHING";
/// Inside the apply transaction: the highest put version of a record.
pub(crate) const MAX_PUT: &str = "SELECT max(version) FROM spot_sync_node_applied \
    WHERE tenant = $1 AND entity = $2 AND id = $3 AND kind = 'put'";

/// The ledger of this node, read through the bridge.
#[derive(Clone, Debug)]
pub struct PgLedger {
    db: Db,
    failures: FailureLog,
}

impl PgLedger {
    /// A ledger reader on `db`.
    #[must_use]
    pub fn new(db: Db) -> Self {
        Self {
            db,
            failures: FailureLog::new(),
        }
    }

    pub(crate) fn with_failures(db: Db, failures: FailureLog) -> Self {
        Self { db, failures }
    }

    /// The last failure of `has` or `has_tombstone` since the previous call.
    #[must_use]
    pub fn take_failure(&self) -> Option<PgNodeError> {
        self.failures.take()
    }

    /// Whether a put of `key` at `min_version` or above was applied for `tenant`; with
    /// `min_version` zero, whether the record is present now (not deleted after its last put).
    /// A failure answers `false` (the intake then parks rather than applies) and is recorded.
    #[must_use]
    pub fn has(&self, tenant: &str, key: &RecordKey, min_version: u64) -> bool {
        self.exists(HAS, tenant, key, min_version)
    }

    /// Whether a delete of `key` at `min_version` or above was applied for `tenant`.
    #[must_use]
    pub fn has_tombstone(&self, tenant: &str, key: &RecordKey, min_version: u64) -> bool {
        self.exists(HAS_TOMBSTONE, tenant, key, min_version)
    }

    fn exists(&self, sql: &'static str, tenant: &str, key: &RecordKey, min: u64) -> bool {
        if !valid_tenant(tenant) {
            return false;
        }
        let Ok(min) = i64::try_from(min) else {
            return false;
        };
        let (tenant, entity, id) = (tenant.to_owned(), key.entity.clone(), key.id.clone());
        let result = self.db.run(LEDGER_DEADLINE, move |conn| {
            Box::pin(async move {
                let row = conn.query_one(sql, &[&tenant, &entity, &id, &min]).await?;
                Ok(row.get::<_, bool>(0))
            })
        });
        match result {
            Ok(found) => found,
            Err(error) => {
                self.failures.record(map(error));
                false
            }
        }
    }

    /// The highest position the ledger holds for `source` in `tenant`.
    ///
    /// # Errors
    /// The store's failure.
    pub fn max_position(
        &self,
        tenant: &str,
        source: &str,
    ) -> Result<Option<Position>, PgNodeError> {
        let (tenant, source) = (tenant.to_owned(), source.to_owned());
        self.db
            .run(LEDGER_DEADLINE, move |conn| {
                Box::pin(async move {
                    let row = conn.query_one(MAX_POSITION, &[&tenant, &source]).await?;
                    row.get::<_, Option<i64>>(0).map(to_u64).transpose()
                })
            })
            .map_err(map)
    }

    /// The applied journal prefix of `source` in `tenant`, recomputed from the ledger: the highest
    /// applied position below the first position of every parked and repair-listed bundle, or the
    /// highest applied position when none is waiting. `None` when nothing is applied below.
    ///
    /// # Errors
    /// The store's failure.
    pub fn applied_prefix(
        &self,
        tenant: &str,
        source: &str,
    ) -> Result<Option<Position>, PgNodeError> {
        let (tenant, source) = (tenant.to_owned(), source.to_owned());
        self.db
            .run(LEDGER_DEADLINE, move |conn| {
                Box::pin(async move {
                    let row = conn.query_one(PREFIX, &[&tenant, &source]).await?;
                    row.get::<_, Option<i64>>(0).map(to_u64).transpose()
                })
            })
            .map_err(map)
    }

    /// Rows in the ledger for `tenant`.
    ///
    /// # Errors
    /// The store's failure.
    pub fn count(&self, tenant: &str) -> Result<u64, PgNodeError> {
        let tenant = tenant.to_owned();
        self.db
            .run(LEDGER_DEADLINE, move |conn| {
                Box::pin(async move {
                    let row = conn.query_one(COUNT, &[&tenant]).await?;
                    to_u64(row.get::<_, i64>(0))
                })
            })
            .map_err(map)
    }
}
