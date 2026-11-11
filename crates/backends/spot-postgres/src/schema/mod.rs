//! `StorageBackend::prepare`: additive schema creation (ADR-33, ADR-45, ADR-053).
//!
//! One transaction, serialised across processes by an advisory lock, brings the database to the
//! catalog:
//!
//! 1. the `spot_meta` table (catalog version and collation) and the tenant's ICU collation;
//! 2. a table per entity (system columns, one column per field), or, for a table that exists,
//!    the missing columns;
//! 3. a deferred, non-cascading foreign key for every `LinkCheck::Database` link and every
//!    child's `parent`;
//! 4. every declared index and an index on each child table's `(parent, idx)`;
//! 5. every declared exclusion constraint (ADR-087 section 5.2), after the indexes.
//!
//! Geo fields, GiST and trigram indexes and scoped exclusions need extensions (`postgis`,
//! `btree_gist`, `pg_trgm`). `prepare` never creates one: it checks `pg_extension` first and
//! refuses with `InvalidSchema`, naming the extension and the audited `extension.install`
//! operation, before it changes anything (SEM-GEO-04). There is no text-column or built-in
//! point fallback.
//!
//! Anything that cannot be done by adding (a changed column type, nullability or text limit, a
//! column the catalog no longer has, a changed link check, a different index, a different
//! collation) fails with `InvalidSchema` and names what differs; nothing is dropped or altered.
//! Running it again with the same catalog finds everything in place and sends no DDL.

mod ddl;
pub(crate) mod names;

use std::collections::BTreeMap;

use spot_storage_contract::schema::system;
use spot_storage_contract::{
    Catalog, EntityDef, EntityKind, ErrorKind, FieldType, IndexDef, IndexMethod, LinkCheck,
    LinkTarget, Name, OpClass, StorageError, StorageResult,
};
use tokio_postgres::Transaction;

use self::ddl::ColumnSpec;
use self::names::{
    META_TABLE, exclusion_name, fk_name, gin_index_name, gist_index_name, parent_fk_name,
    parent_index_name, quote,
};
use crate::PgBackend;
use crate::error::map_pg_error;

/// Held for the transaction's duration so concurrent `prepare` calls queue instead of racing on
/// `CREATE`: the bytes of `SPOTSCHM`.
const ADVISORY_KEY: i64 = 0x5350_4f54_5343_484d;

/// How long `prepare` waits for any one lock (an `ALTER TABLE` needs the table to itself)
/// before failing with `LockTimeout` instead of hanging behind a long transaction.
const LOCK_TIMEOUT: &str = "30s";

/// An existing column as the database reports it.
struct DbColumn {
    ty: String,
    not_null: bool,
}

/// Makes the database able to hold the catalog's entities.
pub(crate) async fn prepare(backend: &PgBackend, catalog: &Catalog) -> StorageResult<()> {
    if catalog.entity(&meta_name()?).is_ok() {
        return Err(invalid(format!(
            "`{META_TABLE}` is reserved for the schema's own bookkeeping and cannot be an entity"
        )));
    }
    refuse_search(catalog)?;
    let mut conn = backend.checkout().await?;
    let tx = conn.transaction().await.map_err(db)?;
    apply(&tx, catalog).await?;
    tx.commit().await.map_err(db)
}

/// ADR-087 section 7: the search projection (generated `tsvector` column, GIN index, rank) is
/// built by WP-STO-13.T1. Until then a catalog with a `search` declaration is refused with
/// `Unsupported` (what [`Capabilities::search`](spot_storage_contract::Capabilities) says), never
/// prepared without its projection.
fn refuse_search(catalog: &Catalog) -> StorageResult<()> {
    for e in catalog.entities() {
        if e.search.is_some() {
            return Err(StorageError::new(
                ErrorKind::Unsupported,
                format!(
                    "the search declaration on `{}` is not supported by this backend yet: the \
                     search projection arrives with WP-STO-13.T1 (ADR-087)",
                    e.name
                ),
            )
            .with_entity(&e.name));
        }
    }
    Ok(())
}

/// An extension the catalog needs and why (the same list as `spot-pg-migrate`'s
/// `required_extensions`: a range field and a single-column exclusion need none).
struct Need {
    extension: &'static str,
    entity: Name,
    because: String,
}

fn needed_extensions(catalog: &Catalog) -> Vec<Need> {
    let mut needs: BTreeMap<&'static str, Need> = BTreeMap::new();
    let mut need = |extension: &'static str, entity: &EntityDef, because: String| {
        needs.entry(extension).or_insert_with(|| Need {
            extension,
            entity: entity.name.clone(),
            because,
        });
    };
    for entity in catalog.entities() {
        for field in &entity.fields {
            if matches!(field.ty, FieldType::Geo { .. }) {
                need("postgis", entity, format!("the geo field `{}`", field.name));
            }
        }
        for index in &entity.indexes {
            match index.opclass {
                Some(OpClass::GistTrgm | OpClass::GinTrgm) => need(
                    "pg_trgm",
                    entity,
                    format!("the trigram index `{}`", index.name),
                ),
                Some(OpClass::GistScalar) => need(
                    "btree_gist",
                    entity,
                    format!("the scalar GiST index `{}`", index.name),
                ),
                None => {}
            }
        }
        for x in &entity.exclusions {
            if !x.scope.is_empty() {
                need(
                    "btree_gist",
                    entity,
                    format!("the exclusion `{}` with scope columns", x.name),
                );
            }
        }
    }
    needs.into_values().collect()
}

/// Refuses a catalog whose extensions this database lacks, before any change (SEM-GEO-04).
async fn check_extensions(tx: &Transaction<'_>, catalog: &Catalog) -> StorageResult<()> {
    let needs = needed_extensions(catalog);
    if needs.is_empty() {
        return Ok(());
    }
    let names: Vec<String> = needs.iter().map(|n| n.extension.to_owned()).collect();
    let rows = tx
        .query(
            "SELECT extname::text FROM pg_extension WHERE extname = ANY($1)",
            &[&names],
        )
        .await
        .map_err(db)?;
    let present: Vec<String> = rows.iter().map(|r| r.get(0)).collect();
    for need in &needs {
        if !present.iter().any(|p| p == need.extension) {
            return Err(invalid(format!(
                "the catalog needs the extension `{}` ({} on `{}`), which is not installed in \
                 this database; `prepare` never creates an extension and never falls back to a \
                 text column or a built-in type: install it with the audited `extension.install` \
                 operation (Admin Center database manager), then prepare again",
                need.extension, need.because, need.entity
            ))
            .with_entity(&need.entity));
        }
    }
    Ok(())
}

fn meta_name() -> StorageResult<Name> {
    Name::new(META_TABLE)
}

fn db(e: tokio_postgres::Error) -> StorageError {
    map_pg_error(&e, None)
}

fn invalid(explanation: String) -> StorageError {
    StorageError::new(ErrorKind::InvalidSchema, explanation)
}

async fn apply(tx: &Transaction<'_>, catalog: &Catalog) -> StorageResult<()> {
    tx.execute(
        "SELECT set_config('lock_timeout', $1, true)",
        &[&LOCK_TIMEOUT],
    )
    .await
    .map_err(db)?;
    tx.execute("SELECT pg_advisory_xact_lock($1)", &[&ADVISORY_KEY])
        .await
        .map_err(db)?;
    check_extensions(tx, catalog).await?;
    tx.batch_execute(&format!(
        "CREATE TABLE IF NOT EXISTS {} (key text COLLATE \"C\" PRIMARY KEY, value text NOT NULL)",
        quote(META_TABLE)
    ))
    .await
    .map_err(db)?;
    check_collation(tx, catalog).await?;
    tx.batch_execute(&ddl::create_collation(&catalog.collation))
        .await
        .map_err(db)?;

    for entity in catalog.entities() {
        ensure_table(tx, entity).await?;
    }
    for entity in catalog.entities() {
        ensure_links(tx, entity).await?;
        ensure_indexes(tx, entity).await?;
    }
    for entity in catalog.entities() {
        ensure_exclusions(tx, entity).await?;
    }
    // The outbox, the change feed and the capture trigger on every document table.
    crate::feed::install(tx, catalog).await?;
    record_meta(tx, catalog).await
}

// --- meta ------------------------------------------------------------------------------------

async fn meta_value(tx: &Transaction<'_>, key: &str) -> StorageResult<Option<String>> {
    let sql = format!("SELECT value FROM {} WHERE key = $1", quote(META_TABLE));
    let row = tx.query_opt(&sql, &[&key]).await.map_err(db)?;
    Ok(row.map(|r| r.get(0)))
}

/// The collation of a database cannot change: existing text would no longer be in order.
async fn check_collation(tx: &Transaction<'_>, catalog: &Catalog) -> StorageResult<()> {
    if let Some(have) = meta_value(tx, "collation").await?
        && have != catalog.collation.as_str()
    {
        return Err(invalid(format!(
            "the database was prepared with collation `{have}` but the catalog says `{}`; \
             a tenant's collation cannot change (create a new database and migrate)",
            catalog.collation.as_str()
        )));
    }
    Ok(())
}

async fn record_meta(tx: &Transaction<'_>, catalog: &Catalog) -> StorageResult<()> {
    let table = quote(META_TABLE);
    let collation = catalog.collation.as_str();
    tx.execute(
        &format!(
            "INSERT INTO {table} (key, value) VALUES ('collation', $1) ON CONFLICT (key) DO NOTHING"
        ),
        &[&collation],
    )
    .await
    .map_err(db)?;
    // Only ever moves forward, and a repeat with the same catalog writes nothing new.
    let version = catalog.version.to_string();
    let stored = meta_value(tx, "catalog_version")
        .await?
        .and_then(|v| v.parse::<u64>().ok());
    if stored.is_none_or(|v| v < catalog.version) {
        tx.execute(
            &format!(
                "INSERT INTO {table} (key, value) VALUES ('catalog_version', $1) \
                 ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value"
            ),
            &[&version],
        )
        .await
        .map_err(db)?;
    }
    Ok(())
}

// --- tables and columns ----------------------------------------------------------------------

async fn table_exists(tx: &Transaction<'_>, table: &str) -> StorageResult<bool> {
    let row = tx
        .query_one("SELECT to_regclass($1) IS NOT NULL", &[&quote(table)])
        .await
        .map_err(db)?;
    Ok(row.get(0))
}

async fn db_columns(
    tx: &Transaction<'_>,
    table: &str,
) -> StorageResult<BTreeMap<String, DbColumn>> {
    let rows = tx
        .query(
            "SELECT a.attname::text, format_type(a.atttypid, a.atttypmod), a.attnotnull \
             FROM pg_attribute a \
             WHERE a.attrelid = to_regclass($1) AND a.attnum > 0 AND NOT a.attisdropped",
            &[&quote(table)],
        )
        .await
        .map_err(db)?;
    Ok(rows
        .iter()
        .map(|r| {
            (
                r.get::<_, String>(0),
                DbColumn {
                    ty: r.get(1),
                    not_null: r.get(2),
                },
            )
        })
        .collect())
}

async fn ensure_table(tx: &Transaction<'_>, entity: &EntityDef) -> StorageResult<()> {
    let table = entity.name.as_str();
    if !table_exists(tx, table).await? {
        return tx
            .batch_execute(&ddl::create_table(entity))
            .await
            .map_err(db);
    }
    let have = db_columns(tx, table).await?;
    let want = ddl::columns(entity);
    for column in &want {
        match have.get(&column.name) {
            Some(existing) => same_column(entity, column, existing)?,
            None if column.field.is_some() => add_column(tx, entity, column).await?,
            None => {
                return Err(invalid(format!(
                    "table `{table}` lacks the system column `{}`; it was not created by this \
                     backend or has a different kind",
                    column.name
                ))
                .with_entity(&entity.name));
            }
        }
    }
    for name in have.keys() {
        if !want.iter().any(|c| &c.name == name) {
            return Err(invalid(format!(
                "column `{table}.{name}` is in the database but no longer in the catalog; \
                 dropping a column is not an additive change"
            ))
            .with_entity(&entity.name));
        }
    }
    check_length_limits(tx, entity).await
}

/// An existing column must be what the catalog says; nothing is altered.
fn same_column(entity: &EntityDef, want: &ColumnSpec, have: &DbColumn) -> StorageResult<()> {
    let table = entity.name.as_str();
    let describe = |what: String| {
        let mut error =
            invalid(format!("column `{table}.{}`: {what}", want.name)).with_entity(&entity.name);
        if let Some(field) = &want.field {
            error = error.with_field(&field.name);
        }
        error
    };
    if ddl::normalise_type(&have.ty) != ddl::normalise_type(&want.ty) {
        return Err(describe(format!(
            "the type changed from {} to {}; a type change is not an additive change",
            have.ty, want.ty
        )));
    }
    if have.not_null != want.not_null {
        return Err(describe(format!(
            "it is {} in the database but {} in the catalog; a nullability change is not an \
             additive change",
            if have.not_null {
                "required"
            } else {
                "optional"
            },
            if want.not_null {
                "required"
            } else {
                "optional"
            },
        )));
    }
    Ok(())
}

async fn add_column(
    tx: &Transaction<'_>,
    entity: &EntityDef,
    column: &ColumnSpec,
) -> StorageResult<()> {
    let table = entity.name.as_str();
    if column.not_null {
        let has_rows = tx
            .query_one(
                &format!("SELECT EXISTS (SELECT 1 FROM {})", quote(table)),
                &[],
            )
            .await
            .map_err(db)?;
        if has_rows.get::<_, bool>(0) {
            let mut error = invalid(format!(
                "column `{table}.{}` is new and required, but `{table}` already holds records; \
                 add it as optional",
                column.name
            ))
            .with_entity(&entity.name);
            if let Some(field) = &column.field {
                error = error.with_field(&field.name);
            }
            return Err(error);
        }
    }
    tx.batch_execute(&format!(
        "ALTER TABLE {} ADD COLUMN {}",
        quote(table),
        ddl::column_definition(column)
    ))
    .await
    .map_err(db)?;
    if let Some(field) = &column.field
        && let Some(max) = ddl::max_chars(field)
    {
        tx.batch_execute(&format!(
            "ALTER TABLE {} ADD {}",
            quote(table),
            ddl::length_check(&entity.name, &field.name, max)
        ))
        .await
        .map_err(db)?;
    }
    Ok(())
}

/// A text field's length limit must match the check constraint in the database. (A column added
/// by this run has just been given its check, so it passes.)
async fn check_length_limits(tx: &Transaction<'_>, entity: &EntityDef) -> StorageResult<()> {
    for field in &entity.fields {
        if !matches!(field.ty, FieldType::Text { .. }) {
            continue;
        }
        let name = names::length_check_name(&entity.name, &field.name);
        let row = tx
            .query_opt(
                "SELECT pg_get_constraintdef(oid) FROM pg_constraint \
                 WHERE conname = $1 AND conrelid = to_regclass($2)",
                &[&name, &quote(entity.name.as_str())],
            )
            .await
            .map_err(db)?;
        let have: Option<String> = row.map(|r| r.get(0));
        let want = ddl::max_chars(field);
        let same = match (&have, want) {
            (None, None) => true,
            (Some(def), Some(max)) => def.contains(&format!("<= {max})")),
            _ => false,
        };
        if !same {
            return Err(invalid(format!(
                "the length limit of `{}.{}` changed (database: {}, catalog: {}); changing a \
                 limit is not an additive change",
                entity.name,
                field.name,
                have.as_deref().map_or("none", |_| "a different limit"),
                want.map_or_else(|| "none".to_owned(), |m| m.to_string()),
            ))
            .with_entity(&entity.name)
            .with_field(&field.name));
        }
    }
    Ok(())
}

// --- links -----------------------------------------------------------------------------------

async fn has_constraint(tx: &Transaction<'_>, table: &str, name: &str) -> StorageResult<bool> {
    let row = tx
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = $1 AND conrelid = to_regclass($2))",
            &[&name, &quote(table)],
        )
        .await
        .map_err(db)?;
    Ok(row.get(0))
}

/// Foreign keys: for each database-checked link, and for a child's `parent`.
async fn ensure_links(tx: &Transaction<'_>, entity: &EntityDef) -> StorageResult<()> {
    let table = &entity.name;
    if let EntityKind::Child { parent } = &entity.kind {
        ensure_link(tx, table, system::PARENT, parent, &parent_fk_name(table)).await?;
    }
    for field in &entity.fields {
        let FieldType::Link { target, check } = &field.ty else {
            continue;
        };
        let fk = fk_name(table, field.name.as_str());
        match (target, check) {
            (LinkTarget::Entity(target), LinkCheck::Database) => {
                ensure_link(tx, table, field.name.as_str(), target, &fk).await?;
            }
            _ => {
                if has_constraint(tx, table.as_str(), &fk).await? {
                    return Err(invalid(format!(
                        "link `{table}.{}` is no longer checked by the database but its foreign \
                         key exists; removing a check is not an additive change",
                        field.name
                    ))
                    .with_entity(table)
                    .with_field(&field.name));
                }
            }
        }
    }
    Ok(())
}

async fn ensure_link(
    tx: &Transaction<'_>,
    table: &Name,
    column: &str,
    target: &Name,
    fk: &str,
) -> StorageResult<()> {
    if !has_constraint(tx, table.as_str(), fk).await? {
        tx.batch_execute(&ddl::add_foreign_key(table, column, target, fk))
            .await
            .map_err(db)?;
    }
    Ok(())
}

// --- indexes ---------------------------------------------------------------------------------

/// An existing index as the database reports it.
struct DbIndex {
    table: String,
    unique: bool,
    /// The access method (`btree`, `gist`, `gin`).
    method: String,
    /// The key columns, in order.
    columns: Vec<String>,
    /// The `INCLUDE` columns, in order.
    include: Vec<String>,
    /// The operator class of each key column.
    opclasses: Vec<String>,
}

async fn db_index(tx: &Transaction<'_>, name: &str) -> StorageResult<Option<DbIndex>> {
    let row = tx
        .query_opt(
            "SELECT t.relname::text, i.indisunique, am.amname::text, \
                    COALESCE((SELECT array_agg(a.attname::text ORDER BY k.ord) \
                              FROM unnest(i.indkey::int2[]) WITH ORDINALITY AS k(attnum, ord) \
                              JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = k.attnum \
                              WHERE k.ord <= i.indnkeyatts), ARRAY[]::text[]), \
                    COALESCE((SELECT array_agg(a.attname::text ORDER BY k.ord) \
                              FROM unnest(i.indkey::int2[]) WITH ORDINALITY AS k(attnum, ord) \
                              JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = k.attnum \
                              WHERE k.ord > i.indnkeyatts), ARRAY[]::text[]), \
                    COALESCE((SELECT array_agg(o.opcname::text ORDER BY k.ord) \
                              FROM unnest(i.indclass::oid[]) WITH ORDINALITY AS k(opc, ord) \
                              JOIN pg_opclass o ON o.oid = k.opc), ARRAY[]::text[]) \
             FROM pg_class c \
             JOIN pg_index i ON i.indexrelid = c.oid \
             JOIN pg_class t ON t.oid = i.indrelid \
             JOIN pg_am am ON am.oid = c.relam \
             WHERE c.relname = $1 AND c.relnamespace = to_regnamespace(current_schema())::oid",
            &[&name],
        )
        .await
        .map_err(db)?;
    Ok(row.map(|r| DbIndex {
        table: r.get(0),
        unique: r.get(1),
        method: r.get(2),
        columns: r.get(3),
        include: r.get(4),
        opclasses: r.get(5),
    }))
}

/// The physical name of a declared index: a B-tree keeps its declared name; a GiST or GIN index
/// is `spot_<table>_<name>_gix` or `_ginx` (the names `spot-pg-migrate` plans).
fn index_name(table: &Name, index: &IndexDef) -> String {
    match index.method {
        IndexMethod::BTree => index.name.as_str().to_owned(),
        IndexMethod::Gist => gist_index_name(table, &index.name),
        IndexMethod::Gin => gin_index_name(table, &index.name),
    }
}

fn method_name(method: IndexMethod) -> &'static str {
    match method {
        IndexMethod::BTree => "btree",
        IndexMethod::Gist => "gist",
        IndexMethod::Gin => "gin",
    }
}

fn names_of(fields: &[Name]) -> Vec<String> {
    fields.iter().map(|f| f.as_str().to_owned()).collect()
}

/// Whether an existing index is what `index` declares. Operator classes are compared only where
/// the declaration names a trigram class (every other class is the type's default).
fn same_index(table: &Name, index: &IndexDef, have: &DbIndex) -> bool {
    let class = match index.opclass {
        Some(OpClass::GistTrgm) => Some("gist_trgm_ops"),
        Some(OpClass::GinTrgm) => Some("gin_trgm_ops"),
        Some(OpClass::GistScalar) | None => None,
    };
    have.table == table.as_str()
        && have.unique == index.unique
        && have.method == method_name(index.method)
        && have.columns == names_of(&index.fields)
        && have.include == names_of(&index.include)
        && class.is_none_or(|c| have.opclasses.iter().all(|o| o == c))
}

async fn ensure_index(
    tx: &Transaction<'_>,
    table: &Name,
    name: &str,
    index: &IndexDef,
) -> StorageResult<()> {
    match db_index(tx, name).await? {
        None => tx
            .batch_execute(&ddl::create_index(name, table, index))
            .await
            .map_err(db),
        Some(have) if same_index(table, index, &have) => Ok(()),
        Some(have) => Err(invalid(format!(
            "index `{name}` exists with a different definition (on `{}`, {}, using {}, \
             columns [{}], include [{}]) than the catalog's (on `{table}`, {}, using {}, \
             columns [{}], include [{}]); changing an index is not an additive change",
            have.table,
            if have.unique { "unique" } else { "not unique" },
            have.method,
            have.columns.join(", "),
            have.include.join(", "),
            if index.unique { "unique" } else { "not unique" },
            method_name(index.method),
            names_of(&index.fields).join(", "),
            names_of(&index.include).join(", "),
        ))
        .with_entity(table)),
    }
}

async fn ensure_indexes(tx: &Transaction<'_>, entity: &EntityDef) -> StorageResult<()> {
    for index in &entity.indexes {
        let name = index_name(&entity.name, index);
        ensure_index(tx, &entity.name, &name, index).await?;
    }
    if matches!(entity.kind, EntityKind::Child { .. }) {
        let wanted = [system::PARENT, system::IDX];
        let declared = entity
            .indexes
            .iter()
            .any(|i| i.fields.iter().map(Name::as_str).eq(wanted));
        if !declared {
            let index = IndexDef::btree(
                Name::new(&parent_index_name(&entity.name))?,
                wanted
                    .iter()
                    .map(|f| Name::new(f))
                    .collect::<Result<_, _>>()?,
                false,
            );
            ensure_index(tx, &entity.name, index.name.as_str(), &index).await?;
        }
    }
    Ok(())
}

// --- exclusions ------------------------------------------------------------------------------

/// The exclusion constraints on `table`, by name, with their definition text.
async fn db_exclusions(
    tx: &Transaction<'_>,
    table: &str,
) -> StorageResult<BTreeMap<String, String>> {
    let rows = tx
        .query(
            "SELECT conname::text, pg_get_constraintdef(oid) FROM pg_constraint \
             WHERE contype = 'x' AND conrelid = to_regclass($1)",
            &[&quote(table)],
        )
        .await
        .map_err(db)?;
    Ok(rows.iter().map(|r| (r.get(0), r.get(1))).collect())
}

/// `EXCLUDE USING gist (scope WITH =, range WITH &&)` for every declared exclusion. One that
/// exists must be the declared one; one the catalog no longer has is not dropped but refused.
async fn ensure_exclusions(tx: &Transaction<'_>, entity: &EntityDef) -> StorageResult<()> {
    let have = db_exclusions(tx, entity.name.as_str()).await?;
    for def in &entity.exclusions {
        let name = exclusion_name(&entity.name, &def.name);
        match have.get(&name) {
            None => {
                tx.batch_execute(&ddl::add_exclusion(&entity.name, def))
                    .await
                    .map_err(db)?;
            }
            Some(existing) => {
                if ddl::comparable_definition(existing) != ddl::exclusion_comparable(def) {
                    return Err(invalid(format!(
                        "exclusion `{}` of `{}` exists with a different definition ({existing}) \
                         than the catalog's; changing an exclusion is not an additive change",
                        def.name, entity.name
                    ))
                    .with_entity(&entity.name)
                    .with_field(&def.field));
                }
            }
        }
    }
    for name in have.keys() {
        let declared = entity
            .exclusions
            .iter()
            .any(|d| &exclusion_name(&entity.name, &d.name) == name);
        if !declared {
            return Err(invalid(format!(
                "exclusion `{name}` of `{}` is in the database but not in the catalog; dropping \
                 an exclusion is not an additive change",
                entity.name
            ))
            .with_entity(&entity.name));
        }
    }
    Ok(())
}
