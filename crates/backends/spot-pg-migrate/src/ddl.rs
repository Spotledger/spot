//! DDL text for migration steps. Pure functions: every identifier is a validated storage name
//! or a name built by [`crate::names`], quoted; no value is interpolated except validated
//! numbers.

use spot_storage_contract::schema::system;
use spot_storage_contract::{
    EntityDef, EntityKind, FieldDef, FieldType, IndexMethod, Name, OpClass, RangeOf,
};

use crate::names::{length_check_name, pk_name, quote};

/// The PostgreSQL type of a column, spelled as `format_type` reports it.
pub fn column_type(ty: &FieldType) -> String {
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
        // ADR-087 section 4.3: the point is PostGIS `geometry`, schema-qualified so a changed
        // `search_path` cannot mean another type. `Geo { srid }` is validated to 4326.
        FieldType::Geo { .. } => GEO_POINT.to_owned(),
        // ADR-087 section 5: half-open ranges are the built-in range types.
        FieldType::Range { of } => match of {
            RangeOf::Int => "int8range",
            RangeOf::Decimal => "numrange",
            RangeOf::Date => "daterange",
            RangeOf::DateTime => "tstzrange",
        }
        .to_owned(),
    }
}

/// The column type of a geo field.
pub const GEO_POINT: &str = "public.geometry(Point,4326)";

/// `ty` as it is compared: without a `public.` schema qualifier and without blanks, so
/// `public.geometry(Point, 4326)` and `geometry(Point,4326)` are one type. `format_type` leaves
/// the qualifier off when the schema is on the `search_path` and puts it on otherwise; a second
/// plan against an unchanged database must be empty either way.
pub fn normalise_type(ty: &str) -> String {
    let rest = ty.trim();
    // Only a leading qualifier of the type name; `numeric(18,2)` and friends are untouched.
    let rest = rest.strip_prefix("public.").unwrap_or(rest);
    if rest.starts_with("geometry(") {
        rest.chars().filter(|c| !c.is_whitespace()).collect()
    } else {
        rest.to_owned()
    }
}

/// One column of an entity's table.
#[derive(Clone, Debug)]
pub struct Col {
    /// Column name.
    pub name: String,
    /// Type as `format_type` spells it.
    pub ty: String,
    /// Whether the column is `NOT NULL`.
    pub not_null: bool,
    /// Declared text columns use the byte-wise `"C"` collation.
    pub c_collation: bool,
    /// The field this column stores; `None` for a system column.
    pub field: Option<FieldDef>,
}

/// The columns the table of `entity` must have: system columns first, then the fields.
pub fn columns(entity: &EntityDef) -> Vec<Col> {
    let system_column = |name: &str, ty: &str, c_collation: bool| Col {
        name: name.to_owned(),
        ty: ty.to_owned(),
        not_null: true,
        c_collation,
        field: None,
    };
    let mut out = vec![system_column(system::ID, "uuid", false)];
    match entity.kind {
        EntityKind::Document => out.push(system_column(system::VERSION, "bigint", false)),
        EntityKind::Child { .. } => {
            out.push(system_column(system::PARENT, "uuid", false));
            out.push(system_column(system::PARENT_FIELD, "text", true));
            out.push(system_column(system::IDX, "integer", false));
        }
    }
    out.extend(entity.fields.iter().map(field_column));
    out
}

/// The column of one declared field.
pub fn field_column(field: &FieldDef) -> Col {
    Col {
        name: field.name.as_str().to_owned(),
        ty: column_type(&field.ty),
        not_null: !field.nullable,
        c_collation: matches!(field.ty, FieldType::Text { .. }),
        field: Some(field.clone()),
    }
}

/// `"name" type [COLLATE "C"] [NOT NULL]`.
pub fn column_definition(column: &Col, force_nullable: bool) -> String {
    let mut sql = format!("{} {}", quote(&column.name), column.ty);
    if column.c_collation {
        sql.push_str(" COLLATE \"C\"");
    }
    if column.not_null && !force_nullable {
        sql.push_str(" NOT NULL");
    }
    sql
}

/// The text limit of a field, if it has one.
pub fn max_chars(field: &FieldDef) -> Option<u32> {
    match field.ty {
        FieldType::Text { max_chars } => max_chars,
        _ => None,
    }
}

/// `CONSTRAINT .. CHECK (char_length(..) <= n)`.
pub fn length_check(table: &Name, field: &Name, max: u32) -> String {
    format!(
        "CONSTRAINT {} CHECK (char_length({}) <= {max})",
        quote(&length_check_name(table, field)),
        quote(field.as_str())
    )
}

/// `CREATE TABLE` for an entity, with its primary key and length checks.
pub fn create_table(entity: &EntityDef) -> String {
    let mut parts: Vec<String> = columns(entity)
        .iter()
        .map(|c| column_definition(c, false))
        .collect();
    parts.push(format!(
        "CONSTRAINT {} PRIMARY KEY ({})",
        quote(&pk_name(&entity.name)),
        quote(system::ID)
    ));
    for field in &entity.fields {
        if let Some(max) = max_chars(field) {
            parts.push(length_check(&entity.name, &field.name, max));
        }
    }
    format!(
        "CREATE TABLE {} ({})",
        quote(entity.name.as_str()),
        parts.join(", ")
    )
}

/// A deferred, non-cascading foreign key to `target(id)`; `not_valid` skips the existing rows
/// (validated by a separate step).
pub fn add_foreign_key(
    table: &Name,
    column: &str,
    target: &Name,
    fk: &str,
    not_valid: bool,
) -> String {
    format!(
        "ALTER TABLE {} ADD CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({}) \
         DEFERRABLE INITIALLY DEFERRED{}",
        quote(table.as_str()),
        quote(fk),
        quote(column),
        quote(target.as_str()),
        quote(system::ID),
        if not_valid { " NOT VALID" } else { "" }
    )
}

/// What `CREATE INDEX` is asked to build beyond the table and the key columns.
#[derive(Clone, Copy, Debug)]
pub struct IndexShape<'a> {
    /// Whether keys must be unique.
    pub unique: bool,
    /// The index method.
    pub method: IndexMethod,
    /// The operator class, applied to every key column (the contract allows one only on a
    /// single-column index for the trigram classes).
    pub opclass: Option<OpClass>,
    /// Columns stored with the key (`INCLUDE`).
    pub include: &'a [&'a str],
}

/// `CREATE [UNIQUE] INDEX [CONCURRENTLY] name ON table [USING method] (cols) [INCLUDE (..)]`.
///
/// `GistScalar` names no class in the statement: with `btree_gist` installed every scalar type
/// has a default GiST class, and naming one per type would repeat the extension's own table.
pub fn create_index(
    name: &str,
    table: &Name,
    fields: &[&str],
    shape: IndexShape<'_>,
    concurrently: bool,
) -> String {
    let class = match shape.opclass {
        Some(OpClass::GistTrgm) => " gist_trgm_ops",
        Some(OpClass::GinTrgm) => " gin_trgm_ops",
        Some(OpClass::GistScalar) | None => "",
    };
    let columns: Vec<String> = fields
        .iter()
        .map(|f| format!("{}{class}", quote(f)))
        .collect();
    let using = match shape.method {
        IndexMethod::BTree => "",
        IndexMethod::Gist => " USING gist",
        IndexMethod::Gin => " USING gin",
    };
    let include = if shape.include.is_empty() {
        String::new()
    } else {
        let cols: Vec<String> = shape.include.iter().map(|f| quote(f)).collect();
        format!(" INCLUDE ({})", cols.join(", "))
    };
    format!(
        "CREATE {}INDEX {}{} ON {}{using} ({}){include}",
        if shape.unique { "UNIQUE " } else { "" },
        if concurrently { "CONCURRENTLY " } else { "" },
        quote(name),
        quote(table.as_str()),
        columns.join(", ")
    )
}

/// `ALTER TABLE .. ADD CONSTRAINT .. EXCLUDE USING gist (..)`.
pub fn add_exclusion(x: &crate::plan::ExclusionConstraint) -> String {
    let elements: Vec<String> = x
        .elements
        .iter()
        .map(|e| format!("{} WITH {}", quote(&e.column), e.operator))
        .collect();
    let predicate = x
        .predicate()
        .map_or_else(String::new, |p| format!(" WHERE ({p})"));
    format!(
        "ALTER TABLE {} ADD CONSTRAINT {} EXCLUDE USING gist ({}){predicate}",
        quote(&x.table),
        quote(&x.name),
        elements.join(", ")
    )
}
