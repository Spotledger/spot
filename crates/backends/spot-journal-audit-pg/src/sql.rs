//! The statements and the read plan of a side: pure functions from a catalogue to SQL text.
//!
//! A side is a *reader* of one scope: the tenant's own tables (`schema: None`, the tables
//! `prepare` creates there) or one scratch schema. Everything in this module is a `SELECT`, or a
//! statement the scratch side writes with; the live side only ever sends the `SELECT`s, and the
//! unit tests at the bottom pin that (ADR-045's "the live read is a read").
//!
//! # Values never reach the SQL text
//!
//! Every value is a `$n` parameter, cast explicitly (`$1::text::uuid`), so the driver's declared
//! parameter type never has to be guessed from context. Identifiers come from validated storage
//! names and are always quoted.

use std::sync::Arc;

use spot_journal_audit::{Amount, Fingerprint};
use spot_storage_contract::{Catalog, EntityDef, EntityKind, FieldDef, FieldType, Name};

use crate::ddl::{column_type, quote};
use crate::error::{PgSideError, options_invalid};

/// How a declared fingerprint's amount is written: integer minor units or an exact decimal
/// (ADR-12 forbids a float for money, so there is no third form).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AmountForm {
    /// The summed column is an integer count of the currency's minor unit.
    Minor,
    /// The summed column is an exact decimal; its text is parsed into a canonical decimal.
    Decimal,
}

/// One financial fingerprint the sides compute: a declared sum over one entity, grouped by its
/// keys (WP-MIG-06's engine, generic over declarations so no ERP DocType name lives here).
///
/// The key of a value is the grouped columns' text joined by `us|` — the unit separator — so two
/// rows never share a key unless every key column agrees.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FingerprintQuery {
    /// The set's name, as the ledger declares it (`trial_balance`, `stock_balance`,
    /// `ar_ageing`, `ap_ageing`).
    pub name: String,
    /// The entity summed.
    pub entity: String,
    /// The columns the sum is grouped by, in declaration order.
    pub keys: Vec<String>,
    /// The column summed.
    pub amount: String,
    /// Whether the amount column holds minor units or an exact decimal.
    pub form: AmountForm,
}

impl FingerprintQuery {
    fn check(&self) -> Result<(), PgSideError> {
        if self.name.trim().is_empty() {
            return Err(options_invalid("a fingerprint declaration has no name"));
        }
        if self.entity.trim().is_empty() {
            return Err(options_invalid("a fingerprint declaration names no entity"));
        }
        if self.keys.is_empty() || self.keys.iter().any(|k| k.trim().is_empty()) {
            return Err(options_invalid(
                "a fingerprint declaration needs at least one non-empty key column",
            ));
        }
        if self.amount.trim().is_empty() {
            return Err(options_invalid(
                "a fingerprint declaration names no amount column",
            ));
        }
        Ok(())
    }
}

/// What one side reads: where its tables are and which records they hold.
#[derive(Debug)]
pub(crate) struct Scope {
    /// `Some(schema)` for a scratch schema, `None` for the tenant's own tables.
    schema: Option<String>,
    /// The catalogue, so a child table can be resolved to its entity.
    catalog: Arc<Catalog>,
    /// The document entities in name order, derived entities excluded. A child table is part of
    /// its parent's body and is never a record of its own (it has no version, ADR-053).
    entities: Vec<EntityDef>,
    /// Rows per keyset page.
    page_size: i64,
    /// The fingerprint declarations this scope can answer.
    fingerprints: Vec<FingerprintQuery>,
}

impl Scope {
    /// The read plan of one side, or a refusal when there is nothing to read.
    pub(crate) fn new(
        catalog: &Arc<Catalog>,
        schema: Option<String>,
        derived: &spot_journal_audit::DerivedEntities,
        page_size: i64,
        fingerprints: Vec<FingerprintQuery>,
    ) -> Result<Self, PgSideError> {
        if page_size < 1 {
            return Err(options_invalid("the page size must be at least one row"));
        }
        let entities: Vec<EntityDef> = catalog
            .entities()
            .filter(|e| matches!(e.kind, EntityKind::Document))
            .filter(|e| !derived.contains(e.name.as_str()))
            .cloned()
            .collect();
        if entities.is_empty() {
            return Err(options_invalid(
                "the catalogue declares no document entity to read",
            ));
        }
        for query in &fingerprints {
            query.check()?;
        }
        Ok(Self {
            schema,
            catalog: Arc::clone(catalog),
            entities,
            page_size,
            fingerprints,
        })
    }

    /// The document entities this scope reads, in name order.
    pub(crate) fn entities(&self) -> &[EntityDef] {
        &self.entities
    }

    /// Rows per page.
    pub(crate) fn page_size(&self) -> i64 {
        self.page_size
    }

    /// The entity called `name`, whether it is a document or a child table.
    pub(crate) fn child_entity(&self, name: &Name) -> Option<&EntityDef> {
        self.catalog.entity(name).ok()
    }

    /// The entity called `name`, when this scope reads it.
    pub(crate) fn entity(&self, name: &str) -> Option<&EntityDef> {
        self.entities.iter().find(|e| e.name.as_str() == name)
    }

    /// The declarations this scope can answer: those naming an entity it reads.
    pub(crate) fn fingerprints(&self) -> &[FingerprintQuery] {
        &self.fingerprints
    }

    /// `"schema"."table"`, or `"table"` for the tenant's own tables.
    pub(crate) fn table(&self, entity: &Name) -> String {
        match &self.schema {
            Some(schema) => format!("{}.{}", quote(schema), quote(entity.as_str())),
            None => quote(entity.as_str()),
        }
    }
}

/// The read plan a held session serves, swappable while the session stays open: a builder on an
/// opened side (`with_derived`, `with_fingerprints`) changes what the worker reads from the same
/// snapshot instead of reopening one.
#[derive(Clone, Debug)]
pub(crate) struct SharedScope(Arc<std::sync::Mutex<Arc<Scope>>>);

impl SharedScope {
    /// A shared plan around `scope`.
    pub(crate) fn new(scope: Arc<Scope>) -> Self {
        Self(Arc::new(std::sync::Mutex::new(scope)))
    }

    /// The plan as it is now.
    pub(crate) fn get(&self) -> Arc<Scope> {
        Arc::clone(
            &self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    /// Replaces the plan; the session's next request reads the new one.
    pub(crate) fn set(&self, scope: Arc<Scope>) {
        *self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = scope;
    }
}

/// One column of a stored value as ADR-055 writes it, as SQL text (see [`crate::encode`]).
fn field_expr(field: &FieldDef) -> String {
    let column = quote(field.name.as_str());
    match &field.ty {
        // RFC 3339 in UTC ending in `Z`, microseconds only when non-zero: `concat` would fold a
        // null into an empty string, so the null is spelled out.
        FieldType::DateTime => format!(
            "CASE WHEN {column} IS NULL THEN NULL ELSE concat(\
             to_char({column} AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS'), \
             rtrim(rtrim(to_char({column} AT TIME ZONE 'UTC', '.US'), '0'), '.'), 'Z') END"
        ),
        // PostgreSQL's base64 wraps at 76 characters; ADR-055's wire form does not.
        FieldType::Bytes => format!("translate(encode({column}, 'base64'), chr(10), '')"),
        // `::text` of a geometry is hex EWKB, not a point.
        FieldType::Geo { .. } => format!("public.ST_AsText({column})"),
        _ => format!("{column}::text"),
    }
}

/// The `SELECT` of one keyset page of documents of `entity`, resuming after `$1` (an id) with at
/// most `$2` rows.
pub(crate) fn document_page(scope: &Scope, entity: &EntityDef) -> String {
    let mut parts = vec![
        format!("{}::text", quote(spot_storage_contract::schema::system::ID)),
        format!(
            "{}::text",
            quote(spot_storage_contract::schema::system::VERSION)
        ),
    ];
    parts.extend(entity.fields.iter().map(field_expr));
    format!(
        "SELECT {} FROM {} WHERE {} > $1::text::uuid ORDER BY {} LIMIT $2",
        parts.join(", "),
        scope.table(&entity.name),
        quote(spot_storage_contract::schema::system::ID),
        quote(spot_storage_contract::schema::system::ID),
    )
}

/// The `SELECT` of every child row of the documents whose ids are `$1`, for the child entity
/// `child` reached through the parent's child table `field`.
pub(crate) fn child_page(scope: &Scope, child: &EntityDef) -> String {
    let mut parts = vec![
        format!(
            "{}::text",
            quote(spot_storage_contract::schema::system::PARENT)
        ),
        format!("{}::text", quote(spot_storage_contract::schema::system::ID)),
        format!(
            "{}::text",
            quote(spot_storage_contract::schema::system::IDX)
        ),
    ];
    parts.extend(child.fields.iter().map(field_expr));
    format!(
        "SELECT {} FROM {} WHERE {} = ANY($1::text[]::uuid[]) ORDER BY {}, {}",
        parts.join(", "),
        scope.table(&child.name),
        quote(spot_storage_contract::schema::system::PARENT),
        quote(spot_storage_contract::schema::system::PARENT),
        quote(spot_storage_contract::schema::system::IDX),
    )
}

/// The `SELECT` of one declared fingerprint: its key and its summed amount, both as text.
pub(crate) fn fingerprint(scope: &Scope, query: &FingerprintQuery) -> Result<String, PgSideError> {
    let name = Name::new(&query.entity)
        .map_err(|_| options_invalid("a fingerprint declaration names a non-storage entity"))?;
    let entity = scope
        .entity(name.as_str())
        .ok_or_else(|| options_invalid("a fingerprint declaration names an entity not read"))?;
    let declared = |field: &str, why: &str| -> Result<&FieldDef, PgSideError> {
        entity
            .field(&Name::new(field).map_err(|_| {
                options_invalid("a fingerprint declaration names a non-storage field")
            })?)
            .map_err(|_| options_invalid(why))
    };
    let mut keys = Vec::with_capacity(query.keys.len());
    for key in &query.keys {
        keys.push(format!(
            "{}::text",
            quote(
                declared(key, "a fingerprint key column is not a field of its entity")?
                    .name
                    .as_str()
            )
        ));
    }
    let amount = declared(
        &query.amount,
        "a fingerprint amount column is not a field of its entity",
    )?;
    if !matches!(
        amount.ty,
        FieldType::Decimal { .. } | FieldType::Int | FieldType::Link { .. }
    ) {
        return Err(options_invalid(
            "a fingerprint amount column must be a decimal, an integer or a link",
        ));
    }
    Ok(format!(
        "SELECT concat_ws(chr(31), {}), SUM({})::text FROM {} GROUP BY 1 ORDER BY 1",
        keys.join(", "),
        quote(amount.name.as_str()),
        scope.table(&entity.name),
    ))
}

/// One fingerprint built from its rows of `(key, amount text)`.
pub(crate) fn build_fingerprint(
    query: &FingerprintQuery,
    rows: Vec<(String, String)>,
) -> Result<Fingerprint, PgSideError> {
    let mut values = Vec::with_capacity(rows.len());
    for (key, amount) in rows {
        let amount = match query.form {
            AmountForm::Minor => {
                let units = amount.parse::<i64>().map_err(|_| {
                    options_invalid("a fingerprint's minor-unit sum is not an integer")
                })?;
                Amount::minor(units)
            }
            AmountForm::Decimal => Amount::decimal(&amount).map_err(|_| {
                options_invalid("a fingerprint's decimal sum is not an exact decimal")
            })?,
        };
        values.push((key, amount));
    }
    Fingerprint::new(query.name.clone(), values)
        .map_err(|_| options_invalid("a fingerprint's keys repeat or are empty"))
}

/// The `INSERT … ON CONFLICT` of one document image: `$1` id, `$2` version, then one parameter per
/// declared field in declaration order.
pub(crate) fn upsert_document(scope: &Scope, entity: &EntityDef) -> String {
    let mut columns = vec![
        quote(spot_storage_contract::schema::system::ID),
        quote(spot_storage_contract::schema::system::VERSION),
    ];
    let mut params = vec!["$1::text::uuid".to_owned(), "$2::text::bigint".to_owned()];
    for (at, field) in entity.fields.iter().enumerate() {
        columns.push(quote(field.name.as_str()));
        params.push(param_expr(&field.ty, at + 3));
    }
    let updates: Vec<String> = columns
        .iter()
        .skip(1)
        .map(|column| format!("{column} = EXCLUDED.{column}"))
        .collect();
    format!(
        "INSERT INTO {} ({}) VALUES ({}) ON CONFLICT ({}) DO UPDATE SET {}",
        scope.table(&entity.name),
        columns.join(", "),
        params.join(", "),
        quote(spot_storage_contract::schema::system::ID),
        updates.join(", "),
    )
}

/// The `INSERT` of one child row: `$1` id, `$2` parent, `$3` the parent's child-table name, `$4`
/// its 1-based position, then one parameter per declared field.
pub(crate) fn insert_child(scope: &Scope, child: &EntityDef) -> String {
    use spot_storage_contract::schema::system;
    let mut columns = vec![
        quote(system::ID),
        quote(system::PARENT),
        quote(system::PARENT_FIELD),
        quote(system::IDX),
    ];
    let mut params = vec![
        "$1::text::uuid".to_owned(),
        "$2::text::uuid".to_owned(),
        "$3::text".to_owned(),
        "$4::text::integer".to_owned(),
    ];
    for (at, field) in child.fields.iter().enumerate() {
        columns.push(quote(field.name.as_str()));
        params.push(param_expr(&field.ty, at + 5));
    }
    format!(
        "INSERT INTO {} ({}) VALUES ({})",
        scope.table(&child.name),
        columns.join(", "),
        params.join(", "),
    )
}

/// `DELETE FROM <table> WHERE <column> = $1::text::uuid`.
pub(crate) fn delete_where(scope: &Scope, table: &Name, column: &str) -> String {
    format!(
        "DELETE FROM {} WHERE {} = $1::text::uuid",
        scope.table(table),
        quote(column)
    )
}

/// `TRUNCATE` of every table of the scope, in one statement.
pub(crate) fn truncate_all(scope: &Scope, tables: &[Name]) -> Option<String> {
    if tables.is_empty() {
        return None;
    }
    let names: Vec<String> = tables.iter().map(|t| scope.table(t)).collect();
    Some(format!("TRUNCATE {}", names.join(", ")))
}

/// One stored value as the parameter the write statement binds: text, cast by the database to the
/// column's own type, so the database's own parsing decides whether a body value is storable.
fn param_expr(ty: &FieldType, at: usize) -> String {
    match ty {
        FieldType::Bytes => format!("decode(${at}::text, 'base64')"),
        FieldType::Geo { srid } => format!("public.ST_GeomFromText(${at}::text, {srid})"),
        other => format!("${at}::text::{}", column_type(other)),
    }
}

#[cfg(test)]
mod tests {
    use spot_storage_contract::{ChildTableDef, Collation, EntityDef, EntityKind, FieldDef, Name};

    use super::*;

    fn n(name: &str) -> Name {
        Name::new(name).unwrap_or_else(|e| panic!("{e}"))
    }

    fn field(name: &str, ty: FieldType) -> FieldDef {
        FieldDef {
            name: n(name),
            ty,
            nullable: true,
        }
    }

    fn catalog() -> Arc<Catalog> {
        let entities = vec![
            EntityDef {
                name: n("invoice"),
                kind: EntityKind::Document,
                fields: vec![
                    field("party", FieldType::Text { max_chars: None }),
                    field(
                        "total",
                        FieldType::Decimal {
                            precision: 18,
                            scale: 2,
                        },
                    ),
                    field("at", FieldType::DateTime),
                    field("blob", FieldType::Bytes),
                ],
                children: vec![ChildTableDef {
                    field: n("lines"),
                    entity: n("invoice_line"),
                }],
                indexes: vec![],
                exclusions: vec![],
                search: None,
            },
            EntityDef {
                name: n("invoice_line"),
                kind: EntityKind::Child {
                    parent: n("invoice"),
                },
                fields: vec![field("qty", FieldType::Int)],
                children: vec![],
                indexes: vec![],
                exclusions: vec![],
                search: None,
            },
        ];
        Arc::new(
            Catalog::new(
                1,
                Collation::new("en").unwrap_or_else(|e| panic!("{e}")),
                entities,
            )
            .unwrap_or_else(|e| panic!("{e}")),
        )
    }

    fn scope(fingerprints: Vec<FingerprintQuery>) -> Scope {
        Scope::new(
            &catalog(),
            None,
            &spot_journal_audit::DerivedEntities::none(),
            10,
            fingerprints,
        )
        .unwrap_or_else(|e| panic!("{e}"))
    }

    /// Every reader statement is a read: no keyword that writes appears in it.
    #[test]
    fn the_reader_statements_never_write() {
        let catalog = catalog();
        let invoice = catalog
            .entity(&n("invoice"))
            .unwrap_or_else(|e| panic!("{e}"));
        let line = catalog
            .entity(&n("invoice_line"))
            .unwrap_or_else(|e| panic!("{e}"));
        let scope = scope(vec![FingerprintQuery {
            name: "trial_balance".to_owned(),
            entity: "invoice".to_owned(),
            keys: vec!["party".to_owned()],
            amount: "total".to_owned(),
            form: AmountForm::Decimal,
        }]);
        let statements = vec![
            document_page(&scope, invoice),
            child_page(&scope, line),
            fingerprint(&scope, &scope.fingerprints()[0]).unwrap_or_else(|e| panic!("{e}")),
            crate::live::HEAD_SQL.to_owned(),
        ];
        for sql in statements {
            let upper = sql.to_ascii_uppercase();
            assert!(upper.starts_with("SELECT"), "{sql}");
            for forbidden in [
                "INSERT", "UPDATE", "DELETE", "TRUNCATE", "DROP", "CREATE", "ALTER", "COMMENT",
                "GRANT", "SET ",
            ] {
                assert!(!upper.contains(forbidden), "{forbidden} in {sql}");
            }
        }
    }

    #[test]
    fn a_page_is_keyset_ordered_and_bounded() {
        let catalog = catalog();
        let invoice = catalog
            .entity(&n("invoice"))
            .unwrap_or_else(|e| panic!("{e}"));
        let page = document_page(&scope(vec![]), invoice);
        assert!(page.contains("WHERE \"id\" > $1::text::uuid ORDER BY \"id\" LIMIT $2"));
        assert!(page.starts_with("SELECT \"id\"::text, \"version\"::text"));
        // The instant is written in UTC ending in `Z`, with the fraction trimmed.
        assert!(page.contains("rtrim(rtrim(to_char(\"at\" AT TIME ZONE 'UTC', '.US'), '0'), '.')"));
        // Bytes are base64 without the driver's line wrapping.
        assert!(page.contains("translate(encode(\"blob\", 'base64'), chr(10), '')"));
    }

    #[test]
    fn a_scratch_statement_qualifies_its_table() {
        let catalog = catalog();
        let scope = Scope::new(
            &catalog,
            Some("spot_scratch_acme_run1".to_owned()),
            &spot_journal_audit::DerivedEntities::none(),
            10,
            vec![],
        )
        .unwrap_or_else(|e| panic!("{e}"));
        let invoice = catalog
            .entity(&n("invoice"))
            .unwrap_or_else(|e| panic!("{e}"));
        assert!(
            document_page(&scope, invoice).contains("FROM \"spot_scratch_acme_run1\".\"invoice\"")
        );
        let upsert = upsert_document(&scope, invoice);
        assert!(upsert.contains("ON CONFLICT (\"id\") DO UPDATE SET"));
        assert!(upsert.contains("$3::text::text"));
        assert!(upsert.contains("decode($6::text, 'base64')"));
    }

    #[test]
    fn a_derived_entity_is_not_read_and_an_empty_scope_is_refused() {
        let catalog = catalog();
        let derived = spot_journal_audit::DerivedEntities::from_names(["invoice"]);
        assert!(
            Scope::new(&catalog, None, &derived, 10, vec![])
                .err()
                .map(|e| e.code())
                == Some(crate::error::OPTIONS_INVALID)
        );
        let all = spot_journal_audit::DerivedEntities::from_names(["invoice", "invoice_line"]);
        assert!(Scope::new(&catalog, None, &all, 10, vec![]).is_err());
        assert!(
            Scope::new(
                &catalog,
                None,
                &spot_journal_audit::DerivedEntities::none(),
                0,
                vec![]
            )
            .is_err()
        );
    }

    #[test]
    fn a_fingerprint_declaration_is_checked_against_the_catalogue() {
        let unknown = FingerprintQuery {
            name: "trial_balance".to_owned(),
            entity: "missing".to_owned(),
            keys: vec!["party".to_owned()],
            amount: "total".to_owned(),
            form: AmountForm::Decimal,
        };
        let scope = scope(vec![]);
        assert!(fingerprint(&scope, &unknown).is_err());
        let good = FingerprintQuery {
            entity: "invoice".to_owned(),
            ..unknown.clone()
        };
        let sql = fingerprint(&scope, &good).unwrap_or_else(|e| panic!("{e}"));
        assert!(sql.contains("concat_ws(chr(31), \"party\"::text)"));
        assert!(sql.contains("SUM(\"total\")::text"));
        let built = build_fingerprint(&good, vec![("acme".to_owned(), "1234.50".to_owned())]);
        assert!(built.is_ok());
        let minor = FingerprintQuery {
            form: AmountForm::Minor,
            ..good
        };
        assert!(build_fingerprint(&minor, vec![("acme".to_owned(), "12".to_owned())]).is_ok());
        assert!(build_fingerprint(&minor, vec![("acme".to_owned(), "1.5".to_owned())]).is_err());
    }
}
