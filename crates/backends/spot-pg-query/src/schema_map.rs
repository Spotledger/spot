//! Mapping from storage names to PostgreSQL identifiers, and the collation naming rule.

use spot_storage_contract::{Collation, Name};

/// The search projection table (ADR-087 section 7.3), created by `spot-pg-search`
/// (WP-STO-13.T1). It is fixed by the ADR, so it is not part of the [`SchemaMap`].
pub const SEARCH_TABLE: &str = "spot_search_doc";
/// The projection's column holding the record id the row describes.
pub const SEARCH_DOC_ID: &str = "doc_id";
/// The projection's column holding the entity (DocType) name.
pub const SEARCH_DOCTYPE: &str = "doctype";
/// The projection's weighted `tsvector` column.
pub const SEARCH_TERMS: &str = "terms";

/// Decides which table and column hold an entity and its fields. WP-STO-03 uses
/// [`DefaultSchemaMap`] unless it has to change it; this trait is the single place of that
/// decision. The compiler quotes every identifier it receives, so a map cannot inject SQL, but
/// it must return identifiers PostgreSQL accepts (non-empty, at most 63 bytes, no NUL).
pub trait SchemaMap {
    /// The table of `entity`.
    fn table(&self, entity: &Name) -> String;
    /// The column of `field` (a declared field or a system column) of `entity`.
    fn column(&self, entity: &Name, field: &Name) -> String;
}

/// Table = entity name, column = field name, in the database's default schema (one tenant is
/// one database, ADR-34). System columns (`id`, `version`, `parent`, `parent_field`, `idx`)
/// keep their names.
#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultSchemaMap;

impl SchemaMap for DefaultSchemaMap {
    fn table(&self, entity: &Name) -> String {
        entity.as_str().to_owned()
    }

    fn column(&self, _entity: &Name, field: &Name) -> String {
        field.as_str().to_owned()
    }
}

/// The SQL name of the ICU collation for a tenant's [`Collation`]: `spot_` followed by the
/// locale in lowercase with `-` replaced by `_`, for example `en` → `spot_en` and
/// `ur-PK` → `spot_ur_pk`.
///
/// WP-STO-03 creates it with
/// `CREATE COLLATION spot_en (provider = icu, locale = 'en', deterministic = true)`. The
/// collation must be **deterministic** (PostgreSQL's default): then values that compare equal
/// under it are byte-identical and ties break by code point, as the contract requires.
pub fn collation_name(collation: &Collation) -> String {
    let locale: String = collation
        .as_str()
        .chars()
        .map(|c| {
            if c == '-' {
                '_'
            } else {
                c.to_ascii_lowercase()
            }
        })
        .collect();
    format!("spot_{locale}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collation_names_follow_the_locale() {
        for (locale, want) in [
            ("en", "spot_en"),
            ("und", "spot_und"),
            ("ur-PK", "spot_ur_pk"),
            ("sr-Latn-RS", "spot_sr_latn_rs"),
        ] {
            let c = Collation::new(locale).unwrap_or_else(|e| panic!("{e}"));
            assert_eq!(collation_name(&c), want);
        }
    }
}
