//! The projection's objects: the table, its columns, its indexes, and `prepare`.
//!
//! ADR-087 section 7.3 fixes the shape:
//!
//! ```text
//! spot_search_doc (doc_id uuid primary key, doctype text, terms tsvector, facets jsonb,
//!                  tenant_language text, updated_at timestamptz)
//! CREATE INDEX spot_search_doc_terms_gin ON spot_search_doc USING GIN (terms)
//! ```
//!
//! It is a **platform table, not a metadata entity**: the name carries the `spot_` prefix the
//! migrator's gate requires (`tools/lints/table-prefix/README.md`), so the migrator never treats
//! it as an app table it may drop, and one tenant is one database (ADR-34), so `doc_id` alone is
//! the primary key.
//!
//! Two decisions are worth stating here because they are not obvious from the DDL:
//!
//! - **`terms` is `NOT NULL`** and the maintenance statement always writes a value: every field
//!   is coalesced, so a document with no text at all produces the empty `tsvector` rather than a
//!   null row that no query could ever match.
//! - **The GIN index is built concurrently for an existing tenant** (ADR-087 section 7.9). Plain
//!   `CREATE INDEX` takes a write lock for the whole build, and `CREATE INDEX CONCURRENTLY`
//!   cannot run inside a transaction block, which every statement this crate sends is. So
//!   [`concurrent_index_sql`] is the statement the migrator plans as `Run::Outside`, and
//!   [`prepare`] uses the plain form (it only ever builds the index on a table that is empty or
//!   minutes old, in the transaction that created it). Either way the plan must then check that
//!   the index became valid — [`index_is_valid`] is that check, and [`conformance::check`]
//!   (crate::conformance) runs it again before a search is served.
//!
//! [`index_is_valid`]: crate::schema::index_is_valid

use spot_storage_contract::Name;

use crate::error::SearchError;
use crate::plan::ProjectionPlan;

/// The projection table (ADR-087 section 7.3).
pub const TABLE: &str = "spot_search_doc";
/// The record the projection row describes.
pub const DOC_ID: &str = "doc_id";
/// The DocType (entity) name the record belongs to.
pub const DOCTYPE: &str = "doctype";
/// The weighted `tsvector` of the searchable fields.
pub const TERMS: &str = "terms";
/// The declared facet fields' values, as a JSON object.
pub const FACETS: &str = "facets";
/// The language the projection was built with (the DocType's, or the tenant collation's).
pub const TENANT_LANGUAGE: &str = "tenant_language";
/// When the projection row was last written.
pub const UPDATED_AT: &str = "updated_at";
/// The GIN index over `terms` (ADR-087 section 7.3).
pub const TERMS_INDEX: &str = "spot_search_doc_terms_gin";
/// A B-tree over `doctype`, for a rebuild and for a projection-lag count.
pub const DOCTYPE_INDEX: &str = "spot_search_doc_doctype_btree";

/// Quotes an identifier that came from a validated [`Name`] or from this module's constants.
pub(crate) fn quote(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// `CREATE TABLE IF NOT EXISTS spot_search_doc (…)`.
#[must_use]
pub fn create_table_sql() -> String {
    format!(
        "CREATE TABLE IF NOT EXISTS {} (\
         {} uuid NOT NULL, \
         {} text NOT NULL, \
         {} tsvector NOT NULL, \
         {} jsonb NOT NULL DEFAULT '{{}}'::jsonb, \
         {} text NOT NULL, \
         {} timestamptz NOT NULL DEFAULT now(), \
         PRIMARY KEY ({}))",
        quote(TABLE),
        quote(DOC_ID),
        quote(DOCTYPE),
        quote(TERMS),
        quote(FACETS),
        quote(TENANT_LANGUAGE),
        quote(UPDATED_AT),
        quote(DOC_ID),
    )
}

/// `CREATE INDEX IF NOT EXISTS spot_search_doc_terms_gin … USING GIN (terms)`.
#[must_use]
pub fn create_index_sql() -> String {
    format!(
        "CREATE INDEX IF NOT EXISTS {} ON {} USING GIN ({})",
        quote(TERMS_INDEX),
        quote(TABLE),
        quote(TERMS)
    )
}

/// The B-tree over `doctype`, used by [`rebuild`](crate::rebuild) and by the projection-lag count.
#[must_use]
pub fn create_doctype_index_sql() -> String {
    format!(
        "CREATE INDEX IF NOT EXISTS {} ON {} ({})",
        quote(DOCTYPE_INDEX),
        quote(TABLE),
        quote(DOCTYPE)
    )
}

/// The same GIN index, built without blocking writes, for a tenant that already has documents
/// (ADR-087 section 7.9). `CREATE INDEX CONCURRENTLY` cannot run inside a transaction block, so
/// the migrator plans this as `Run::Outside`; verify with [`index_is_valid`] afterwards.
#[must_use]
pub fn concurrent_index_sql() -> String {
    format!(
        "CREATE INDEX CONCURRENTLY IF NOT EXISTS {} ON {} USING GIN ({})",
        quote(TERMS_INDEX),
        quote(TABLE),
        quote(TERMS)
    )
}

/// Whether `spot_search_doc_terms_gin` exists and PostgreSQL reports it valid.
///
/// A concurrent build that was interrupted leaves an invalid index behind that PostgreSQL will
/// not use; this is the check that catches it instead of a slow search in production.
///
/// # Errors
///
/// A storage failure from the transaction.
pub async fn index_is_valid(tx: &mut spot_postgres::PgTransaction) -> Result<bool, SearchError> {
    let sql = "SELECT i.indisvalid FROM pg_index i WHERE i.indexrelid = to_regclass($1::text)";
    let row = tx.raw_query_opt(sql, &[&TERMS_INDEX], None).await?;
    Ok(row.is_some_and(|row| row.try_get::<_, bool>(0).unwrap_or(false)))
}

/// The extensions installed in this database, by name.
///
/// # Errors
///
/// A storage failure from the transaction.
pub async fn installed_extensions(
    tx: &mut spot_postgres::PgTransaction,
) -> Result<std::collections::BTreeSet<String>, SearchError> {
    let rows = tx
        .raw_query("SELECT extname::text FROM pg_extension", &[], None)
        .await?;
    let mut names = std::collections::BTreeSet::new();
    for row in rows {
        if let Ok(name) = row.try_get::<_, String>(0) {
            names.insert(name);
        }
    }
    Ok(names)
}

/// Makes the database able to hold the projections of `plans`: the extensions those plans need,
/// the table, and the indexes. Additive only — it never drops or rewrites what is there.
///
/// The extension rules of ADR-087 section 7.8 apply, and both are refusals rather than
/// degradations:
///
/// - `unaccent` is required by a projection that folds accents (the default; `unaccent` is a
///   filtering dictionary used **only** inside the projection, so equality and unique keys keep
///   treating accents literally, SEM-TXT-03);
/// - `pg_trgm` is required by a projection that declares `typo_tolerance`.
///
/// Installing an extension is not this crate's job: `extension.install` in the Admin Center's
/// database manager is the only way (ADR-087 section 4.3), so a missing extension is
/// `InvalidSchema` naming the extension and that remedy.
///
/// # Errors
///
/// `InvalidSchema` for a missing extension or an index that did not become valid; a storage
/// failure from the transaction.
pub async fn prepare(
    tx: &mut spot_postgres::PgTransaction,
    plans: &[ProjectionPlan],
) -> Result<(), SearchError> {
    let installed = installed_extensions(tx).await?;
    check_extensions(plans, &installed)?;

    tx.raw_execute_with(spot_postgres::PgTransaction::READ, &create_table_sql(), &[])
        .await?;
    tx.raw_execute_with(
        spot_postgres::PgTransaction::READ,
        &create_doctype_index_sql(),
        &[],
    )
    .await?;
    tx.raw_execute_with(spot_postgres::PgTransaction::READ, &create_index_sql(), &[])
        .await?;

    if !index_is_valid(tx).await? {
        return Err(SearchError::invalid_schema(format!(
            "`{TERMS_INDEX}` exists but is not valid; a concurrent build was interrupted. Rebuild \
             it with `{}`",
            concurrent_index_sql()
        )));
    }
    Ok(())
}

/// Refuses when an extension a plan needs is not installed (ADR-087 section 7.8).
///
/// # Errors
///
/// `InvalidSchema`, naming the missing extension(s) and the remedy.
pub fn check_extensions(
    plans: &[ProjectionPlan],
    installed: &std::collections::BTreeSet<String>,
) -> Result<(), SearchError> {
    let mut missing: Vec<&str> = Vec::new();
    if plans.iter().any(ProjectionPlan::folds_accents) && !installed.contains("unaccent") {
        missing.push("unaccent");
    }
    if plans.iter().any(ProjectionPlan::typo_tolerance) && !installed.contains("pg_trgm") {
        missing.push("pg_trgm");
    }
    if missing.is_empty() {
        return Ok(());
    }
    Err(SearchError::invalid_schema(format!(
        "the search projection needs the PostgreSQL extension(s) {}; install them through \
         `extension.install` in the Admin Center database manager (ADR-087 section 4.3), then \
         prepare again",
        missing.join(", ")
    )))
}

/// A [`Name`] from one of this module's constants, for callers that need them typed.
///
/// # Panics
///
/// Never: the constants are lower-case identifiers by construction, and the unit tests prove it.
#[must_use]
pub fn column(name: &str) -> Name {
    match Name::new(name) {
        Ok(name) => name,
        Err(_) => unreachable!("a projection column name is a valid storage name"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use spot_pg_query::{SEARCH_DOC_ID, SEARCH_DOCTYPE, SEARCH_TABLE, SEARCH_TERMS};

    #[test]
    fn the_names_match_the_query_compiler_expects() {
        // The compiler (`spot-pg-query`) already reads this table; the two must not drift.
        assert_eq!(TABLE, SEARCH_TABLE);
        assert_eq!(DOC_ID, SEARCH_DOC_ID);
        assert_eq!(DOCTYPE, SEARCH_DOCTYPE);
        assert_eq!(TERMS, SEARCH_TERMS);
    }

    #[test]
    fn every_object_is_spot_prefixed() {
        for name in [TABLE, TERMS_INDEX, DOCTYPE_INDEX] {
            assert!(
                name.starts_with("spot_"),
                "`{name}` misses the spot_ prefix"
            );
        }
    }

    #[test]
    fn the_table_has_the_columns_the_adr_names() {
        let sql = create_table_sql();
        for column in [DOC_ID, DOCTYPE, TERMS, FACETS, TENANT_LANGUAGE, UPDATED_AT] {
            assert!(sql.contains(&format!("\"{column}\"")), "`{column}` missing");
        }
        assert!(sql.contains("\"doc_id\" uuid NOT NULL"));
        assert!(sql.contains("\"terms\" tsvector NOT NULL"));
        assert!(sql.contains("\"facets\" jsonb NOT NULL"));
        assert!(sql.contains("PRIMARY KEY (\"doc_id\")"));
        assert!(sql.starts_with("CREATE TABLE IF NOT EXISTS \"spot_search_doc\""));
    }

    #[test]
    fn the_gin_index_is_on_terms_and_the_concurrent_form_is_usable() {
        assert_eq!(
            create_index_sql(),
            "CREATE INDEX IF NOT EXISTS \"spot_search_doc_terms_gin\" ON \"spot_search_doc\" USING \
             GIN (\"terms\")"
        );
        assert!(concurrent_index_sql().contains("CREATE INDEX CONCURRENTLY IF NOT EXISTS"));
        assert!(concurrent_index_sql().contains("USING GIN (\"terms\")"));
        assert!(create_doctype_index_sql().contains("(\"doctype\")"));
    }

    #[test]
    fn a_missing_extension_is_named_with_the_remedy() {
        let catalog = spot_storage_contract::contract_tests::search_catalog()
            .unwrap_or_else(|e| panic!("{e}"));
        let entity = catalog
            .entity(&column("listing"))
            .unwrap_or_else(|e| panic!("{e}"));
        let language = crate::plan::Language::new("en").unwrap_or_else(|e| panic!("{e}"));
        let plan = ProjectionPlan::from_entity(entity, &language, &crate::Protection::new())
            .unwrap_or_else(|e| panic!("{e}"))
            .unwrap_or_else(|| panic!("listing declares search"));
        let error = match check_extensions(&[plan], &Default::default()) {
            Ok(()) => panic!("a missing unaccent must be refused"),
            Err(error) => error,
        };
        assert_eq!(error.code(), "storage.invalid_schema");
        assert!(error.explanation().contains("unaccent"));
        assert!(error.explanation().contains("extension.install"));
    }
}
