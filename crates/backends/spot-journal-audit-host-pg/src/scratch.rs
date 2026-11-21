//! The throwaway scratch database of one audit run.
//!
//! The rebuild applies a **full copy of a tenant's data**, so the scratch lives in a database of
//! its own: `spot_audit_<tenant>_<runid>` on the same PostgreSQL server, created before the run
//! and dropped after it, owned by a dedicated role (`services.journal_audit.scratch_role`)
//! that holds `CREATEDB` and nothing else. `CONNECT` is revoked from `PUBLIC`, so no other site's
//! role can reach it.
//!
//! * **Role**: created by the installer, never by the service. [`ScratchFactory::check_role`]
//!   only verifies that it exists, can create databases and holds no more: a superuser, a role
//!   that can create roles or replicate, or one that bypasses row security is refused with
//!   `journal_audit_host.role_too_powerful`.
//! * **Every path drops it**: [`ScratchDatabase`] drops its database in `Drop` as well as in
//!   [`ScratchDatabase::drop_now`], and [`ScratchFactory::sweep`] removes what a crashed process
//!   left (the database's comment records its creation instant, so a sweep with an instant a day
//!   old removes exactly the leaks and never a run in flight).
//! * **Names** are built from validated parts only (lowercase letters, digits and `_`), so the
//!   identifier is safe to quote: a database name cannot be a bind parameter.

use std::sync::Arc;

use spot_events_contract::envelope::Position;
use spot_journal_audit::Fingerprint;
use spot_journal_audit_pg::PgScratchSide;
use spot_journal_audit_run::{ScratchSide, Summaries};
use spot_journal_rebuild::{
    AppliedAudit, AppliedDelete, AppliedImage, ApplySink, PlanSummary, SinkFuture,
};
use spot_pg_bridge::{Bridge, BridgeConfig};
use spot_postgres::{PgBackend, PgConfig};

use crate::error::{HostError, db_error};
use crate::worker::SyncPg;

/// The prefix of every scratch database.
pub const DATABASE_PREFIX: &str = "spot_audit_";
/// A leaked scratch database is swept after this long.
pub const SWEEP_AFTER_MILLIS: i64 = 24 * 60 * 60 * 1000;
const COMMENT_TAG: &str = "spot-audit";

/// Whether `part` can be one component of a scratch database name.
fn valid_part(part: &str) -> bool {
    !part.is_empty()
        && part
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        && !part.starts_with(|c: char| c.is_ascii_digit())
}

/// `spot_audit_<tenant>_<run>`, or why the parts cannot name a database.
///
/// # Errors
/// [`HostError::ScratchName`] when a part is empty, has a character outside lowercase letters,
/// digits and `_`, or the name passes PostgreSQL's 63 byte limit.
pub fn database_name(tenant: &str, run: &str) -> Result<String, HostError> {
    if !valid_part(tenant) || !valid_part(run) {
        return Err(HostError::ScratchName);
    }
    let name = format!("{DATABASE_PREFIX}{tenant}_{run}");
    if name.len() > 63 {
        return Err(HostError::ScratchName);
    }
    Ok(name)
}

/// What a scratch role may be: `CREATEDB` and nothing more.
///
/// # Errors
/// [`HostError::RoleTooPowerful`] naming the privilege, or [`HostError::RoleUnusable`] when the
/// role cannot create a database or does not exist.
pub fn judge_role(
    exists: bool,
    createdb: bool,
    superuser: bool,
    createrole: bool,
    replication: bool,
    bypassrls: bool,
) -> Result<(), HostError> {
    if !exists || !createdb {
        return Err(HostError::RoleUnusable);
    }
    if superuser {
        return Err(HostError::RoleTooPowerful("superuser"));
    }
    if createrole {
        return Err(HostError::RoleTooPowerful("createrole"));
    }
    if replication {
        return Err(HostError::RoleTooPowerful("replication"));
    }
    if bypassrls {
        return Err(HostError::RoleTooPowerful("bypassrls"));
    }
    Ok(())
}

/// Creates, drops and sweeps scratch databases through a connection as the scratch role.
pub struct ScratchFactory {
    admin: Arc<SyncPg>,
    config: PgConfig,
}

impl ScratchFactory {
    /// A factory over `admin`, a worker connected **as the scratch role** to a maintenance
    /// database of the same server, and `config`, the scratch role's own connection settings
    /// (the database name is replaced per run). The password stays in `config` and never reaches
    /// a log, an error or a result.
    #[must_use]
    pub fn new(admin: Arc<SyncPg>, config: PgConfig) -> Self {
        Self { admin, config }
    }

    /// A bridge over a fresh pool on the scratch database `name`, as the scratch role. The pool
    /// connects inside the admin worker's runtime, which is driven for as long as the factory
    /// lives, so the bridge's calls keep working.
    ///
    /// # Errors
    /// [`HostError::Scratch`] when the connection or the bridge cannot be made.
    pub fn open_bridge(&self, name: &str) -> Result<Arc<Bridge>, HostError> {
        let mut config = self.config.clone().with_dbname(name);
        config.pool_max_size = config.pool_max_size.max(8);
        let backend = self.admin.run(move |_pool| async move {
            PgBackend::connect(config)
                .await
                .map_err(|_| HostError::Scratch("connected"))
        })?;
        Bridge::for_backend(BridgeConfig::default(), &backend)
            .map(Arc::new)
            .map_err(|_| HostError::Scratch("bridged"))
    }

    /// Verifies the role the factory runs as.
    ///
    /// # Errors
    /// See [`judge_role`]; [`HostError::Db`] when the catalogue cannot be read.
    pub fn check_role(&self) -> Result<(), HostError> {
        let flags = self.admin.run(|pool| async move {
            let client = pool.get().await.map_err(|_| HostError::Worker)?;
            let rows = client
                .query(
                    "SELECT rolcreatedb, rolsuper, rolcreaterole, rolreplication, rolbypassrls \
                     FROM pg_roles WHERE rolname = current_user",
                    &[],
                )
                .await
                .map_err(|e| db_error(&e))?;
            match rows.first() {
                None => Ok(None),
                Some(row) => Ok(Some([
                    row.try_get::<_, bool>(0).map_err(|e| db_error(&e))?,
                    row.try_get::<_, bool>(1).map_err(|e| db_error(&e))?,
                    row.try_get::<_, bool>(2).map_err(|e| db_error(&e))?,
                    row.try_get::<_, bool>(3).map_err(|e| db_error(&e))?,
                    row.try_get::<_, bool>(4).map_err(|e| db_error(&e))?,
                ])),
            }
        })?;
        match flags {
            None => judge_role(false, false, false, false, false, false),
            Some([createdb, superuser, createrole, replication, bypassrls]) => judge_role(
                true,
                createdb,
                superuser,
                createrole,
                replication,
                bypassrls,
            ),
        }
    }

    /// Creates the scratch database of `tenant` for `run`, secured and stamped with
    /// `created_millis` for the sweep.
    ///
    /// # Errors
    /// [`HostError::ScratchName`], or [`HostError::Scratch`] when the database could not be
    /// created or secured (a half-made database is dropped before the error returns).
    pub fn create(
        self: &Arc<Self>,
        tenant: &str,
        run: &str,
        created_millis: i64,
    ) -> Result<ScratchDatabase, HostError> {
        let name = database_name(tenant, run)?;
        let sql_create = format!("CREATE DATABASE \"{name}\"");
        let sql_revoke = format!("REVOKE CONNECT ON DATABASE \"{name}\" FROM PUBLIC");
        let sql_comment =
            format!("COMMENT ON DATABASE \"{name}\" IS '{COMMENT_TAG}:{tenant}:{created_millis}'");
        let created = self.admin.run(move |pool| async move {
            let client = pool.get().await.map_err(|_| HostError::Worker)?;
            client
                .batch_execute(&sql_create)
                .await
                .map_err(|_| HostError::Scratch("created"))?;
            let secured = async {
                client.batch_execute(&sql_revoke).await?;
                client.batch_execute(&sql_comment).await
            }
            .await;
            if secured.is_err() {
                // Never leave a copy-capable database behind that is not secured.
                let _ = client
                    .batch_execute(&format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"))
                    .await;
                return Err(HostError::Scratch("secured"));
            }
            Ok(())
        });
        created?;
        Ok(ScratchDatabase {
            factory: Arc::clone(self),
            name: database_name(tenant, run)?,
            dropped: false,
        })
    }

    fn drop_database(&self, name: &str) -> Result<(), HostError> {
        let sql = format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)");
        self.admin.run(move |pool| async move {
            let client = pool.get().await.map_err(|_| HostError::Worker)?;
            client
                .batch_execute(&sql)
                .await
                .map_err(|_| HostError::Scratch("dropped"))
        })
    }

    /// Whether the server has a database called `name` (a question for tests and operators).
    ///
    /// # Errors
    /// [`HostError::Db`] when the catalogue cannot be read.
    pub fn exists(&self, name: &str) -> Result<bool, HostError> {
        let name = name.to_owned();
        self.admin.run(move |pool| async move {
            let client = pool.get().await.map_err(|_| HostError::Worker)?;
            let rows = client
                .query("SELECT 1 FROM pg_database WHERE datname = $1", &[&name])
                .await
                .map_err(|e| db_error(&e))?;
            Ok(!rows.is_empty())
        })
    }

    /// Drops the scratch databases of this tenant that were created before `older_than_millis`
    /// (a crashed run's leak) and returns their names. Only databases whose comment carries this
    /// tenant and a creation instant are touched; another tenant's, and a run in flight, are not.
    ///
    /// # Errors
    /// [`HostError::Db`] when the leftovers cannot be listed, [`HostError::Scratch`] when one
    /// cannot be dropped.
    pub fn sweep(&self, tenant: &str, older_than_millis: i64) -> Result<Vec<String>, HostError> {
        if !valid_part(tenant) {
            return Err(HostError::ScratchName);
        }
        let tenant = tenant.to_owned();
        let leaked = self.admin.run(move |pool| async move {
            let client = pool.get().await.map_err(|_| HostError::Worker)?;
            let like = format!("{DATABASE_PREFIX}{tenant}\\_%");
            let rows = client
                .query(
                    "SELECT datname::text, coalesce(shobj_description(oid, 'pg_database'), '') \
                     FROM pg_database WHERE datname LIKE $1 ORDER BY datname",
                    &[&like],
                )
                .await
                .map_err(|e| db_error(&e))?;
            let mut out = Vec::new();
            for row in &rows {
                let name: String = row.try_get(0).map_err(|e| db_error(&e))?;
                let comment: String = row.try_get(1).map_err(|e| db_error(&e))?;
                let mut parts = comment.splitn(3, ':');
                let (Some(tag), Some(owner), Some(at)) = (parts.next(), parts.next(), parts.next())
                else {
                    continue;
                };
                if tag == COMMENT_TAG
                    && owner == tenant
                    && at.parse::<i64>().is_ok_and(|at| at < older_than_millis)
                {
                    out.push(name);
                }
            }
            Ok(out)
        })?;
        for name in &leaked {
            self.drop_database(name)?;
        }
        Ok(leaked)
    }
}

/// One scratch database; dropped on every path.
pub struct ScratchDatabase {
    factory: Arc<ScratchFactory>,
    name: String,
    dropped: bool,
}

impl ScratchDatabase {
    /// The database's name (`spot_audit_<tenant>_<run>`).
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Drops the database now.
    ///
    /// # Errors
    /// [`HostError::Scratch`] when it cannot be dropped (the sweep removes it later).
    pub fn drop_now(&mut self) -> Result<(), HostError> {
        if self.dropped {
            return Ok(());
        }
        self.dropped = true;
        self.factory.drop_database(&self.name)
    }
}

impl Drop for ScratchDatabase {
    fn drop(&mut self) {
        let _ = self.drop_now();
    }
}

/// A [`PgScratchSide`] that owns its database: discarding the side drops the schema and then the
/// database, and dropping the side without discarding still drops both (the side's own guard
/// first, then this struct's field order drops the database last).
pub struct ScratchWithDatabase {
    // Field order is the drop order: the side (and its session) before the database.
    side: PgScratchSide,
    database: ScratchDatabase,
}

impl ScratchWithDatabase {
    /// Wraps `side`, which lives in `database`.
    #[must_use]
    pub fn new(side: PgScratchSide, database: ScratchDatabase) -> Self {
        Self { side, database }
    }

    /// The scratch database's name.
    #[must_use]
    pub fn database(&self) -> &str {
        self.database.name()
    }
}

impl ApplySink for ScratchWithDatabase {
    fn begin<'a>(&'a mut self, summary: &'a PlanSummary) -> SinkFuture<'a> {
        self.side.begin(summary)
    }
    fn apply_image(&mut self, image: AppliedImage) -> SinkFuture<'_> {
        self.side.apply_image(image)
    }
    fn apply_delete(&mut self, delete: AppliedDelete) -> SinkFuture<'_> {
        self.side.apply_delete(delete)
    }
    fn apply_audit(&mut self, audit: AppliedAudit) -> SinkFuture<'_> {
        self.side.apply_audit(audit)
    }
    fn commit(&mut self, last_position: Position) -> SinkFuture<'_> {
        self.side.commit(last_position)
    }
    fn abort<'a>(&'a mut self, reason: &'a str) -> SinkFuture<'a> {
        self.side.abort(reason)
    }
}

impl ScratchSide for ScratchWithDatabase {
    fn tenant(&self) -> &str {
        ScratchSide::tenant(&self.side)
    }
    fn summaries(&self) -> Summaries<'_> {
        ScratchSide::summaries(&self.side)
    }
    fn fingerprints(&self) -> Vec<Fingerprint> {
        ScratchSide::fingerprints(&self.side)
    }
    fn discard(&mut self) {
        ScratchSide::discard(&mut self.side);
        // The side's session is closed; now the whole database goes.
        let _ = self.database.drop_now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_safe_to_quote() {
        assert_eq!(
            database_name("acme", "run7").unwrap_or_default(),
            "spot_audit_acme_run7"
        );
        for bad in [
            ("", "r"),
            ("acme", ""),
            ("Acme", "r"),
            ("a\"; DROP DATABASE x; --", "r"),
            ("a b", "r"),
            ("acme", "1run"),
            ("acme-1", "r"),
        ] {
            assert_eq!(
                database_name(bad.0, bad.1),
                Err(HostError::ScratchName),
                "{bad:?}"
            );
        }
        let long = "t".repeat(60);
        assert_eq!(database_name(&long, "run"), Err(HostError::ScratchName));
    }

    #[test]
    fn the_role_may_create_databases_and_do_nothing_else() {
        assert_eq!(judge_role(true, true, false, false, false, false), Ok(()));
        assert_eq!(
            judge_role(false, false, false, false, false, false),
            Err(HostError::RoleUnusable)
        );
        assert_eq!(
            judge_role(true, false, false, false, false, false),
            Err(HostError::RoleUnusable)
        );
        assert_eq!(
            judge_role(true, true, true, false, false, false),
            Err(HostError::RoleTooPowerful("superuser"))
        );
        assert_eq!(
            judge_role(true, true, false, true, false, false),
            Err(HostError::RoleTooPowerful("createrole"))
        );
        assert_eq!(
            judge_role(true, true, false, false, true, false),
            Err(HostError::RoleTooPowerful("replication"))
        );
        assert_eq!(
            judge_role(true, true, false, false, false, true),
            Err(HostError::RoleTooPowerful("bypassrls"))
        );
    }
}
