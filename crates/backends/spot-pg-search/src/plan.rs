//! What one DocType's search declaration compiles to, and the SQL that builds a projection row.
//!
//! A [`ProjectionPlan`] is the pure half of the projection: it validates the metadata
//! declaration of ADR-087 section 7.2 against the entity and the protection groups, fixes the
//! per-field language, and produces the SQL expressions for [`terms`](ProjectionPlan::terms_sql)
//! and [`facets`](ProjectionPlan::facets_sql). Maintenance ([`crate::maintain`]) and
//! [`crate::rebuild`] both write rows through these expressions, so there is exactly one
//! definition of what a projection row contains: a rebuild cannot drift from a write.
//!
//! # The weighted vector
//!
//! `terms` is the pattern the PostgreSQL documentation gives for relevance over several fields:
//!
//! ```text
//! setweight(to_tsvector('simple', unaccent(coalesce("title"::text, ''))), 'A')
//!   || setweight(to_tsvector('simple', unaccent(coalesce("body"::text, ''))), 'D')
//! ```
//!
//! Every field is `coalesce`d because `to_tsvector(NULL)` is `NULL` and one null operand would
//! make the whole concatenation null (the wart ADR-087 section 7.3 lists). Every field carries
//! the weight the declaration gives it (A to D, the closed `setweight` set); a GIN index stores
//! lexemes only and not their weights, so a weighted query is rechecked against the table row
//! (`ts_rank`/the `&&&` family), which is what `terms @@ tsquery` does.
//!
//! # Language, per field
//!
//! Each field's parser and dictionary come from its own language, so an English title and an
//! Urdu body can be stemmed differently in the same row: the declaration's `language` is the
//! default, and [`ProjectionPlan::with_field_language`] overrides it for one field. A language
//! PostgreSQL has no configuration for — and that includes **Urdu**, which Snowball does not
//! provide — falls back to `simple` (lower case and the tokeniser), as ADR-087 section 7.4
//! requires. The mapping is a closed table: nothing from metadata ever reaches SQL as an
//! identifier, and a language tag the table does not know is `simple`, never an error for a
//! tenant whose content is simply not stemmed.
//!
//! One consequence is worth stating plainly, because it constrains the query side:
//! `to_tsvector(config, …)` and `to_tsquery(config, …)` must use the **same** configuration or
//! the lexemes do not meet. This crate's own search path parses per field; `Expr::Search` through
//! `spot-pg-query` parses every term with `simple`. So a projection built with a stemming
//! configuration answers this crate's search fully and `Expr::Search` by exact lexeme only.
//! `simple` for every field — which is the default and what Urdu gets anyway — makes the two
//! agree, and that is the configuration a DocType that must answer both paths should declare.
//!
//! # `unaccent`, only inside the projection
//!
//! Accent folding happens when a row is projected, through the `unaccent` dictionary, and
//! nowhere else: equality, ordering and unique keys keep treating `café` and `cafe` as different
//! strings (SEM-TXT-03). A plan that does not fold accents is available
//! ([`without_accent_folding`](ProjectionPlan::without_accent_folding)) for a tenant that wants
//! that, and then `unaccent` is not required at `prepare`.

use std::collections::BTreeSet;

use spot_storage_contract::{EntityDef, ErrorKind, FieldType, Name, SearchWeight, StorageError};

use crate::Protection;
use crate::error::SearchError;
use crate::schema::{DOC_ID, DOCTYPE, FACETS, TABLE, TENANT_LANGUAGE, UPDATED_AT, quote};

/// The language tags this crate maps to a PostgreSQL text search configuration, by their primary
/// subtag.
///
/// Deliberately a table and not a query: the configuration name goes into SQL, so it must come
/// from a closed set. `ur` is absent because PostgreSQL has no Urdu configuration (Snowball does
/// not provide one), so Urdu content takes `simple`.
const CONFIGURATIONS: &[(&str, &str)] = &[
    ("ar", "arabic"),
    ("ca", "catalan"),
    ("da", "danish"),
    ("de", "german"),
    ("el", "greek"),
    ("en", "english"),
    ("es", "spanish"),
    ("et", "estonian"),
    ("eu", "basque"),
    ("fi", "finnish"),
    ("fr", "french"),
    ("ga", "irish"),
    ("hi", "hindi"),
    ("hu", "hungarian"),
    ("hy", "armenian"),
    ("id", "indonesian"),
    ("it", "italian"),
    ("lt", "lithuanian"),
    ("ne", "nepali"),
    ("nl", "dutch"),
    ("no", "norwegian"),
    ("pt", "portuguese"),
    ("ro", "romanian"),
    ("ru", "russian"),
    ("sr", "serbian"),
    ("sv", "swedish"),
    ("ta", "tamil"),
    ("tr", "turkish"),
    ("yi", "yiddish"),
];

/// The configuration a language tag gets when the table has no entry for it.
pub const FALLBACK_CONFIGURATION: &str = "simple";

/// A language tag, validated and normalised (lower case, `-` separators).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Language(String);

impl Language {
    /// Validates a BCP-47-shaped tag such as `en`, `ur-PK` or `sr-Latn-RS`.
    ///
    /// # Errors
    ///
    /// `InvalidName` when the tag is blank, longer than 35 bytes, or holds anything but ASCII
    /// letters, digits, `-` and `_`.
    pub fn new(tag: &str) -> Result<Self, StorageError> {
        let trimmed = tag.trim();
        let valid = !trimmed.is_empty()
            && trimmed.len() <= 35
            && trimmed
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic())
            && trimmed
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !valid {
            return Err(StorageError::new(
                ErrorKind::InvalidName,
                format!("`{tag}` is not a usable language tag"),
            ));
        }
        Ok(Language(trimmed.to_ascii_lowercase().replace('_', "-")))
    }

    /// The normalised tag.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The PostgreSQL text search configuration this tag maps to.
    #[must_use]
    pub fn configuration(&self) -> &'static str {
        configuration_for(&self.0)
    }
}

/// The configuration for `tag`: its primary subtag when this PostgreSQL has one, else
/// [`FALLBACK_CONFIGURATION`].
#[must_use]
pub fn configuration_for(tag: &str) -> &'static str {
    let primary = tag
        .split(['-', '_'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    CONFIGURATIONS
        .iter()
        .find(|(subtag, _)| *subtag == primary)
        .map(|(_, configuration)| *configuration)
        .unwrap_or(FALLBACK_CONFIGURATION)
}

/// One searchable field with its weight and its own language.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedField {
    /// The declared text field.
    pub name: Name,
    /// Its `setweight` label.
    pub weight: SearchWeight,
    /// The parser and dictionary its text is projected with.
    pub language: Language,
}

impl PlannedField {
    /// The `setweight` label as the letter PostgreSQL takes.
    #[must_use]
    pub fn label(&self) -> char {
        match self.weight {
            SearchWeight::A => 'A',
            SearchWeight::B => 'B',
            SearchWeight::C => 'C',
            SearchWeight::D => 'D',
        }
    }
}

/// One DocType's projection: the fields, weights, languages and facets, compiled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectionPlan {
    doctype: Name,
    language: Language,
    fields: Vec<PlannedField>,
    facets: Vec<Name>,
    typo_tolerance: bool,
    fold_accents: bool,
}

impl ProjectionPlan {
    /// Compiles `entity`'s search declaration, or `Ok(None)` when it declares none (search is
    /// opt-in: a DocType without a `search` list gets no projection and pays nothing).
    ///
    /// `language` is the DocType's default, which the caller takes from the declaration and
    /// otherwise from the tenant collation (ADR-053); `protection` is the field-to-group model of
    /// [`crate::Protection`].
    ///
    /// # Errors
    ///
    /// `InvalidSchema` when the declaration cannot be honoured: a searchable field that is not a
    /// declared text field, a faceted field that is not declared, or either of them in a
    /// protection group. A protected field must never reach the projection (ADR-087 section 7.7
    /// points 1 and 4), and refusing here is the only place that can be guaranteed for every
    /// write, so it is a refusal and not a silent omission.
    pub fn from_entity(
        entity: &EntityDef,
        language: &Language,
        protection: &Protection,
    ) -> Result<Option<Self>, SearchError> {
        let Some(declared) = &entity.search else {
            return Ok(None);
        };
        let default = match &declared.language {
            Some(tag) => {
                Language::new(tag).map_err(|e| SearchError::Storage(e.with_entity(&entity.name)))?
            }
            None => language.clone(),
        };
        let mut fields = Vec::new();
        let mut seen = BTreeSet::new();
        for field in &declared.fields {
            let def = entity
                .fields
                .iter()
                .find(|f| f.name == field.name)
                .ok_or_else(|| {
                    SearchError::invalid_schema(format!(
                        "`{}.{}` is declared searchable but is not a field",
                        entity.name, field.name
                    ))
                })?;
            if !matches!(def.ty, FieldType::Text { .. }) {
                return Err(SearchError::invalid_schema(format!(
                    "`{}.{}` is declared searchable but is not a text field",
                    entity.name, field.name
                )));
            }
            if protection.is_protected(&entity.name, &field.name) {
                return Err(SearchError::invalid_schema(format!(
                    "`{}.{}` is in protection group `{}`, so it cannot be searchable: a protected \
                     field never enters a search projection (ADR-087 section 7.7)",
                    entity.name,
                    field.name,
                    protection
                        .group_of(&entity.name, &field.name)
                        .unwrap_or("unknown")
                )));
            }
            if !seen.insert(field.name.clone()) {
                return Err(SearchError::invalid_schema(format!(
                    "`{}.{}` is declared searchable twice",
                    entity.name, field.name
                )));
            }
            fields.push(PlannedField {
                name: field.name.clone(),
                weight: field.weight,
                language: default.clone(),
            });
        }
        if fields.is_empty() {
            return Err(SearchError::invalid_schema(format!(
                "`{}` declares search with no fields",
                entity.name
            )));
        }
        let mut facets = Vec::new();
        for facet in &declared.facets {
            let def = entity
                .fields
                .iter()
                .find(|f| &f.name == facet)
                .ok_or_else(|| {
                    SearchError::invalid_schema(format!(
                        "`{}.{facet}` is declared a facet but is not a field",
                        entity.name
                    ))
                })?;
            if !facetable(&def.ty) {
                return Err(SearchError::invalid_schema(format!(
                    "`{}.{facet}` is declared a facet but its type cannot be counted by",
                    entity.name
                )));
            }
            if protection.is_protected(&entity.name, facet) {
                return Err(SearchError::invalid_schema(format!(
                    "`{}.{facet}` is in a protection group, so it cannot be a facet: a count over \
                     it would reveal that its values exist (ADR-087 section 7.7 point 4)",
                    entity.name
                )));
            }
            facets.push(facet.clone());
        }
        Ok(Some(ProjectionPlan {
            doctype: entity.name.clone(),
            language: default,
            fields,
            facets,
            typo_tolerance: declared.typo_tolerance,
            fold_accents: true,
        }))
    }

    /// The DocType this projection serves.
    #[must_use]
    pub fn doctype(&self) -> &Name {
        &self.doctype
    }

    /// The DocType's default language.
    #[must_use]
    pub fn language(&self) -> &Language {
        &self.language
    }

    /// The searchable fields, in declaration order (which is the vector's order).
    #[must_use]
    pub fn fields(&self) -> &[PlannedField] {
        &self.fields
    }

    /// The faceted fields.
    #[must_use]
    pub fn facets(&self) -> &[Name] {
        &self.facets
    }

    /// Whether the declaration asks for `pg_trgm` typo tolerance.
    #[must_use]
    pub fn typo_tolerance(&self) -> bool {
        self.typo_tolerance
    }

    /// Whether the projection folds accents with `unaccent`.
    #[must_use]
    pub fn folds_accents(&self) -> bool {
        self.fold_accents
    }

    /// Whether `field` is searchable in this projection.
    #[must_use]
    pub fn is_searchable(&self, field: &Name) -> bool {
        self.fields.iter().any(|f| &f.name == field)
    }

    /// Whether `field` is a declared facet of this projection.
    #[must_use]
    pub fn is_facet(&self, field: &Name) -> bool {
        self.facets.contains(field)
    }

    /// The language one field is projected with.
    ///
    /// # Errors
    ///
    /// `UnknownField` when `field` is not searchable.
    pub fn field_language(&self, field: &Name) -> Result<&Language, SearchError> {
        self.fields
            .iter()
            .find(|f| &f.name == field)
            .map(|f| &f.language)
            .ok_or_else(|| {
                SearchError::Storage(
                    StorageError::new(
                        ErrorKind::UnknownField,
                        format!("`{}.{field}` is not searchable", self.doctype),
                    )
                    .with_entity(&self.doctype)
                    .with_field(field),
                )
            })
    }

    /// Gives one field its own language (the rest keep the DocType's).
    ///
    /// # Errors
    ///
    /// `UnknownField` when `field` is not searchable.
    pub fn with_field_language(
        mut self,
        field: &Name,
        language: Language,
    ) -> Result<Self, SearchError> {
        let planned = self
            .fields
            .iter_mut()
            .find(|f| &f.name == field)
            .ok_or_else(|| {
                SearchError::Storage(
                    StorageError::new(
                        ErrorKind::UnknownField,
                        format!("`{}.{field}` is not searchable", self.doctype),
                    )
                    .with_entity(&self.doctype)
                    .with_field(field),
                )
            })?;
        planned.language = language;
        Ok(self)
    }

    /// Stops folding accents (and drops the `unaccent` requirement at `prepare`).
    #[must_use]
    pub fn without_accent_folding(mut self) -> Self {
        self.fold_accents = false;
        self
    }

    /// Turns this plan's typo-tolerance flag on or off.
    ///
    /// The flag is this crate's copy of the declaration, used by [`crate::prepare`] and
    /// [`crate::check`] to decide whether `pg_trgm` is required. The query compiler
    /// reads the catalog's own
    /// [`SearchDef`](spot_storage_contract::SearchDef), so a plan that disagrees with its
    /// declaration either requires an extension the queries will not use or misses one they will:
    /// pass the declaration's value.
    #[must_use]
    pub fn with_typo_tolerance(mut self, enabled: bool) -> Self {
        self.typo_tolerance = enabled;
        self
    }

    /// The `to_tsvector` expression of one field, with its weight.
    #[must_use]
    pub fn field_vector_sql(&self, field: &PlannedField) -> String {
        let column = format!("coalesce({}::text, '')", quote(field.name.as_str()));
        let text = if self.fold_accents {
            format!("unaccent('unaccent'::regdictionary, {column})")
        } else {
            column
        };
        format!(
            "setweight(to_tsvector('{}'::regconfig, {text}), '{}')",
            field.language.configuration(),
            field.label()
        )
    }

    /// The weighted `tsvector` expression of the whole projection (`terms`).
    #[must_use]
    pub fn terms_sql(&self) -> String {
        self.fields
            .iter()
            .map(|field| self.field_vector_sql(field))
            .collect::<Vec<_>>()
            .join(" || ")
    }

    /// The `jsonb` expression of the declared facets.
    #[must_use]
    pub fn facets_sql(&self) -> String {
        if self.facets.is_empty() {
            return "'{}'::jsonb".to_owned();
        }
        let pairs = self
            .facets
            .iter()
            .map(|facet| {
                format!(
                    "'{}', {}",
                    facet.as_str().replace('\'', "''"),
                    quote(facet.as_str())
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!("jsonb_build_object({pairs})")
    }

    /// The `SELECT` list and `FROM` of one projection row: the parameters are `$1` the DocType
    /// and `$2` the language.
    #[must_use]
    pub fn row_select_sql(&self) -> String {
        format!(
            "SELECT \"id\", $1::text, {}, {}, $2::text, now() FROM {}",
            self.terms_sql(),
            self.facets_sql(),
            quote(self.doctype.as_str())
        )
    }

    /// The `INSERT … SELECT` that (re)builds a projection row from the entity row, with an
    /// optional `WHERE` over the entity table and `ON CONFLICT` so a write and a rebuild converge
    /// on the same row content.
    ///
    /// `row_filter` is a predicate this crate builds from its own constants (never a caller's
    /// text); `None` projects every row of the entity, which is what a rebuild wants.
    #[must_use]
    pub fn upsert_sql(&self, row_filter: Option<&str>) -> String {
        let filter = match row_filter {
            Some(predicate) => format!(" WHERE {predicate}"),
            None => String::new(),
        };
        format!(
            "INSERT INTO {} ({}, {}, {}, {}, {}, {}) {}{filter} ON CONFLICT ({}) DO UPDATE SET \
             {} = EXCLUDED.{}, {} = EXCLUDED.{}, {} = EXCLUDED.{}, {} = EXCLUDED.{}, \
             {} = now()",
            quote(TABLE),
            quote(DOC_ID),
            quote(DOCTYPE),
            quote(crate::schema::TERMS),
            quote(FACETS),
            quote(TENANT_LANGUAGE),
            quote(UPDATED_AT),
            self.row_select_sql(),
            quote(DOC_ID),
            quote(DOCTYPE),
            quote(DOCTYPE),
            quote(crate::schema::TERMS),
            quote(crate::schema::TERMS),
            quote(FACETS),
            quote(FACETS),
            quote(TENANT_LANGUAGE),
            quote(TENANT_LANGUAGE),
            quote(UPDATED_AT),
        )
    }
}

/// Whether a field's type can be counted as a facet: the scalar, comparable kinds. JSON, bytes,
/// geo and ranges are refused: their values have no stable text form to count by.
fn facetable(ty: &FieldType) -> bool {
    matches!(
        ty,
        FieldType::Bool
            | FieldType::Int
            | FieldType::Decimal { .. }
            | FieldType::Text { .. }
            | FieldType::Date
            | FieldType::Time
            | FieldType::DateTime
            | FieldType::Link { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(text: &str) -> Name {
        Name::new(text).unwrap_or_else(|e| panic!("{e}"))
    }

    fn plan() -> ProjectionPlan {
        let catalog = spot_storage_contract::contract_tests::search_catalog()
            .unwrap_or_else(|e| panic!("{e}"));
        let entity = catalog
            .entity(&name("listing"))
            .unwrap_or_else(|e| panic!("{e}"));
        let language = Language::new("en").unwrap_or_else(|e| panic!("{e}"));
        ProjectionPlan::from_entity(entity, &language, &Protection::new())
            .unwrap_or_else(|e| panic!("{e}"))
            .unwrap_or_else(|| panic!("listing declares search"))
    }

    #[test]
    fn urdu_falls_back_to_simple_and_english_keeps_its_stemmer() {
        assert_eq!(configuration_for("ur"), "simple");
        assert_eq!(configuration_for("ur-PK"), "simple");
        assert_eq!(configuration_for("en"), "english");
        assert_eq!(configuration_for("en-GB"), "english");
        assert_eq!(configuration_for("xx"), "simple");
        assert_eq!(configuration_for(""), "simple");
    }

    #[test]
    fn a_language_tag_is_validated_and_normalised() {
        let tag = Language::new(" UR_pk ").unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(tag.as_str(), "ur-pk");
        assert_eq!(tag.configuration(), "simple");
        for bad in ["", "  ", "1en", "en us", "en!"] {
            assert!(Language::new(bad).is_err(), "`{bad}` should be refused");
        }
    }

    #[test]
    fn the_vector_weights_each_field_and_coalesces_every_one() {
        let sql = plan().terms_sql();
        assert!(sql.contains("setweight(to_tsvector('english'::regconfig, unaccent('unaccent'::regdictionary, coalesce(\"title\"::text, ''))), 'A')"));
        assert!(sql.contains("setweight(to_tsvector('english'::regconfig, unaccent('unaccent'::regdictionary, coalesce(\"body\"::text, ''))), 'D')"));
        assert_eq!(sql.matches(" || ").count(), 1, "two fields, one junction");
        assert!(
            !sql.contains("to_tsvector(coalesce"),
            "no field skips coalesce"
        );
    }

    #[test]
    fn a_field_language_can_differ_from_the_doctypes() {
        let plan = plan()
            .with_field_language(
                &name("body"),
                Language::new("ur").unwrap_or_else(|e| panic!("{e}")),
            )
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            plan.field_language(&name("body")).map(Language::as_str),
            Ok("ur")
        );
        assert!(plan.terms_sql().contains("to_tsvector('simple'::regconfig"));
        assert!(
            plan.terms_sql()
                .contains("to_tsvector('english'::regconfig")
        );
        assert!(
            plan.with_field_language(
                &name("nope"),
                Language::new("en").unwrap_or_else(|e| panic!("{e}"))
            )
            .is_err()
        );
    }

    #[test]
    fn without_accent_folding_the_dictionary_is_not_used() {
        let plan = plan().without_accent_folding();
        assert!(!plan.folds_accents());
        assert!(!plan.terms_sql().contains("unaccent"));
        assert!(plan.terms_sql().contains("coalesce(\"title\"::text, '')"));
    }

    #[test]
    fn a_protected_searchable_field_is_refused() {
        let catalog = spot_storage_contract::contract_tests::search_catalog()
            .unwrap_or_else(|e| panic!("{e}"));
        let entity = catalog
            .entity(&name("listing"))
            .unwrap_or_else(|e| panic!("{e}"));
        let protection = Protection::new()
            .group_field(&name("listing"), &name("body"), "owner")
            .unwrap_or_else(|e| panic!("{e}"));
        let error = crate::fixture::refusal(ProjectionPlan::from_entity(
            entity,
            &Language::new("en").unwrap_or_else(|e| panic!("{e}")),
            &protection,
        ));
        assert_eq!(error.code(), "storage.invalid_schema");
        assert!(error.explanation().contains("body"));
        assert!(error.explanation().contains("owner"));
    }

    #[test]
    fn an_entity_without_a_search_declaration_has_no_projection() {
        let catalog =
            spot_storage_contract::contract_tests::geo_catalog().unwrap_or_else(|e| panic!("{e}"));
        let entity = catalog
            .entity(&name("place"))
            .unwrap_or_else(|e| panic!("{e}"));
        let plan = ProjectionPlan::from_entity(
            entity,
            &Language::new("en").unwrap_or_else(|e| panic!("{e}")),
            &Protection::new(),
        )
        .unwrap_or_else(|e| panic!("{e}"));
        assert!(plan.is_none());
    }

    #[test]
    fn the_insert_statement_writes_every_column_from_the_entity_row() {
        let sql = plan().upsert_sql(None);
        assert!(sql.starts_with("INSERT INTO \"spot_search_doc\" (\"doc_id\", \"doctype\", \"terms\", \"facets\", \"tenant_language\", \"updated_at\")"));
        assert!(sql.contains("$1::text"));
        assert!(sql.contains("$2::text"));
        assert!(sql.contains("FROM \"listing\""));
        assert!(sql.contains("ON CONFLICT (\"doc_id\") DO UPDATE SET"));
        assert!(!sql.contains(" WHERE "), "the bulk form projects every row");
        // Nothing is ever deleted by a write, and no value is ever formatted into the text.
        assert!(!sql.contains("DELETE"));
        assert!(
            sql.contains("'{}'::jsonb"),
            "a facet-less plan writes an empty object"
        );
        assert!(!sql.contains(";"), "one statement, never a batch");
    }

    #[test]
    fn the_single_row_form_filters_by_id() {
        let sql = plan().upsert_sql(Some("\"id\" = $3::uuid"));
        assert!(sql.contains("FROM \"listing\" WHERE \"id\" = $3::uuid ON CONFLICT"));
    }

    #[test]
    fn the_facet_object_quotes_its_keys_and_values() {
        let mut plan = plan();
        plan.facets = vec![name("price_band")];
        assert_eq!(
            plan.facets_sql(),
            "jsonb_build_object('price_band', \"price_band\")"
        );
        plan.facets.clear();
        assert_eq!(plan.facets_sql(), "'{}'::jsonb");
    }
}
