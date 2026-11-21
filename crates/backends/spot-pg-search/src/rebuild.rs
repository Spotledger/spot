//! Rebuilding one DocType's projection from the entity's own data (ADR-087 sections 7.3 and 7.5).
//!
//! A metadata change that alters a weight or a language is a backfill, not an additive change
//! (section 7.9), and an operator needs one operation that says "make the projection for this
//! DocType match the table again". That is [`rebuild`]: it deletes the DocType's projection rows
//! and re-projects every entity row, **reading the entity table and never writing it**.
//!
//! The two statements are ordered so a failure leaves the projection whole or empty, never
//! half-built with stale rows: both run in the caller's transaction, so a rollback restores the
//! previous projection exactly. The re-projection uses the same expression a write uses
//! ([`ProjectionPlan::upsert_sql`]), which is what makes a rebuild produce a projection
//! *identical* to the one writes produced — the property the tests compare row for row.
//!
//! Rebuilding is deliberately per DocType and not cluster-wide: a caller that must rebuild
//! everything calls it once per planned DocType, and each call can be its own transaction.

use spot_postgres::PgTransaction;
use spot_storage_contract::Name;

use crate::error::SearchError;
use crate::plan::ProjectionPlan;
use crate::schema::{DOCTYPE, TABLE, quote};

/// What a rebuild did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RebuildReport {
    /// The DocType rebuilt.
    pub doctype: Name,
    /// Projection rows removed before the rebuild.
    pub removed: u64,
    /// Projection rows written from the entity table.
    pub projected: u64,
}

/// Rebuilds `plan`'s DocType: clears its projection rows and projects every entity row again.
///
/// # Errors
///
/// A storage failure from the transaction. Nothing is committed by this call: the caller's
/// transaction decides, and the order is `DELETE` then `INSERT … SELECT`, so an interrupted
/// rebuild leaves the previous projection intact after the rollback.
pub async fn rebuild(
    tx: &mut PgTransaction,
    plan: &ProjectionPlan,
) -> Result<RebuildReport, SearchError> {
    let delete = format!(
        "DELETE FROM {} WHERE {} = $1::text",
        quote(TABLE),
        quote(DOCTYPE)
    );
    let removed = tx
        .raw_execute_with(
            PgTransaction::write(1),
            &delete,
            &[&plan.doctype().as_str()],
        )
        .await?;

    let doctype = plan.doctype().as_str().to_owned();
    let language = plan.language().as_str().to_owned();
    let projected = tx
        .raw_execute_with(
            PgTransaction::write(1),
            &plan.upsert_sql(None),
            &[&doctype, &language],
        )
        .await?;

    Ok(RebuildReport {
        doctype: plan.doctype().clone(),
        removed,
        projected,
    })
}

/// Rebuilds every planned DocType in one transaction.
///
/// # Errors
///
/// The first failure, which rolls the whole transaction back when the caller does.
pub async fn rebuild_all(
    tx: &mut PgTransaction,
    plans: &[ProjectionPlan],
) -> Result<Vec<RebuildReport>, SearchError> {
    let mut reports = Vec::with_capacity(plans.len());
    for plan in plans {
        reports.push(rebuild(tx, plan).await?);
    }
    Ok(reports)
}

/// The DocTypes this catalog plans a projection for, so a caller can rebuild what exists without
/// inventing an entity name.
///
/// # Errors
///
/// A translation failure: `InvalidSchema` for a declaration the projection cannot honour, or a
/// storage failure for a bad language tag. `Ok(vec![])` when nothing declares search.
pub fn planned_doctypes(
    catalog: &spot_storage_contract::Catalog,
    language: &crate::plan::Language,
    protection: &crate::Protection,
) -> Result<Vec<ProjectionPlan>, SearchError> {
    let mut plans = Vec::new();
    for entity in catalog.entities() {
        if let Some(plan) = ProjectionPlan::from_entity(entity, language, protection)? {
            plans.push(plan);
        }
    }
    Ok(plans)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_planned_catalog_lists_the_searchable_doctypes() {
        let catalog = spot_storage_contract::contract_tests::search_catalog()
            .unwrap_or_else(|e| panic!("{e}"));
        let plans = planned_doctypes(
            &catalog,
            &crate::plan::Language::new("en").unwrap_or_else(|e| panic!("{e}")),
            &crate::Protection::new(),
        )
        .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(plans.len(), 1);
        assert_eq!(plans.first().map(|p| p.doctype().as_str()), Some("listing"));
    }

    #[test]
    fn a_rebuild_statement_touches_only_the_projection() {
        let catalog = spot_storage_contract::contract_tests::search_catalog()
            .unwrap_or_else(|e| panic!("{e}"));
        let entity = catalog
            .entity(&Name::new("listing").unwrap_or_else(|e| panic!("{e}")))
            .unwrap_or_else(|e| panic!("{e}"));
        let plan = ProjectionPlan::from_entity(
            entity,
            &crate::plan::Language::new("en").unwrap_or_else(|e| panic!("{e}")),
            &crate::Protection::new(),
        )
        .unwrap_or_else(|e| panic!("{e}"))
        .unwrap_or_else(|| panic!("listing declares search"));
        let sql = plan.upsert_sql(None);
        assert!(sql.contains("FROM \"listing\""), "the entity is only read");
        assert!(!sql.contains("UPDATE \"listing\""));
        assert!(sql.contains("ON CONFLICT (\"doc_id\") DO UPDATE SET"));
    }
}
