//! The guard installer and the read-only verifier.
//!
//! The migrator creates the chain tables from [`crate::TABLE_DDL`], but it only creates objects:
//! the application role, its grants and the append-only triggers are installed by
//! [`install_guard`], which the migrate command calls after the plan. [`verify`] is how a host
//! proves, before it serves, that the guard is in place. Every statement here is a constant of
//! this crate; nothing from a caller or the database is interpolated into SQL, and no message
//! carries a statement or a row value.

use deadpool_postgres::Pool;

use crate::appender_sql::APPENDER_COLUMNS;
use crate::appender_verify::verify_appender;
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
    /// The four appender tables (`spot_audit_bookmark`, `spot_audit_event_index`,
    /// `spot_audit_doc_index`, `spot_audit_site_checkpoint`) exist with the expected columns.
    pub appender_tables: bool,
    /// The role holds exactly the appender privileges (see the grant table in `sql.rs`): `SELECT,
    /// INSERT, UPDATE` on the bookmark and site checkpoint tables, `SELECT, INSERT` on the two
    /// index tables, no `DELETE` or `TRUNCATE` anywhere, and `PUBLIC` holds nothing on any of the
    /// six tables. False when a table or the role is missing.
    pub appender_grants: bool,
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
            (self.appender_tables, "spot_audit_appender_tables"),
            (self.appender_grants, "spot_audit_appender_grants"),
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
    // A host that runs the audit chain always runs the appender (the audit participant installs
    // both), so all six tables must exist. Nothing is changed when any is missing.
    let mut missing: Vec<&str> = Vec::new();
    for name in [crate::CHAIN_TABLE, crate::CHECKPOINT_TABLE]
        .into_iter()
        .chain(APPENDER_COLUMNS.iter().map(|(table, _)| *table))
    {
        let found: bool = tx
            .query_one(TABLE_EXISTS, &[&name])
            .await
            .map_err(|e| db("install guard look-up", &e))?
            .get(0);
        if !found {
            missing.push(name);
        }
    }
    if !missing.is_empty() {
        return Err(Error::new(
            TABLES_MISSING,
            format!(
                "the table(s) {} do not exist; run the migration plan first",
                missing
                    .iter()
                    .map(|name| format!("`{name}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ));
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
    drop(client);
    let appender_tables = verify_appender(db_pool).await?.is_complete();
    let client = db_pool.get().await.map_err(|e| pool("verify", &e))?;
    let appender_grants: bool = client
        .query_one(VERIFY_APPENDER_GRANTS_SQL, &[])
        .await
        .map_err(|e| db("verify", &e))?
        .get(0);
    Ok(Protection {
        appender_tables,
        appender_grants,
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

/// The appender grants, read-only: true only when every table and the role exist and the role
/// holds exactly what the grant table in `sql.rs` says (checked with `has_table_privilege`, which
/// includes membership and `PUBLIC`), and `PUBLIC` itself holds no privilege on any of the six
/// tables (`aclexplode` reports `PUBLIC` as grantee 0). A `CASE` guards the privilege checks, so a
/// missing table or role reports false instead of raising.
const VERIFY_APPENDER_GRANTS_SQL: &str = "\
WITH t AS (\
  SELECT to_regclass(quote_ident(current_schema()) || '.spot_audit_chain') AS chain, \
         to_regclass(quote_ident(current_schema()) || '.spot_audit_checkpoint') AS cp, \
         to_regclass(quote_ident(current_schema()) || '.spot_audit_bookmark') AS bm, \
         to_regclass(quote_ident(current_schema()) || '.spot_audit_event_index') AS ev, \
         to_regclass(quote_ident(current_schema()) || '.spot_audit_doc_index') AS doc, \
         to_regclass(quote_ident(current_schema()) || '.spot_audit_site_checkpoint') AS sc) \
SELECT CASE WHEN EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'spot_audit_app') \
             AND t.chain IS NOT NULL AND t.cp IS NOT NULL AND t.bm IS NOT NULL \
             AND t.ev IS NOT NULL AND t.doc IS NOT NULL AND t.sc IS NOT NULL \
       THEN \
         has_table_privilege('spot_audit_app', t.bm, 'SELECT') \
         AND has_table_privilege('spot_audit_app', t.bm, 'INSERT') \
         AND has_table_privilege('spot_audit_app', t.bm, 'UPDATE') \
         AND NOT has_table_privilege('spot_audit_app', t.bm, 'DELETE') \
         AND NOT has_table_privilege('spot_audit_app', t.bm, 'TRUNCATE') \
         AND has_table_privilege('spot_audit_app', t.sc, 'SELECT') \
         AND has_table_privilege('spot_audit_app', t.sc, 'INSERT') \
         AND has_table_privilege('spot_audit_app', t.sc, 'UPDATE') \
         AND NOT has_table_privilege('spot_audit_app', t.sc, 'DELETE') \
         AND NOT has_table_privilege('spot_audit_app', t.sc, 'TRUNCATE') \
         AND has_table_privilege('spot_audit_app', t.ev, 'SELECT') \
         AND has_table_privilege('spot_audit_app', t.ev, 'INSERT') \
         AND NOT has_table_privilege('spot_audit_app', t.ev, 'UPDATE') \
         AND NOT has_table_privilege('spot_audit_app', t.ev, 'DELETE') \
         AND NOT has_table_privilege('spot_audit_app', t.ev, 'TRUNCATE') \
         AND has_table_privilege('spot_audit_app', t.doc, 'SELECT') \
         AND has_table_privilege('spot_audit_app', t.doc, 'INSERT') \
         AND NOT has_table_privilege('spot_audit_app', t.doc, 'UPDATE') \
         AND NOT has_table_privilege('spot_audit_app', t.doc, 'DELETE') \
         AND NOT has_table_privilege('spot_audit_app', t.doc, 'TRUNCATE') \
         AND NOT EXISTS (\
           SELECT 1 FROM pg_class c \
           CROSS JOIN LATERAL aclexplode(COALESCE(c.relacl, acldefault('r', c.relowner))) a \
           WHERE c.oid = ANY (ARRAY[t.chain::oid, t.cp::oid, t.bm::oid, t.ev::oid, t.doc::oid, t.sc::oid]) \
             AND a.grantee = 0) \
       ELSE false END \
FROM t";
