//! The live schema, read as plain data.

use std::collections::{BTreeMap, BTreeSet};

use spot_postgres::PgBackend;
use tokio_postgres::Client;

use crate::conn::connect;
use crate::error::{MigrateError, db};
use crate::names::{META_TABLE, PROGRESS_TABLE, quote};

/// One column as the database reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveColumn {
    /// Column name.
    pub name: String,
    /// `format_type` of the column.
    pub ty: String,
    /// Whether it is `NOT NULL`.
    pub not_null: bool,
    /// The collation, when it is not the database default.
    pub collation: Option<String>,
}

/// One index (primary keys excluded).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveIndex {
    /// Index name.
    pub name: String,
    /// The table.
    pub table: String,
    /// Whether it is unique.
    pub unique: bool,
    /// Columns in order.
    pub columns: Vec<String>,
    /// False for the leftover of a failed `CREATE INDEX CONCURRENTLY`.
    pub valid: bool,
}

/// One foreign key, check or exclusion constraint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveConstraint {
    /// Constraint name.
    pub name: String,
    /// The table.
    pub table: String,
    /// `f` (foreign key), `c` (check) or `x` (exclusion).
    pub kind: char,
    /// Whether existing rows were checked (`NOT VALID` otherwise).
    pub validated: bool,
    /// `pg_get_constraintdef`.
    pub definition: String,
}

/// The extensions `extension.install` may create (ADR-087 section 4.3), the only ones the
/// migrator reads: a closed, platform-owned list.
pub const MANAGED_EXTENSIONS: [&str; 4] = ["postgis", "btree_gist", "pg_trgm", "unaccent"];

/// One extension as `pg_available_extensions` and `pg_extension` report it. An extension
/// installed into `public` is invisible to the schema scan, which reads `current_schema()` only,
/// so the extension state is read from these two catalogues and never from the schema.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveExtension {
    /// The extension name.
    pub name: String,
    /// The version the server offers (`pg_available_extensions.default_version`); `None` when
    /// the server does not have the extension's files at all.
    pub available_version: Option<String>,
    /// The version created in this database (`pg_extension.extversion`); `None` when not created.
    pub installed_version: Option<String>,
}

/// Reads the state of the extensions in `names` from `pg_available_extensions` and
/// `pg_extension`. Every name gets an entry, absent or not.
pub(crate) async fn read_extensions(
    client: &Client,
    names: &[String],
) -> Result<BTreeMap<String, LiveExtension>, MigrateError> {
    let mut out: BTreeMap<String, LiveExtension> = names
        .iter()
        .map(|n| {
            (
                n.clone(),
                LiveExtension {
                    name: n.clone(),
                    available_version: None,
                    installed_version: None,
                },
            )
        })
        .collect();
    let available = client
        .query(
            "SELECT name::text, default_version::text FROM pg_available_extensions \
             WHERE name = ANY($1)",
            &[&names],
        )
        .await
        .map_err(|e| db(&e))?;
    for r in &available {
        if let Some(e) = out.get_mut(&r.get::<_, String>(0)) {
            e.available_version = r.get(1);
        }
    }
    let installed = client
        .query(
            "SELECT extname::text, extversion::text FROM pg_extension WHERE extname = ANY($1)",
            &[&names],
        )
        .await
        .map_err(|e| db(&e))?;
    for r in &installed {
        if let Some(e) = out.get_mut(&r.get::<_, String>(0)) {
            e.installed_version = r.get(1);
        }
    }
    Ok(out)
}

/// One table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveTable {
    /// Columns by name.
    pub columns: BTreeMap<String, LiveColumn>,
    /// Whether the table holds at least one row.
    pub has_rows: bool,
}

/// The spot-owned schema as the database holds it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LiveSchema {
    /// Tables by name (every table of the current schema).
    pub tables: BTreeMap<String, LiveTable>,
    /// Indexes by name.
    pub indexes: BTreeMap<String, LiveIndex>,
    /// Foreign keys, checks and exclusions by `(table, name)`.
    pub constraints: BTreeMap<(String, String), LiveConstraint>,
    /// The collation recorded in `spot_meta`, when the database was prepared.
    pub collation: Option<String>,
    /// Columns an earlier run added optional on its way to required, `table.column`.
    pub expanding: BTreeSet<String>,
}

impl LiveSchema {
    /// Reads the live schema of the database `backend` points at.
    pub async fn read(backend: &PgBackend) -> Result<Self, MigrateError> {
        let client = connect(backend).await?;
        Self::read_with(&client).await
    }

    pub(crate) async fn read_with(client: &Client) -> Result<Self, MigrateError> {
        let mut schema = LiveSchema::default();
        let tables = client
            .query(
                "SELECT c.relname::text FROM pg_class c \
                 WHERE c.relnamespace = to_regnamespace(current_schema())::oid AND c.relkind = 'r'",
                &[],
            )
            .await
            .map_err(|e| db(&e))?;
        let names: Vec<String> = tables.iter().map(|r| r.get(0)).collect();

        let columns = client
            .query(
                "SELECT c.relname::text, a.attname::text, format_type(a.atttypid, a.atttypmod), \
                        a.attnotnull, \
                        (SELECT co.collname::text FROM pg_collation co \
                          WHERE co.oid = a.attcollation AND a.attcollation <> t.typcollation) \
                 FROM pg_attribute a \
                 JOIN pg_class c ON c.oid = a.attrelid \
                 JOIN pg_type t ON t.oid = a.atttypid \
                 WHERE c.relnamespace = to_regnamespace(current_schema())::oid \
                   AND c.relkind = 'r' AND a.attnum > 0 AND NOT a.attisdropped",
                &[],
            )
            .await
            .map_err(|e| db(&e))?;
        for name in &names {
            schema.tables.insert(
                name.clone(),
                LiveTable {
                    columns: BTreeMap::new(),
                    has_rows: false,
                },
            );
        }
        for r in &columns {
            let table: String = r.get(0);
            let column = LiveColumn {
                name: r.get(1),
                ty: crate::ddl::normalise_type(&r.get::<_, String>(2)),
                not_null: r.get(3),
                collation: r.get(4),
            };
            if let Some(t) = schema.tables.get_mut(&table) {
                t.columns.insert(column.name.clone(), column);
            }
        }
        for name in &names {
            if name.starts_with("spot_") {
                continue;
            }
            let row = client
                .query_one(
                    &format!("SELECT EXISTS (SELECT 1 FROM {})", quote(name)),
                    &[],
                )
                .await
                .map_err(|e| db(&e))?;
            if let Some(t) = schema.tables.get_mut(name) {
                t.has_rows = row.get(0);
            }
        }

        let indexes = client
            .query(
                "SELECT c.relname::text, t.relname::text, i.indisunique, i.indisvalid, \
                        COALESCE((SELECT array_agg(a.attname::text ORDER BY k.ord) \
                                  FROM unnest(i.indkey::int2[]) WITH ORDINALITY AS k(attnum, ord) \
                                  JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = k.attnum), \
                                 ARRAY[]::text[]) \
                 FROM pg_index i \
                 JOIN pg_class c ON c.oid = i.indexrelid \
                 JOIN pg_class t ON t.oid = i.indrelid \
                 WHERE t.relnamespace = to_regnamespace(current_schema())::oid \
                   AND NOT i.indisprimary",
                &[],
            )
            .await
            .map_err(|e| db(&e))?;
        for r in &indexes {
            let index = LiveIndex {
                name: r.get(0),
                table: r.get(1),
                unique: r.get(2),
                valid: r.get(3),
                columns: r.get(4),
            };
            schema.indexes.insert(index.name.clone(), index);
        }

        let constraints = client
            .query(
                "SELECT con.conname::text, c.relname::text, con.contype::text, con.convalidated, \
                        pg_get_constraintdef(con.oid) \
                 FROM pg_constraint con JOIN pg_class c ON c.oid = con.conrelid \
                 WHERE c.relnamespace = to_regnamespace(current_schema())::oid \
                   AND con.contype IN ('f', 'c', 'x')",
                &[],
            )
            .await
            .map_err(|e| db(&e))?;
        for r in &constraints {
            let kind: String = r.get(2);
            let constraint = LiveConstraint {
                name: r.get(0),
                table: r.get(1),
                kind: kind.chars().next().unwrap_or('c'),
                validated: r.get(3),
                definition: r.get(4),
            };
            schema.constraints.insert(
                (constraint.table.clone(), constraint.name.clone()),
                constraint,
            );
        }

        if schema.tables.contains_key(META_TABLE) {
            let row = client
                .query_opt(
                    &format!(
                        "SELECT value FROM {} WHERE key = 'collation'",
                        quote(META_TABLE)
                    ),
                    &[],
                )
                .await
                .map_err(|e| db(&e))?;
            schema.collation = row.map(|r| r.get(0));
        }
        if schema.tables.contains_key(PROGRESS_TABLE) {
            let rows = client
                .query(
                    &format!(
                        "SELECT key FROM {} WHERE state = 'expanding'",
                        quote(PROGRESS_TABLE)
                    ),
                    &[],
                )
                .await
                .map_err(|e| db(&e))?;
            schema.expanding = rows.iter().map(|r| r.get(0)).collect();
        }
        Ok(schema)
    }
}
