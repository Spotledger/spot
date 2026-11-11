//! Keeping the projection in step with the entity, in the write's own transaction.
//!
//! **This is the decision ADR-087 section 7.3 makes, and this crate takes it literally:** the
//! projection row is written **in the same transaction as the document write**, on the same
//! connection, never from the outbox and never by a background job. The ADR's reason is the
//! reason this module exists: a committed write is visible to the next search, with no separate
//! pipeline and no window in which a listing that was just unpublished is still findable. The
//! outbox path in the ADR belongs to the *external engine* case (section 7.8's `projection_lag`,
//! fed from the outbox) — that engine is not built, and when it is, it consumes the same rows
//! this module writes rather than replacing them (section 7.1: a read projection behind the same
//! seam, the database path remains the guaranteed one).
//!
//! # What the caller does, and why that is enough
//!
//! The document engine calls [`upsert`] after it has written a document (insert, update, rename,
//! restore) and [`remove`] after it has deleted one, on the transaction that performed the write:
//!
//! ```text
//! let version = tx.documents().insert(&doc).await?;   // the entity row
//! spot_pg_search::upsert(&mut tx, &plan, doc.id).await?;  // its projection, same transaction
//! tx.commit().await?;                                  // both, or neither
//! ```
//!
//! Both statements read the entity row **inside the same transaction**, so the projection is
//! built from what was just written — not from a copy carried in an event payload that could be
//! stale or, worse, differ from the stored row. That also means the projection cannot be wrong
//! about a field the caller cannot see: it only ever holds what the table holds.
//!
//! A rollback removes both; a conflict on the entity write removes both. There is no path by
//! which one commits and the other does not, which is the property the tests assert with a
//! rolled-back insert and a rolled-back delete.
//!
//! # Writes are counted
//!
//! Each maintenance statement is one write against the transaction's `max_writes`, declared
//! through [`PgTransaction::raw_execute`], so a caller writing ten thousand documents in one
//! transaction is told `TransactionTooLarge` at the same limit as any other write (ADR-45).

use spot_postgres::PgTransaction;
use spot_storage_contract::{Name, RecordId};

use crate::error::SearchError;
use crate::plan::ProjectionPlan;
use crate::schema::{DOC_ID, DOCTYPE, TABLE, quote};

/// Writes (or rewrites) the projection row of `id` from its entity row.
///
/// Returns the number of rows written: `1`, or `0` when the entity row is not there (a document
/// deleted in the same transaction, or an id from another DocType). The entity table is only
/// read.
///
/// # Errors
///
/// A storage failure from the transaction, including `TransactionTooLarge` before any SQL is
/// sent.
pub async fn upsert(
    tx: &mut PgTransaction,
    plan: &ProjectionPlan,
    id: RecordId,
) -> Result<u64, SearchError> {
    let sql = plan.upsert_sql(Some("\"id\" = $3::uuid"));
    let (doctype, language) = (
        plan.doctype().as_str().to_owned(),
        plan.language().as_str().to_owned(),
    );
    let uuid = uuid::Uuid::from_bytes(*id.as_bytes());
    let affected = tx.raw_execute(&sql, &[&doctype, &language, &uuid]).await?;
    Ok(affected)
}

/// Removes the projection row of `id`, for a document that was deleted (or a DocType that is no
/// longer searchable).
///
/// Returns the number of rows removed: `1` or `0`.
///
/// # Errors
///
/// A storage failure from the transaction.
pub async fn remove(
    tx: &mut PgTransaction,
    doctype: &Name,
    id: RecordId,
) -> Result<u64, SearchError> {
    let sql = format!(
        "DELETE FROM {} WHERE {} = $1::uuid AND {} = $2::text",
        quote(TABLE),
        quote(DOC_ID),
        quote(DOCTYPE)
    );
    let uuid = uuid::Uuid::from_bytes(*id.as_bytes());
    let affected = tx.raw_execute(&sql, &[&uuid, &doctype.as_str()]).await?;
    Ok(affected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Protection;
    use crate::plan::Language;

    fn plan() -> ProjectionPlan {
        let catalog = spot_storage_contract::contract_tests::search_catalog()
            .unwrap_or_else(|e| panic!("{e}"));
        let entity = catalog
            .entity(&Name::new("listing").unwrap_or_else(|e| panic!("{e}")))
            .unwrap_or_else(|e| panic!("{e}"));
        ProjectionPlan::from_entity(
            entity,
            &Language::new("en").unwrap_or_else(|e| panic!("{e}")),
            &Protection::new(),
        )
        .unwrap_or_else(|e| panic!("{e}"))
        .unwrap_or_else(|| panic!("listing declares search"))
    }

    #[test]
    fn the_upsert_statement_names_one_row_and_carries_three_parameters() {
        let sql = plan().upsert_sql(Some("\"id\" = $3::uuid"));
        assert!(sql.contains("WHERE \"id\" = $3::uuid"));
        assert_eq!(sql.matches('$').count(), 3, "{sql}");
    }

    #[test]
    fn a_projection_write_never_touches_the_entity_table() {
        let sql = plan().upsert_sql(Some("\"id\" = $3::uuid"));
        assert_eq!(sql.matches("INSERT INTO").count(), 1);
        for write in [
            "UPDATE \"listing\"",
            "DELETE FROM \"listing\"",
            "ALTER TABLE",
        ] {
            assert!(
                !sql.contains(write),
                "`{write}` must not be in a projection write"
            );
        }
    }
}
