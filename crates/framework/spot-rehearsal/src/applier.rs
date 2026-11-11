//! Applying the plan to the clone.
//!
//! The engine never applies anything itself: it hands the plan and the clone to an [`Applier`].
//! The real one is [`PgMigrateApplier`], which re-plans against the clone's live schema and runs
//! `spot-pg-migrate::apply` — the same engine the live apply step uses, so a rehearsal exercises
//! the real migration. Tests use a fake that mutates an in-memory fingerprint source.

use spot_migration_plan::ReleasePlan;
use spot_pg_migrate::{ApplyOptions, LiveSchema};
use spot_postgres::{PgBackend, PgConfig};
use spot_snapshot::RestoreTarget;
use spot_storage_contract::Catalog;

use serde::{Deserialize, Serialize};

use crate::BoxFuture;
use crate::clone::CloneTarget;
use crate::error::{Error, Result};

/// What an [`Applier`] did.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplyOutcome {
    /// The keys of the migration steps that ran, in order. Empty when the clone was already
    /// current.
    pub executed: Vec<String>,
    /// The hash of the schema plan that was executed (after the applier re-planned under its
    /// lock).
    pub plan_hash: String,
    /// The digest of the release plan the rehearsal was rehearsing, for provenance.
    pub plan_digest: String,
    /// Rows filled by backfill steps.
    pub backfilled_rows: u64,
}

/// Applies a plan to a clone.
pub trait Applier: Send + Sync {
    /// Applies `plan` to `clone`. The clone is the only thing written; the live site is never
    /// touched.
    fn apply<'a>(
        &'a self,
        plan: &'a ReleasePlan,
        clone: &'a CloneTarget,
    ) -> BoxFuture<'a, Result<ApplyOutcome>>;
}

/// The real applier: `spot-pg-migrate` against the clone's scratch database.
///
/// It is constructed with the catalogue the plan was computed for (`wanted`); it reads the
/// clone's live schema, re-plans against it and applies, so it runs exactly the steps the live
/// apply step would.
pub struct PgMigrateApplier {
    wanted: Catalog,
    options: ApplyOptions,
}

impl PgMigrateApplier {
    /// An applier targeting `wanted` with the default apply options.
    pub fn new(wanted: Catalog) -> PgMigrateApplier {
        PgMigrateApplier {
            wanted,
            options: ApplyOptions::default(),
        }
    }

    /// Overrides the apply options (timeouts, retries, batch size).
    #[must_use]
    pub fn with_options(mut self, options: ApplyOptions) -> PgMigrateApplier {
        self.options = options;
        self
    }
}

impl Applier for PgMigrateApplier {
    fn apply<'a>(
        &'a self,
        plan: &'a ReleasePlan,
        clone: &'a CloneTarget,
    ) -> BoxFuture<'a, Result<ApplyOutcome>> {
        Box::pin(async move {
            let config = self.config_for(clone)?;
            let backend = PgBackend::connect(config)
                .await
                .map_err(|error| Error::ApplyFailed {
                    reason: error.to_string(),
                })?;
            let live = LiveSchema::read(&backend)
                .await
                .map_err(|error| Error::ApplyFailed {
                    reason: error.to_string(),
                })?;
            let migration = spot_pg_migrate::plan(&live, &self.wanted);
            let report = spot_pg_migrate::apply(&backend, &migration, self.options.clone())
                .await
                .map_err(|error| Error::ApplyFailed {
                    reason: error.to_string(),
                })?;
            Ok(ApplyOutcome {
                executed: report.executed,
                plan_hash: report.plan_hash,
                plan_digest: plan.plan_digest.clone(),
                backfilled_rows: report.backfilled_rows,
            })
        })
    }
}

impl PgMigrateApplier {
    /// The PostgreSQL config for the clone.
    fn config_for(&self, clone: &CloneTarget) -> Result<PgConfig> {
        match clone.restore() {
            RestoreTarget::Database {
                connection,
                database,
            } => PgConfig::from_url(connection)
                .map(|config| config.with_dbname(database))
                .map_err(|error| Error::ApplyFailed {
                    reason: error.to_string(),
                }),
            RestoreTarget::Instance { .. } => Err(Error::ApplyFailed {
                reason: "the PostgreSQL applier needs a database clone, not a stopped instance"
                    .to_owned(),
            }),
        }
    }
}
