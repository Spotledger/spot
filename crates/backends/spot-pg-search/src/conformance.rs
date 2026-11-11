//! The conformance checks that fail loudly (ADR-087 section 7.3 and section 10).
//!
//! Three settings can be wrong in a way no test of ours would notice until production, so they
//! are checked before a search is served and again by CI:
//!
//! - **`gin_fuzzy_search_limit` must be 0.** If it is ever set above 0, GIN returns "a subset of
//!   the whole result set, chosen at random", which silently breaks stable paging and makes two
//!   identical searches disagree. ADR-087 section 10 marks this as a failed *conformance check*
//!   in CI, not a hope, and this module is the check.
//! - **`unaccent` must be installed** when a projection folds accents (7.8: a projection that
//!   declares it refuses to prepare), and **`pg_trgm`** when one declares typo tolerance. Both
//!   are trusted extensions, and both are installed through `extension.install` only.
//! - **The GIN index must exist and be valid.** A concurrent build that was interrupted leaves an
//!   invalid index that PostgreSQL will not use: the search would still be correct and suddenly
//!   slow, which is exactly the kind of failure that needs to be loud.
//!
//! [`check`] returns a [`ConformanceReport`] on success and the first violation as an error,
//! listing every finding in the explanation so one run tells an operator everything that is
//! wrong rather than one thing at a time.

use std::collections::BTreeSet;

use spot_postgres::PgTransaction;

use crate::error::SearchError;
use crate::plan::ProjectionPlan;
use crate::schema;

/// What the database reported for the checks that passed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConformanceReport {
    /// The effective `gin_fuzzy_search_limit` (must be 0).
    pub gin_fuzzy_search_limit: i64,
    /// The extensions installed in this database.
    pub extensions: BTreeSet<String>,
    /// Whether `spot_search_doc_terms_gin` exists and is valid.
    pub index_valid: bool,
    /// Whether `spot_search_doc` exists at all (a fresh tenant has no projection until
    /// `prepare`).
    pub table_present: bool,
}

impl ConformanceReport {
    /// Every violation for `plans`, in a stable order.
    #[must_use]
    pub fn findings(&self, plans: &[ProjectionPlan]) -> Vec<String> {
        let mut findings = Vec::new();
        if self.gin_fuzzy_search_limit != 0 {
            findings.push(format!(
                "`gin_fuzzy_search_limit` is {} and must be 0: above 0 GIN returns a random subset \
                 of the matches, so paging is not stable (ADR-087 section 7.3)",
                self.gin_fuzzy_search_limit
            ));
        }
        if plans.iter().any(ProjectionPlan::folds_accents) && !self.extensions.contains("unaccent")
        {
            findings.push(
                "the `unaccent` extension is not installed, so a projection that folds accents \
                 cannot be built or searched (ADR-087 section 7.8)"
                    .to_owned(),
            );
        }
        if plans.iter().any(ProjectionPlan::typo_tolerance) && !self.extensions.contains("pg_trgm")
        {
            findings.push(
                "a search declaration asks for typo tolerance and the `pg_trgm` extension is not \
                 installed (ADR-087 section 7.8)"
                    .to_owned(),
            );
        }
        if self.table_present && !self.index_valid {
            findings.push(format!(
                "`{}` is missing or invalid, so a search would scan instead of using the GIN \
                 index; rebuild it concurrently and verify it (ADR-087 section 7.9)",
                schema::TERMS_INDEX
            ));
        }
        findings
    }

    /// Whether every check passed for `plans`.
    #[must_use]
    pub fn ok(&self, plans: &[ProjectionPlan]) -> bool {
        self.findings(plans).is_empty()
    }
}

/// Reads the settings and checks them against `plans`, failing with every violation at once.
///
/// # Errors
///
/// `InvalidSchema` naming the findings when a check fails, or a storage failure from the
/// transaction.
pub async fn check(
    tx: &mut PgTransaction,
    plans: &[ProjectionPlan],
) -> Result<ConformanceReport, SearchError> {
    let report = read(tx).await?;
    let findings = report.findings(plans);
    if findings.is_empty() {
        return Ok(report);
    }
    Err(SearchError::invalid_schema(format!(
        "the database does not conform to the search projection's requirements: {}",
        findings.join("; ")
    )))
}

/// Reads the settings without judging them (for a report an operator asks for).
///
/// # Errors
///
/// A storage failure from the transaction.
pub async fn read(tx: &mut PgTransaction) -> Result<ConformanceReport, SearchError> {
    let limit = tx
        .raw_query_opt(
            "SELECT setting::text FROM pg_settings WHERE name = 'gin_fuzzy_search_limit'",
            &[],
            None,
        )
        .await?
        .and_then(|row| row.try_get::<_, String>(0).ok())
        .and_then(|text| text.trim().parse::<i64>().ok())
        .unwrap_or(0);
    let extensions = schema::installed_extensions(tx).await?;
    let table_present = tx
        .raw_query_opt(
            "SELECT to_regclass($1::text) IS NOT NULL",
            &[&schema::TABLE],
            None,
        )
        .await?
        .and_then(|row| row.try_get::<_, bool>(0).ok())
        .unwrap_or(false);
    let index_valid = if table_present {
        schema::index_is_valid(tx).await?
    } else {
        false
    };
    Ok(ConformanceReport {
        gin_fuzzy_search_limit: limit,
        extensions,
        index_valid,
        table_present,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Protection;
    use crate::plan::Language;

    fn plan(typo: bool) -> ProjectionPlan {
        let catalog = spot_storage_contract::contract_tests::search_catalog()
            .unwrap_or_else(|e| panic!("{e}"));
        let entity = catalog
            .entity(&spot_storage_contract::Name::new("listing").unwrap_or_else(|e| panic!("{e}")))
            .unwrap_or_else(|e| panic!("{e}"));
        let plan = ProjectionPlan::from_entity(
            entity,
            &Language::new("en").unwrap_or_else(|e| panic!("{e}")),
            &Protection::new(),
        )
        .unwrap_or_else(|e| panic!("{e}"))
        .unwrap_or_else(|| panic!("listing declares search"));
        plan.with_typo_tolerance(typo)
    }

    fn report(limit: i64, extensions: &[&str], index_valid: bool) -> ConformanceReport {
        ConformanceReport {
            gin_fuzzy_search_limit: limit,
            extensions: extensions.iter().map(|e| (*e).to_owned()).collect(),
            index_valid,
            table_present: true,
        }
    }

    #[test]
    fn a_conforming_database_has_no_findings() {
        let report = report(0, &["unaccent", "pg_trgm"], true);
        assert!(report.ok(&[plan(false)]));
        assert!(report.findings(&[plan(false)]).is_empty());
    }

    #[test]
    fn a_non_zero_fuzzy_search_limit_is_a_finding() {
        let report = report(1000, &["unaccent"], true);
        let findings = report.findings(&[plan(false)]);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].contains("gin_fuzzy_search_limit"));
        assert!(findings[0].contains("must be 0"));
    }

    #[test]
    fn a_missing_unaccent_is_a_finding_and_an_unfolded_plan_does_not_need_it() {
        let report = report(0, &[], true);
        assert!(report.findings(&[plan(false)])[0].contains("unaccent"));
        let unfolded = plan(false).without_accent_folding();
        assert!(report.findings(&[unfolded]).is_empty());
    }

    #[test]
    fn a_missing_pg_trgm_matters_only_when_typo_tolerance_is_declared() {
        let report = report(0, &["unaccent"], true);
        assert!(report.findings(&[plan(false)]).is_empty());
        let findings = report.findings(&[plan(true)]);
        assert_eq!(findings.len(), 1);
        assert!(findings[0].contains("pg_trgm"));
        assert!(findings[0].contains("typo tolerance"));
    }

    #[test]
    fn an_invalid_index_is_a_finding() {
        let report = report(0, &["unaccent"], false);
        assert!(report.findings(&[plan(false)])[0].contains("spot_search_doc_terms_gin"));
    }

    #[test]
    fn a_missing_table_is_not_an_index_finding() {
        let mut report = report(0, &["unaccent"], false);
        report.table_present = false;
        assert!(report.findings(&[plan(false)]).is_empty());
    }
}
