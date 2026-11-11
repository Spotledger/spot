//! The DDL statements `prepare` sends. Pure functions from the catalog to SQL text: every
//! identifier comes from a validated [`Name`] (or a name built by [`super::names`]) and is quoted;
//! no value is ever interpolated (ADR-45). The few string literals inside generated function
//! bodies are such validated names.

use spot_storage_contract::schema::system;
use spot_storage_contract::{
    Collation, EntityDef, EntityKind, ExclusionDef, FieldDef, FieldType, IndexDef, IndexMethod,
    Name, OpClass, RangeOf,
};

use super::names::{collation_name, exclusion_name, length_check_name, pk_name, quote};

/// The PostgreSQL type of a column, spelled as `format_type` reports it, so the same text is used
/// to create a column and to compare it with an existing one.
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
        // ADR-087 section 4.3: PostGIS `geometry`, schema-qualified so a changed `search_path`
        // cannot mean another type. `Geo { srid }` is validated to 4326. The same text as
        // `spot-pg-migrate`'s `GEO_POINT`.
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
pub(crate) const GEO_POINT: &str = "public.geometry(Point,4326)";

/// `ty` as it is compared: without a `public.` schema qualifier and without blanks, so
/// `public.geometry(Point, 4326)` and `geometry(Point,4326)` are one type. `format_type` leaves
/// the qualifier off when the schema is on the `search_path` and puts it on otherwise; a second
/// `prepare` against an unchanged database must change nothing either way. Same rule as
/// `spot-pg-migrate`'s `normalise_type`.
pub(crate) fn normalise_type(ty: &str) -> String {
    let rest = ty.trim();
    // Only a leading qualifier of the type name; `numeric(18,2)` and friends are untouched.
    let rest = rest.strip_prefix("public.").unwrap_or(rest);
    if rest.starts_with("geometry(") {
        rest.chars().filter(|c| !c.is_whitespace()).collect()
    } else {
        rest.to_owned()
    }
}

/// One column of an entity's table: its name, type text and nullability. System columns come
/// first, in a fixed order, then the declared fields.
pub(crate) struct ColumnSpec {
    pub(crate) name: String,
    pub(crate) ty: String,
    pub(crate) not_null: bool,
    /// Declared text fields use the byte-wise `"C"` collation: equality, grouping and unique
    /// indexes are exact, and queries add the ICU collation where the contract orders by it.
    pub(crate) c_collation: bool,
    /// The field this column stores, if it is not a system column.
    pub(crate) field: Option<FieldDef>,
}

/// The columns the table of `entity` must have.
pub(crate) fn columns(entity: &EntityDef) -> Vec<ColumnSpec> {
    let system_column = |name: &str, ty: &str, c_collation: bool| ColumnSpec {
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
pub(crate) fn field_column(field: &FieldDef) -> ColumnSpec {
    ColumnSpec {
        name: field.name.as_str().to_owned(),
        ty: column_type(&field.ty),
        not_null: !field.nullable,
        c_collation: matches!(field.ty, FieldType::Text { .. }),
        field: Some(field.clone()),
    }
}

/// `"name" type [COLLATE "C"] [NOT NULL]`.
pub(crate) fn column_definition(column: &ColumnSpec) -> String {
    let mut sql = format!("{} {}", quote(&column.name), column.ty);
    if column.c_collation {
        sql.push_str(" COLLATE \"C\"");
    }
    if column.not_null {
        sql.push_str(" NOT NULL");
    }
    sql
}

/// `CHECK` limiting a text column to `max_chars` characters.
pub(crate) fn length_check(table: &Name, field: &Name, max_chars: u32) -> String {
    format!(
        "CONSTRAINT {} CHECK (char_length({}) <= {max_chars})",
        quote(&length_check_name(table, field)),
        quote(field.as_str())
    )
}

/// The text limit of a field, if it has one.
pub(crate) fn max_chars(field: &FieldDef) -> Option<u32> {
    match field.ty {
        FieldType::Text { max_chars } => max_chars,
        _ => None,
    }
}

/// `CREATE TABLE` for an entity, with its primary key and text length checks. Foreign keys and
/// indexes are added afterwards, once every table exists.
pub(crate) fn create_table(entity: &EntityDef) -> String {
    let mut parts: Vec<String> = columns(entity).iter().map(column_definition).collect();
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

/// `CREATE COLLATION IF NOT EXISTS`: deterministic ICU, so values that compare equal are
/// byte-identical and ties break by code point (the contract's total order). The locale is a
/// validated `Collation` (letters, digits and `-`), quoted as a literal all the same.
pub(crate) fn create_collation(collation: &Collation) -> String {
    format!(
        "CREATE COLLATION IF NOT EXISTS {} (provider = icu, locale = '{}', deterministic = true)",
        quote(&collation_name(collation)),
        collation.as_str().replace('\'', "''")
    )
}

/// `ALTER TABLE ... ADD CONSTRAINT` for a deferred foreign key to `target(id)`. Not cascading: a
/// delete of a linked record fails, it never deletes the linking rows.
pub(crate) fn add_foreign_key(table: &Name, column: &str, target: &Name, fk: &str) -> String {
    format!(
        "ALTER TABLE {} ADD CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({}) \
         DEFERRABLE INITIALLY DEFERRED",
        quote(table.as_str()),
        quote(fk),
        quote(column),
        quote(target.as_str()),
        quote(system::ID)
    )
}

/// The operator class named in an index statement: `GistScalar` names none, because with
/// `btree_gist` installed every scalar type has a default GiST class.
pub(crate) fn opclass_sql(opclass: Option<OpClass>) -> &'static str {
    match opclass {
        Some(OpClass::GistTrgm) => " gist_trgm_ops",
        Some(OpClass::GinTrgm) => " gin_trgm_ops",
        Some(OpClass::GistScalar) | None => "",
    }
}

/// `CREATE [UNIQUE] INDEX name ON table [USING method] (cols) [INCLUDE (..)]` for a declared
/// index called `name` in the database.
pub(crate) fn create_index(name: &str, table: &Name, index: &IndexDef) -> String {
    let class = opclass_sql(index.opclass);
    let columns: Vec<String> = index
        .fields
        .iter()
        .map(|f| format!("{}{class}", quote(f.as_str())))
        .collect();
    let using = match index.method {
        IndexMethod::BTree => "",
        IndexMethod::Gist => " USING gist",
        IndexMethod::Gin => " USING gin",
    };
    let include = if index.include.is_empty() {
        String::new()
    } else {
        let cols: Vec<String> = index.include.iter().map(|f| quote(f.as_str())).collect();
        format!(" INCLUDE ({})", cols.join(", "))
    };
    format!(
        "CREATE {}INDEX {} ON {}{using} ({}){include}",
        if index.unique { "UNIQUE " } else { "" },
        quote(name),
        quote(table.as_str()),
        columns.join(", ")
    )
}

/// The `(column, operator)` elements of an exclusion: scope columns with `=`, the range last
/// with `&&`.
pub(crate) fn exclusion_elements(def: &ExclusionDef) -> Vec<(String, &'static str)> {
    let mut elements: Vec<(String, &'static str)> = def
        .scope
        .iter()
        .map(|s| (s.as_str().to_owned(), "="))
        .collect();
    elements.push((def.field.as_str().to_owned(), "&&"));
    elements
}

/// `pg_get_constraintdef` text reduced to what must match: no quotes, no blanks, lower case.
pub(crate) fn comparable_definition(text: &str) -> String {
    text.chars()
        .filter(|c| *c != '"' && !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

/// The `EXCLUDE USING gist (..)` definition of `def`, as [`comparable_definition`] spells it.
pub(crate) fn exclusion_comparable(def: &ExclusionDef) -> String {
    let elements: Vec<String> = exclusion_elements(def)
        .iter()
        .map(|(column, operator)| format!("{column} WITH {operator}"))
        .collect();
    comparable_definition(&format!("EXCLUDE USING gist ({})", elements.join(", ")))
}

/// `ALTER TABLE .. ADD CONSTRAINT .. EXCLUDE USING gist (scope WITH =, range WITH &&)`. Not
/// deferrable: a conflict fails the statement that causes it.
pub(crate) fn add_exclusion(table: &Name, def: &ExclusionDef) -> String {
    let elements: Vec<String> = exclusion_elements(def)
        .iter()
        .map(|(column, operator)| format!("{} WITH {operator}", quote(column)))
        .collect();
    format!(
        "ALTER TABLE {} ADD CONSTRAINT {} EXCLUDE USING gist ({})",
        quote(table.as_str()),
        quote(&exclusion_name(table, &def.name)),
        elements.join(", ")
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use spot_storage_contract::{LinkCheck, LinkTarget};

    use super::*;

    fn n(s: &str) -> Name {
        Name::new(s).unwrap()
    }

    #[test]
    fn a_document_table_has_system_columns_typed_fields_and_limits() {
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
                FieldDef {
                    name: n("boss"),
                    ty: FieldType::Link {
                        target: LinkTarget::Entity(n("party")),
                        check: LinkCheck::Database,
                    },
                    nullable: true,
                },
            ],
            children: vec![],
            indexes: vec![],
            exclusions: vec![],
            search: None,
        };
        let sql = create_table(&entity);
        assert_eq!(
            sql,
            "CREATE TABLE \"party\" (\"id\" uuid NOT NULL, \"version\" bigint NOT NULL, \
             \"name\" text COLLATE \"C\" NOT NULL, \"limit\" numeric(18,2), \"boss\" uuid, \
             CONSTRAINT \"party_pkey\" PRIMARY KEY (\"id\"), \
             CONSTRAINT \"party_name_len\" CHECK (char_length(\"name\") <= 140))"
        );
    }

    #[test]
    fn a_child_table_has_parent_columns_and_no_version() {
        let entity = EntityDef {
            name: n("line"),
            kind: EntityKind::Child {
                parent: n("invoice"),
            },
            fields: vec![],
            children: vec![],
            indexes: vec![],
            exclusions: vec![],
            search: None,
        };
        let sql = create_table(&entity);
        assert!(sql.contains("\"parent\" uuid NOT NULL, \"parent_field\" text COLLATE \"C\" NOT NULL, \"idx\" integer NOT NULL"));
        assert!(!sql.contains("version"));
    }

    #[test]
    fn geo_and_range_columns_use_the_types_spot_pg_migrate_plans() {
        let geo = column_type(&FieldType::Geo { srid: 4326 });
        assert_eq!(geo, "public.geometry(Point,4326)");
        assert_eq!(
            [
                RangeOf::Int,
                RangeOf::Decimal,
                RangeOf::Date,
                RangeOf::DateTime
            ]
            .map(|of| column_type(&FieldType::Range { of })),
            ["int8range", "numrange", "daterange", "tstzrange"]
        );
        // `format_type` leaves the qualifier off on the search path and may add blanks.
        for spelled in [
            "geometry(Point,4326)",
            "public.geometry(Point, 4326)",
            " geometry(Point,4326) ",
        ] {
            assert_eq!(normalise_type(spelled), normalise_type(&geo), "{spelled}");
        }
        assert_eq!(normalise_type("numeric(18,2)"), "numeric(18,2)");
    }

    #[test]
    fn an_exclusion_is_scope_equality_then_range_overlap() {
        let def = ExclusionDef {
            name: n("unit_during"),
            field: n("during"),
            scope: vec![n("unit")],
            message: "taken".into(),
        };
        assert_eq!(
            add_exclusion(&n("occupancy"), &def),
            "ALTER TABLE \"occupancy\" ADD CONSTRAINT \"spot_occupancy_unit_during_excl\" \
             EXCLUDE USING gist (\"unit\" WITH =, \"during\" WITH &&)"
        );
        assert_eq!(
            exclusion_comparable(&def),
            comparable_definition("EXCLUDE USING gist (unit WITH =, during WITH &&)")
        );
        let single = ExclusionDef {
            scope: vec![],
            ..def
        };
        assert!(add_exclusion(&n("occupancy"), &single).ends_with("(\"during\" WITH &&)"));
    }

    #[test]
    fn index_statements_name_the_method_class_and_included_columns() {
        let mut index = IndexDef::btree(n("ix"), vec![n("a"), n("b")], false);
        assert_eq!(
            create_index("ix", &n("t"), &index),
            "CREATE INDEX \"ix\" ON \"t\" (\"a\", \"b\")"
        );
        index.method = IndexMethod::Gin;
        index.opclass = Some(OpClass::GinTrgm);
        index.fields = vec![n("a")];
        index.include = vec![n("b")];
        assert_eq!(
            create_index("spot_t_ix_ginx", &n("t"), &index),
            "CREATE INDEX \"spot_t_ix_ginx\" ON \"t\" USING gin (\"a\" gin_trgm_ops) INCLUDE (\"b\")"
        );
    }

    #[test]
    fn collation_is_deterministic_icu() {
        let sql = create_collation(&Collation::new("ur-PK").unwrap());
        assert_eq!(
            sql,
            "CREATE COLLATION IF NOT EXISTS \"spot_ur_pk\" (provider = icu, locale = 'ur-PK', deterministic = true)"
        );
    }

    #[test]
    fn foreign_keys_are_deferred_and_never_cascade() {
        let sql = add_foreign_key(&n("invoice"), "party", &n("party"), "invoice_party_fkey");
        assert!(sql.ends_with("DEFERRABLE INITIALLY DEFERRED"));
        assert!(!sql.to_uppercase().contains("CASCADE"));
    }
}
