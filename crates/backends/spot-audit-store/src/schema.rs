//! The upgrade from the pre-prefix table names.
//!
//! Every table of this crate starts with `spot_`, the prefix `spot_pg_migrate` accepts as the
//! platform's own. Databases created before the prefix hold `audit_chain` and `audit_checkpoint`;
//! [`rename_old_tables`] renames them, with their constraint, index and trigger names, inside the
//! transaction [`crate::sql::prepare`] already holds under its advisory lock. Rows are never
//! copied, rewritten or dropped: `ALTER TABLE ... RENAME` keeps the very same heap, so the hash
//! chain (which covers record bytes, not relation names) verifies exactly as before. The rename
//! takes an `ACCESS EXCLUSIVE` lock held to commit, so no append can land between the old and the
//! new name. A database holding both names of a pair, or an old-named table with other columns,
//! is refused before anything is renamed.
//!
//! All names here are constants of this crate; nothing from the database or a caller is
//! interpolated into SQL, and error messages carry table names only, never row data.

use tokio_postgres::Transaction;

use crate::error::{Error, Result, db};

/// Each table this crate used to create unprefixed: the old name, the new name, and the columns
/// the old table must have (sorted) to be recognised as this crate's own.
pub(crate) const RENAMES: [(&str, &str, &[&str]); 2] = [
    (
        "audit_chain",
        "spot_audit_chain",
        &["at", "hash", "prev_hash", "record", "seq", "site"],
    ),
    (
        "audit_checkpoint",
        "spot_audit_checkpoint",
        &["at", "head_hash", "key_id", "seq", "signature", "site"],
    ),
];

/// Whether `name` is a relation of the current schema (the schema `CREATE TABLE` writes to).
async fn exists(tx: &Transaction<'_>, name: &str) -> Result<bool> {
    let row = tx
        .query_one(
            "SELECT to_regclass(quote_ident(current_schema()) || '.' || quote_ident($1)) IS NOT NULL",
            &[&name],
        )
        .await
        .map_err(|e| db("prepare look-up", &e))?;
    Ok(row.get(0))
}

/// Renames the pre-prefix tables and the names derived from them, all or nothing: the caller's
/// transaction commits or rolls back every rename together with the DDL after it.
///
/// # Errors
///
/// [`crate::UNAVAILABLE`] when an old and a new name of one pair both exist, when an old-named
/// table does not have this crate's columns, or when the database fails.
pub(crate) async fn rename_old_tables(tx: &Transaction<'_>) -> Result<()> {
    // Check every pair first, so a refusal happens before the first rename.
    let mut to_rename = Vec::new();
    for (old, new, columns) in RENAMES {
        if !exists(tx, old).await? {
            continue;
        }
        if exists(tx, new).await? {
            return Err(Error::unavailable(format!(
                "both `{old}` and `{new}` exist; nothing was renamed or dropped, resolve it by hand"
            )));
        }
        let found: Vec<String> = tx
            .query(
                "SELECT column_name::text FROM information_schema.columns \
                 WHERE table_schema = current_schema() AND table_name = $1 \
                 ORDER BY column_name",
                &[&old],
            )
            .await
            .map_err(|e| db("prepare columns", &e))?
            .iter()
            .map(|r| r.get(0))
            .collect();
        if found.iter().map(String::as_str).ne(columns.iter().copied()) {
            return Err(Error::unavailable(format!(
                "the table `{old}` does not have the columns of this crate's table, so it was not renamed to `{new}`"
            )));
        }
        to_rename.push((old, new));
    }
    for (old, new) in to_rename {
        tx.batch_execute(&format!(
            "ALTER TABLE {} RENAME TO {}",
            quote(old),
            quote(new)
        ))
        .await
        .map_err(|e| db("prepare rename", &e))?;
        // Constraint names (the primary key, the PostgreSQL 18 NOT NULL constraints, and the
        // primary key's index with them) carry the old table name as a prefix.
        let pattern = format!("{}\\_%", old.replace('_', "\\_"));
        let names: Vec<String> = tx
            .query(
                "SELECT conname::text FROM pg_constraint \
                 WHERE conrelid = to_regclass(quote_ident(current_schema()) || '.' || quote_ident($1)) \
                 AND conname LIKE $2 ORDER BY conname",
                &[&new, &pattern],
            )
            .await
            .map_err(|e| db("prepare constraints", &e))?
            .iter()
            .map(|r| r.get(0))
            .collect();
        for name in names {
            let Some(rest) = name.strip_prefix(old) else {
                continue;
            };
            tx.batch_execute(&format!(
                "ALTER TABLE {} RENAME CONSTRAINT {} TO {}",
                quote(new),
                quote(&name),
                quote(&format!("{new}{rest}"))
            ))
            .await
            .map_err(|e| db("prepare rename constraint", &e))?;
        }
    }
    Ok(())
}

/// A storage name as a quoted identifier. The names are this crate's constants, so quoting is
/// safe; the escape is defensive.
fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}
