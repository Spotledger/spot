//! The scratch schema's DDL: the catalogue's tables, text for text as the backend creates them.
//!
//! A scratch side holds the same tables `StorageBackend::prepare` creates in the tenant database
//! (`spot-postgres`'s `schema::ddl::create_table`), so an image stored into the scratch and the
//! same image in the live database are read back by the same select list and digest to the same
//! bytes. That module is private to its crate, so the rules are reproduced here and this module is
//! the one place that decides them:
//!
//! * system columns first — `id uuid` for every entity, `version bigint` for a document and
//!   `parent uuid`, `parent_field text`, `idx integer` for a child table;
//! * then the declared fields, each with its declared type, `COLLATE "C"` when it is text, and
//!   `NOT NULL` when it is not nullable;
//! * the primary key on `id`, named `<table>_pkey` and bounded to PostgreSQL's 63 bytes;
//! * a `CHECK (char_length(..) <= n)` per text field that declares a maximum.
//!
//! Foreign keys, indexes beyond the primary key, exclusions and search projections are **not**
//! created: the scratch is a compare target written in one pass from a journal whose records are
//! already the final state (ADR-061 s6), never a serving database, and a deferred foreign key it
//! does not need would only make the order of images matter.

use spot_storage_contract::schema::system;
use spot_storage_contract::{EntityDef, EntityKind, FieldType, Name};

/// PostgreSQL's identifier limit in bytes.
const MAX_IDENT: usize = 63;

/// `name` as a quoted identifier. Storage names are lowercase letters, digits and underscores
/// (validated by [`Name`]), so quoting is for reserved words only; a scratch schema name is built
/// from the same alphabet.
pub(crate) fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// `text`, cut to PostgreSQL's identifier limit; a longer name keeps its first 53 bytes, `_` and
/// eight hex digits of an FNV-1a hash of the whole name, as the backend's `names::bounded` does.
pub(crate) fn bounded(text: &str) -> String {
    if text.len() <= MAX_IDENT {
        return text.to_owned();
    }
    let mut hash: u32 = 0x811c_9dc5;
    for byte in text.bytes() {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    let head: String = text.chars().take(MAX_IDENT - 9).collect();
    format!("{head}_{hash:08x}")
}

/// The primary key constraint of an entity's table.
pub(crate) fn pk_name(table: &Name) -> String {
    bounded(&format!("{table}_pkey"))
}

/// The check constraint limiting a text column's length.
pub(crate) fn length_check_name(table: &Name, field: &Name) -> String {
    bounded(&format!("{table}_{field}_len"))
}

/// The PostgreSQL type of a column, spelled as the backend spells it.
pub(crate) fn column_type(ty: &FieldType) -> String {
    match ty {
        FieldType::Bool => "boolean".to_owned(),
        FieldType::Int => "bigint".to_owned(),
        FieldType::Decimal { precision, scale } => format!("numeric({precision},{scale})"),
        FieldType::Text { .. } => "text".to_owned(),
        FieldType::Date => "date".to_owned(),
        FieldType::Time => "time without time zone".to_owned(),
        FieldType::DateTime => "timestamp with time zone".to_owned(),
        FieldType::Link { .. } => "uuid".to_owned(),
        FieldType::Json => "jsonb".to_owned(),
        FieldType::Bytes => "bytea".to_owned(),
        FieldType::Geo { .. } => "public.geometry(Point,4326)".to_owned(),
        FieldType::Range { of } => match of {
            spot_storage_contract::RangeOf::Int => "int8range",
            spot_storage_contract::RangeOf::Decimal => "numrange",
            spot_storage_contract::RangeOf::Date => "daterange",
            spot_storage_contract::RangeOf::DateTime => "tstzrange",
        }
        .to_owned(),
    }
}

/// The text limit of a field, when it declares one.
fn max_chars(field: &spot_storage_contract::FieldDef) -> Option<u32> {
    match field.ty {
        FieldType::Text { max_chars } => max_chars,
        _ => None,
    }
}

/// `CREATE TABLE` for one entity: its columns, its primary key and its text length checks. The
/// table name is unqualified, so the statement lands in the schema the caller put on the
/// transaction's `search_path`.
pub(crate) fn create_table(entity: &EntityDef) -> String {
    let mut parts = vec![format!("{} uuid NOT NULL", quote(system::ID))];
    match entity.kind {
        EntityKind::Document => parts.push(format!("{} bigint NOT NULL", quote(system::VERSION))),
        EntityKind::Child { .. } => {
            parts.push(format!("{} uuid NOT NULL", quote(system::PARENT)));
            parts.push(format!(
                "{} text COLLATE \"C\" NOT NULL",
                quote(system::PARENT_FIELD)
            ));
            parts.push(format!("{} integer NOT NULL", quote(system::IDX)));
        }
    }
    for field in &entity.fields {
        let mut column = format!("{} {}", quote(field.name.as_str()), column_type(&field.ty));
        if matches!(field.ty, FieldType::Text { .. }) {
            column.push_str(" COLLATE \"C\"");
        }
        if !field.nullable {
            column.push_str(" NOT NULL");
        }
        parts.push(column);
    }
    parts.push(format!(
        "CONSTRAINT {} PRIMARY KEY ({})",
        quote(&pk_name(&entity.name)),
        quote(system::ID)
    ));
    for field in &entity.fields {
        if let Some(max) = max_chars(field) {
            parts.push(format!(
                "CONSTRAINT {} CHECK (char_length({}) <= {max})",
                quote(&length_check_name(&entity.name, &field.name)),
                quote(field.name.as_str()),
            ));
        }
    }
    format!(
        "CREATE TABLE {} ({})",
        quote(entity.name.as_str()),
        parts.join(", ")
    )
}

/// A `'text'` literal with its quotes escaped; used for the schema's own comment only.
pub(crate) fn literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use spot_storage_contract::{ChildTableDef, EntityKind, FieldDef};

    use super::*;

    fn n(name: &str) -> Name {
        Name::new(name).unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn a_document_table_has_the_backends_shape() {
        let entity = EntityDef {
            name: n("party"),
            kind: EntityKind::Document,
            fields: vec![
                FieldDef {
                    name: n("name"),
                    ty: FieldType::Text {
                        max_chars: Some(140),
                    },
                    nullable: false,
                },
                FieldDef {
                    name: n("limit"),
                    ty: FieldType::Decimal {
                        precision: 18,
                        scale: 2,
                    },
                    nullable: true,
                },
            ],
            children: vec![ChildTableDef {
                field: n("lines"),
                entity: n("party_line"),
            }],
            indexes: vec![],
            exclusions: vec![],
            search: None,
        };
        assert_eq!(
            create_table(&entity),
            "CREATE TABLE \"party\" (\"id\" uuid NOT NULL, \"version\" bigint NOT NULL, \
             \"name\" text COLLATE \"C\" NOT NULL, \"limit\" numeric(18,2), \
             CONSTRAINT \"party_pkey\" PRIMARY KEY (\"id\"), \
             CONSTRAINT \"party_name_len\" CHECK (char_length(\"name\") <= 140))"
        );
    }

    #[test]
    fn a_child_table_has_parent_columns_and_no_version() {
        let entity = EntityDef {
            name: n("party_line"),
            kind: EntityKind::Child { parent: n("party") },
            fields: vec![],
            children: vec![],
            indexes: vec![],
            exclusions: vec![],
            search: None,
        };
        let sql = create_table(&entity);
        assert!(sql.contains(
            "\"parent\" uuid NOT NULL, \"parent_field\" text COLLATE \"C\" NOT NULL, \"idx\" integer NOT NULL"
        ));
        assert!(!sql.contains("version"));
    }

    #[test]
    fn a_long_primary_key_name_is_bounded_and_stable() {
        let long = n(&"a".repeat(60));
        let name = pk_name(&long);
        assert!(name.len() <= MAX_IDENT);
        assert_eq!(name, pk_name(&long));
        assert_eq!(pk_name(&n("party")), "party_pkey");
    }

    #[test]
    fn a_literal_escapes_its_quotes() {
        assert_eq!(literal("a'b"), "'a''b'");
    }
}
