//! The guard installer and the read-only verifier.
//!
//! The migrator creates the chain tables from [`crate::TABLE_DDL`], but it only creates objects:
//! the application role, its grants and the append-only triggers are installed by
//! [`install_guard`], which the migrate command calls after the plan. [`verify`] is how a host
//! proves, before it serves, that the guard is in place. Every statement here is a constant of
//! this crate; nothing from a caller or the database is interpolated into SQL, and no message
//! carries a statement or a row value.

use deadpool_postgres::Pool;

use crate::error::{Error, PROTECTION_INCOMPLETE, Result, TABLES_MISSING, db, pool};
use crate::sql::{PREPARE_LOCK, apply_guard};

/// What [`verify`] found. Every field is a fact about the current schema; the chain is protected
/// only when [`Protection::is_complete`] is true.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Protection {
    /// `spot_audit_chain` exists.
    pub chain_table: bool,
    /// `spot_audit_checkpoint` exists.
    pub checkpoint_table: bool,
    /// The `UPDATE`/`DELETE` guard trigger is present on the chain and enabled.
    pub rewrite_trigger: bool,
    /// The `TRUNCATE` guard trigger is present on the chain and enabled.
    pub truncate_trigger: bool,
    /// The role `spot_audit_app` exists.
    pub role: bool,
    /// The role holds none of `UPDATE`, `DELETE`, `TRUNCATE` on the chain (directly, through
    /// membership or through `PUBLIC`). False when the chain or the role is missing.
    pub role_cannot_rewrite: bool,
}

impl Protection {
    /// Whether every part of the protection is in place.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.missing().is_empty()
    }

    /// The names of the parts that are missing, in a fixed order. Empty when complete.
    #[must_use]
    pub fn missing(&self) -> Vec<&'static str> {
        [
            (self.chain_table, "spot_audit_chain"),
            (self.checkpoint_table, "spot_audit_checkpoint"),
            (self.rewrite_trigger, "spot_audit_chain_no_rewrite"),
            (self.truncate_trigger, "spot_audit_chain_no_truncate"),
            (self.role, "spot_audit_app"),
            (self.role_cannot_rewrite, "spot_audit_app restrictions"),
        ]
        .into_iter()
        .filter_map(|(present, name)| (!present).then_some(name))
        .collect()
    }

    /// `Ok` when complete.
    ///
    /// # Errors
    ///
    /// [`crate::PROTECTION_INCOMPLETE`], naming the missing parts (fixed names, never data).
    pub fn require_complete(&self) -> Result<()> {
        let missing = self.missing();
        if missing.is_empty() {
            Ok(())
        } else {
            Err(Error::new(
                PROTECTION_INCOMPLETE,
                format!(
                    "the audit chain is not fully protected; missing: {}",
                    missing.join(", ")
                ),
            ))
        }
    }
}

/// Whether the table is a relation of the current schema.
const TABLE_EXISTS: &str =
    "SELECT to_regclass(quote_ident(current_schema()) || '.' || quote_ident($1)) IS NOT NULL";

/// Installs the role, the grants and the guard triggers on tables the migrator created.
/// Idempotent, and under the advisory lock `prepare` takes. Creates no table.
///
/// # Errors
///
/// [`crate::TABLES_MISSING`] when either table does not exist (nothing was changed);
/// [`crate::UNAVAILABLE`] when the database or its pool fails.
pub async fn install_guard(db_pool: &Pool) -> Result<()> {
    let mut client = db_pool.get().await.map_err(|e| pool("install guard", &e))?;
    let tx = client
        .transaction()
        .await
        .map_err(|e| db("install guard", &e))?;
    tx.batch_execute(PREPARE_LOCK)
        .await
        .map_err(|e| db("install guard lock", &e))?;
    for name in [crate::CHAIN_TABLE, crate::CHECKPOINT_TABLE] {
        let found: bool = tx
            .query_one(TABLE_EXISTS, &[&name])
            .await
            .map_err(|e| db("install guard look-up", &e))?
            .get(0);
        if !found {
            return Err(Error::new(
                TABLES_MISSING,
                format!("the table `{name}` does not exist; run the migration plan first"),
            ));
        }
    }
    apply_guard(&tx, "install guard").await?;
    tx.commit().await.map_err(|e| db("install guard", &e))
}

/// Reads, without changing anything, whether the chain is fully protected.
///
/// # Errors
///
/// [`crate::UNAVAILABLE`] when the database or its pool fails.
pub async fn verify(db_pool: &Pool) -> Result<Protection> {
    let client = db_pool.get().await.map_err(|e| pool("verify", &e))?;
    let row = client
        .query_one(VERIFY_SQL, &[])
        .await
        .map_err(|e| db("verify", &e))?;
    Ok(Protection {
        chain_table: row.get(0),
        checkpoint_table: row.get(1),
        rewrite_trigger: row.get(2),
        truncate_trigger: row.get(3),
        role: row.get(4),
        role_cannot_rewrite: row.get(5),
    })
}

/// One read-only statement. A trigger counts only when it is on the chain, runs the guard
/// function, has the right timing and events, and is enabled for ordinary sessions (`O` origin or
/// `A` always; `D` disabled and `R` replica-only do not count). `tgtype` bits: 1 row, 2 before,
/// 8 delete, 16 update, 32 truncate. The privilege checks are guarded so a missing role or table
/// reports false instead of raising (a `CASE`, because SQL does not promise `AND` short-circuits).
const VERIFY_SQL: &str = "\
WITH rel AS (\
  SELECT to_regclass(quote_ident(current_schema()) || '.spot_audit_chain') AS chain, \
         to_regclass(quote_ident(current_schema()) || '.spot_audit_checkpoint') AS checkpoint), \
trig AS (\
  SELECT t.tgname::text AS name, t.tgtype::int AS kind, t.tgenabled::text AS enabled, \
         t.tgfoid = to_regproc(quote_ident(current_schema()) || '.spot_audit_chain_guard') AS guard \
  FROM pg_trigger t, rel WHERE t.tgrelid = rel.chain AND NOT t.tgisinternal) \
SELECT \
  rel.chain IS NOT NULL, \
  rel.checkpoint IS NOT NULL, \
  EXISTS (SELECT 1 FROM trig WHERE name = 'spot_audit_chain_no_rewrite' AND guard \
          AND enabled IN ('O', 'A') AND (kind & 1) = 1 AND (kind & 2) = 2 \
          AND (kind & 8) = 8 AND (kind & 16) = 16), \
  EXISTS (SELECT 1 FROM trig WHERE name = 'spot_audit_chain_no_truncate' AND guard \
          AND enabled IN ('O', 'A') AND (kind & 1) = 0 AND (kind & 2) = 2 AND (kind & 32) = 32), \
  EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'spot_audit_app'), \
  CASE WHEN rel.chain IS NOT NULL \
        AND EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'spot_audit_app') \
       THEN NOT (has_table_privilege('spot_audit_app', rel.chain, 'UPDATE') \
                 OR has_table_privilege('spot_audit_app', rel.chain, 'DELETE') \
                 OR has_table_privilege('spot_audit_app', rel.chain, 'TRUNCATE')) \
       ELSE false END \
FROM rel";
