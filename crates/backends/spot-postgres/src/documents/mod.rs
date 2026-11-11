//! `DocumentStore` for [`PgTransaction`] (ADR-33, ADR-45, ADR-053).
//!
//! A document is one row of its entity's table plus the rows of its child tables, written and
//! loaded as a unit. Rules this module enforces on top of the schema:
//!
//! - **Writes are full and checked before anything is sent.** The fields must be exactly the
//!   entity's declared fields, each passing `FieldType::check` (types, nullability, text limits,
//!   decimals that are *never rounded*), plus what PostgreSQL cannot store (NUL, bad JSON). A
//!   failed check is `InvalidValue` and leaves the transaction usable.
//! - **Child rows** are stored with `parent`, `parent_field` and a 1-based `idx`. `update`
//!   deletes the rows that are gone, updates those whose id is still there and inserts the new
//!   ones; a `load` returns them ordered by `idx`.
//! - **Versions.** `insert` stores version 1; `update` and `delete` apply only at the expected
//!   version (`StaleVersion`, or `NotFound` when the id does not exist) and `update` adds exactly 1.
//! - **Counting.** Every parent and child row written or deleted counts against `max_writes`;
//!   exceeding it is `TransactionTooLarge`. Outbox events are counted by WP-STO-08.T1.
//! - **Links** are checked by deferred foreign keys, so the `ReferenceViolation` of a missing or
//!   still-linked record normally comes from `COMMIT`. PostgreSQL names the constraint but not the column, so a
//!   violation is named (entity and field) from its constraint through the catalog, whether a
//!   write or `COMMIT` raised it.
//!
//! Row locks: a `RowLock` locks the parent row. With `Wait::SkipLocked` a locked row loads as
//! `None`, which the contract's return type cannot tell from a missing one.

mod values;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use spot_storage_contract::schema::system;
use spot_storage_contract::{
    Catalog, ChildRow, Document, DocumentData, EntityDef, EntityKind, ErrorKind, Fields, Name,
    RecordId, RowLock, StorageError, StorageResult, Version, Wait,
};
use tokio_postgres::Statement;
use tokio_postgres::types::ToSql;

use self::values::{Param, check_value, placeholder, read_value, select_expr, to_param, uuid_of};
use crate::PgTransaction;
use crate::schema::names::{entity_for_pk, exclusion_for_constraint, link_for_constraint, quote};

/// A shared bound parameter, so one list of field values can serve several statements.
type Shared = Arc<dyn ToSql + Sync + Send>;

fn shared(param: Param) -> Shared {
    Arc::from(param)
}

/// A child row ready to write.
struct ChildWrite {
    field: Name,
    id: RecordId,
    idx: i32,
    values: Vec<Shared>,
}

/// A document that has passed every check, with its parameters.
struct Checked<'a> {
    entity: &'a EntityDef,
    values: Vec<Shared>,
    /// Rows per child entity (two child tables may share one entity).
    children: BTreeMap<Name, (&'a EntityDef, Vec<ChildWrite>)>,
    rows: u32,
}

// --- the operations --------------------------------------------------------------------------

/// Inserts a new document.
pub(crate) async fn insert(tx: &mut PgTransaction, doc: &DocumentData) -> StorageResult<Version> {
    let catalog = tx.catalog_arc().clone();
    let checked = checked(tx, &catalog, doc)?;
    tx.count_writes(checked.rows.saturating_add(1))?;

    let table = checked.entity.name.as_str();
    let mut columns = vec![quote(system::ID), quote(system::VERSION)];
    let mut marks = vec!["$1".to_owned(), "$2".to_owned()];
    for (i, field) in checked.entity.fields.iter().enumerate() {
        columns.push(quote(field.name.as_str()));
        marks.push(placeholder(&field.ty, i + 3));
    }
    let sql = format!(
        "INSERT INTO {} ({}) VALUES ({})",
        quote(table),
        columns.join(", "),
        marks.join(", ")
    );
    let mut params: Vec<Shared> = vec![Arc::new(uuid_of(doc.id)), Arc::new(1_i64)];
    params.extend(checked.values.iter().cloned());
    execute(tx, &catalog, &sql, &params).await?;

    for (child, rows) in checked.children.values() {
        insert_children(tx, &catalog, child, doc.id, rows).await?;
    }
    Ok(Version::FIRST)
}

/// Loads a document, optionally locking its parent row.
pub(crate) async fn load(
    tx: &mut PgTransaction,
    entity: &Name,
    id: RecordId,
    lock: RowLock,
) -> StorageResult<Option<Document>> {
    let catalog = tx.catalog_arc().clone();
    let def = document_entity(tx, &catalog, entity)?;
    let (suffix, wait) = match lock {
        RowLock::None => ("", None),
        RowLock::Share(w) => (" FOR SHARE", Some(w)),
        RowLock::Update(w) => (" FOR UPDATE", Some(w)),
    };
    let suffix = format!(
        "{suffix}{}",
        match wait {
            Some(Wait::NoWait) => " NOWAIT",
            Some(Wait::SkipLocked) => " SKIP LOCKED",
            _ => "",
        }
    );
    let mut select = vec![quote(system::VERSION)];
    select.extend(
        def.fields
            .iter()
            .map(|f| select_expr(&f.ty, &quote(f.name.as_str()))),
    );
    let sql = format!(
        "SELECT {} FROM {} WHERE {} = $1{suffix}",
        select.join(", "),
        quote(entity.as_str()),
        quote(system::ID)
    );
    let params: [Shared; 1] = [Arc::new(uuid_of(id))];
    let rows = query(tx, &catalog, &sql, &params, wait).await?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let version = stored_version(tx, row.try_get::<_, i64>(0))?;
    let mut fields = Fields::new();
    for (i, field) in def.fields.iter().enumerate() {
        let value = read_value(row, i + 1, &field.ty).map_err(|e| tx.fail_storage(e))?;
        fields.insert(field.name.clone(), value);
    }

    let mut children: BTreeMap<Name, Vec<ChildRow>> = def
        .children
        .iter()
        .map(|c| (c.field.clone(), Vec::new()))
        .collect();
    for (_, child) in child_entities(&catalog, def)? {
        let mut select = vec![quote(system::ID), quote(system::PARENT_FIELD)];
        select.extend(
            child
                .fields
                .iter()
                .map(|f| select_expr(&f.ty, &quote(f.name.as_str()))),
        );
        let sql = format!(
            "SELECT {} FROM {} WHERE {} = $1 ORDER BY {}, {}",
            select.join(", "),
            quote(child.name.as_str()),
            quote(system::PARENT),
            quote(system::PARENT_FIELD),
            quote(system::IDX)
        );
        for row in query(tx, &catalog, &sql, &params, None).await? {
            let field = Name::new(&row.try_get::<_, String>(1).map_err(|_| {
                tx.fail_storage(StorageError::new(
                    ErrorKind::Internal,
                    "a child row has no readable parent_field",
                ))
            })?)
            .map_err(|e| tx.fail_storage(e))?;
            let child_id = row.try_get::<_, uuid::Uuid>(0).map_err(|_| {
                tx.fail_storage(StorageError::new(
                    ErrorKind::Internal,
                    "a child row has no readable id",
                ))
            })?;
            let mut values = Fields::new();
            for (i, f) in child.fields.iter().enumerate() {
                let v = read_value(&row, i + 2, &f.ty).map_err(|e| tx.fail_storage(e))?;
                values.insert(f.name.clone(), v);
            }
            children.entry(field).or_default().push(ChildRow {
                id: RecordId::from_bytes(*child_id.as_bytes()).map_err(|e| tx.fail_storage(e))?,
                fields: values,
            });
        }
    }
    Ok(Some(Document {
        data: DocumentData {
            entity: entity.clone(),
            id,
            fields,
            children,
        },
        version,
    }))
}

/// Replaces a document if its stored version is `expected`.
pub(crate) async fn update(
    tx: &mut PgTransaction,
    doc: &DocumentData,
    expected: Version,
) -> StorageResult<Version> {
    let catalog = tx.catalog_arc().clone();
    let checked = checked(tx, &catalog, doc)?;
    let expected_sql = version_sql(tx, expected)?;
    tx.count_writes(1)?;

    let mut sets = vec![format!("{v} = {v} + 1", v = quote(system::VERSION))];
    for (i, field) in checked.entity.fields.iter().enumerate() {
        sets.push(format!(
            "{} = {}",
            quote(field.name.as_str()),
            placeholder(&field.ty, i + 3)
        ));
    }
    let sql = format!(
        "UPDATE {} SET {} WHERE {id} = $1 AND {v} = $2 RETURNING {v}",
        quote(checked.entity.name.as_str()),
        sets.join(", "),
        id = quote(system::ID),
        v = quote(system::VERSION)
    );
    let mut params: Vec<Shared> = vec![Arc::new(uuid_of(doc.id)), Arc::new(expected_sql)];
    params.extend(checked.values.iter().cloned());
    let rows = query(tx, &catalog, &sql, &params, None).await?;
    let Some(row) = rows.first() else {
        return Err(missing_or_stale(tx, &catalog, &doc.entity, doc.id, expected).await);
    };
    let version = stored_version(tx, row.try_get::<_, i64>(0))?;

    for (name, child) in child_entities(&catalog, checked.entity)? {
        let empty = Vec::new();
        let rows = checked.children.get(name).map_or(&empty, |(_, rows)| rows);
        replace_children(tx, &catalog, child, doc.id, rows).await?;
    }
    Ok(version)
}

/// Deletes a document and its child rows if its stored version is `expected`.
pub(crate) async fn delete(
    tx: &mut PgTransaction,
    entity: &Name,
    id: RecordId,
    expected: Version,
) -> StorageResult<()> {
    let catalog = tx.catalog_arc().clone();
    let def = document_entity(tx, &catalog, entity)?;
    let expected_sql = version_sql(tx, expected)?;
    tx.count_writes(1)?;
    let sql = format!(
        "DELETE FROM {} WHERE {} = $1 AND {} = $2",
        quote(entity.as_str()),
        quote(system::ID),
        quote(system::VERSION)
    );
    let params: [Shared; 2] = [Arc::new(uuid_of(id)), Arc::new(expected_sql)];
    if execute(tx, &catalog, &sql, &params).await? == 0 {
        return Err(missing_or_stale(tx, &catalog, entity, id, expected).await);
    }
    // Nothing cascades: the child rows go because this is their document.
    for (_, child) in child_entities(&catalog, def)? {
        let sql = format!(
            "DELETE FROM {} WHERE {} = $1",
            quote(child.name.as_str()),
            quote(system::PARENT)
        );
        let removed = execute(tx, &catalog, &sql, &params[..1]).await?;
        tx.count_writes(u32::try_from(removed).unwrap_or(u32::MAX))?;
    }
    Ok(())
}

// --- child rows ------------------------------------------------------------------------------

async fn insert_children(
    tx: &mut PgTransaction,
    catalog: &Catalog,
    child: &EntityDef,
    parent: RecordId,
    rows: &[ChildWrite],
) -> StorageResult<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let mut columns = [
        system::ID,
        system::PARENT,
        system::PARENT_FIELD,
        system::IDX,
    ]
    .map(quote)
    .to_vec();
    let mut marks: Vec<String> = (1..=4).map(|n| format!("${n}")).collect();
    for (i, field) in child.fields.iter().enumerate() {
        columns.push(quote(field.name.as_str()));
        marks.push(placeholder(&field.ty, i + 5));
    }
    let sql = format!(
        "INSERT INTO {} ({}) VALUES ({})",
        quote(child.name.as_str()),
        columns.join(", "),
        marks.join(", ")
    );
    let statement = prepare(tx, catalog, &sql).await?;
    for row in rows {
        let mut params: Vec<Shared> = vec![
            Arc::new(uuid_of(row.id)),
            Arc::new(uuid_of(parent)),
            Arc::new(row.field.as_str().to_owned()),
            Arc::new(row.idx),
        ];
        params.extend(row.values.iter().cloned());
        execute_prepared(tx, catalog, &statement, &params).await?;
    }
    Ok(())
}

/// Brings the rows of `child` under `parent` to `rows`: delete the missing, update the kept,
/// insert the new.
async fn replace_children(
    tx: &mut PgTransaction,
    catalog: &Catalog,
    child: &EntityDef,
    parent: RecordId,
    rows: &[ChildWrite],
) -> StorageResult<()> {
    let sql = format!(
        "SELECT {} FROM {} WHERE {} = $1",
        quote(system::ID),
        quote(child.name.as_str()),
        quote(system::PARENT)
    );
    let parent_param: [Shared; 1] = [Arc::new(uuid_of(parent))];
    let mut existing = BTreeSet::new();
    for row in query(tx, catalog, &sql, &parent_param, None).await? {
        let id = row.try_get::<_, uuid::Uuid>(0).map_err(|_| {
            tx.fail_storage(StorageError::new(
                ErrorKind::Internal,
                "a child row has no readable id",
            ))
        })?;
        existing.insert(id);
    }
    let kept: BTreeSet<uuid::Uuid> = rows.iter().map(|r| uuid_of(r.id)).collect();
    let gone: Vec<uuid::Uuid> = existing.difference(&kept).copied().collect();
    let (updated, added): (Vec<&ChildWrite>, Vec<&ChildWrite>) =
        rows.iter().partition(|r| existing.contains(&uuid_of(r.id)));
    let touched = gone.len() + updated.len() + added.len();
    tx.count_writes(u32::try_from(touched).unwrap_or(u32::MAX))?;

    if !gone.is_empty() {
        let sql = format!(
            "DELETE FROM {} WHERE {} = $1 AND {} = ANY($2)",
            quote(child.name.as_str()),
            quote(system::PARENT),
            quote(system::ID)
        );
        let params: [Shared; 2] = [Arc::new(uuid_of(parent)), Arc::new(gone)];
        execute(tx, catalog, &sql, &params).await?;
    }
    if !updated.is_empty() {
        let mut sets = vec![
            format!("{} = $3", quote(system::PARENT_FIELD)),
            format!("{} = $4", quote(system::IDX)),
        ];
        for (i, field) in child.fields.iter().enumerate() {
            sets.push(format!(
                "{} = {}",
                quote(field.name.as_str()),
                placeholder(&field.ty, i + 5)
            ));
        }
        let sql = format!(
            "UPDATE {} SET {} WHERE {} = $1 AND {} = $2",
            quote(child.name.as_str()),
            sets.join(", "),
            quote(system::ID),
            quote(system::PARENT)
        );
        let statement = prepare(tx, catalog, &sql).await?;
        for row in updated {
            let mut params: Vec<Shared> = vec![
                Arc::new(uuid_of(row.id)),
                Arc::new(uuid_of(parent)),
                Arc::new(row.field.as_str().to_owned()),
                Arc::new(row.idx),
            ];
            params.extend(row.values.iter().cloned());
            execute_prepared(tx, catalog, &statement, &params).await?;
        }
    }
    let new_rows: Vec<ChildWrite> = added
        .into_iter()
        .map(|r| ChildWrite {
            field: r.field.clone(),
            id: r.id,
            idx: r.idx,
            values: r.values.clone(),
        })
        .collect();
    insert_children(tx, catalog, child, parent, &new_rows).await
}

// --- validation ------------------------------------------------------------------------------

/// The document entity `name`; a child entity is `UnknownEntity` with the reason.
fn document_entity<'a>(
    tx: &mut PgTransaction,
    catalog: &'a Catalog,
    name: &Name,
) -> StorageResult<&'a EntityDef> {
    let found = catalog.entity(name).and_then(|def| match &def.kind {
        EntityKind::Document => Ok(def),
        EntityKind::Child { parent } => Err(StorageError::new(
            ErrorKind::UnknownEntity,
            format!(
                "`{name}` is a child entity and cannot be read or written directly; \
                 use its document entity `{parent}`"
            ),
        )
        .with_entity(name)),
    });
    found.map_err(|e| tx.fail_storage(e))
}

/// The child entities of `def` (each once), by name.
fn child_entities<'a>(
    catalog: &'a Catalog,
    def: &EntityDef,
) -> StorageResult<Vec<(&'a Name, &'a EntityDef)>> {
    let mut seen = BTreeMap::new();
    for table in &def.children {
        let child = catalog.entity(&table.entity)?;
        seen.insert(&child.name, child);
    }
    Ok(seen.into_iter().collect())
}

fn checked<'a>(
    tx: &mut PgTransaction,
    catalog: &'a Catalog,
    doc: &DocumentData,
) -> StorageResult<Checked<'a>> {
    let entity = document_entity(tx, catalog, &doc.entity)?;
    check_document(catalog, entity, doc).map_err(|e| tx.fail_storage(e))
}

fn check_document<'a>(
    catalog: &'a Catalog,
    entity: &'a EntityDef,
    doc: &DocumentData,
) -> StorageResult<Checked<'a>> {
    let values = check_fields(entity, &doc.fields)?;
    for field in doc.children.keys() {
        entity.child_table(field)?;
    }
    let mut ids = BTreeSet::from([doc.id]);
    let mut children: BTreeMap<Name, (&EntityDef, Vec<ChildWrite>)> = BTreeMap::new();
    let mut rows = 0_u32;
    for table in &entity.children {
        let child = catalog.entity(&table.entity)?;
        let slot = children
            .entry(child.name.clone())
            .or_insert_with(|| (child, Vec::new()));
        for (position, row) in doc
            .children
            .get(&table.field)
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .enumerate()
        {
            if !ids.insert(row.id) {
                return Err(StorageError::new(
                    ErrorKind::InvalidValue,
                    format!(
                        "`{}.{}` lists the row id {} twice; ids must be unique within a document",
                        entity.name, table.field, row.id
                    ),
                )
                .with_entity(&entity.name)
                .with_field(&table.field));
            }
            slot.1.push(ChildWrite {
                field: table.field.clone(),
                id: row.id,
                idx: i32::try_from(position + 1).map_err(|_| {
                    StorageError::new(ErrorKind::InvalidValue, "too many child rows")
                })?,
                values: check_fields(child, &row.fields)?,
            });
            rows = rows.saturating_add(1);
        }
    }
    Ok(Checked {
        entity,
        values,
        children,
        rows,
    })
}

/// The parameters for `fields` in the entity's declared order; the set must be exactly the
/// declared fields.
fn check_fields(entity: &EntityDef, fields: &Fields) -> StorageResult<Vec<Shared>> {
    for name in fields.keys() {
        entity.field(name).map_err(|_| {
            StorageError::new(
                ErrorKind::InvalidValue,
                format!("`{}` has no field `{name}`", entity.name),
            )
            .with_entity(&entity.name)
            .with_field(name)
        })?;
    }
    let mut out = Vec::with_capacity(entity.fields.len());
    for field in &entity.fields {
        let value = fields.get(&field.name).ok_or_else(|| {
            StorageError::new(
                ErrorKind::InvalidValue,
                format!(
                    "`{}.{}` is missing: writes are full and must give every declared field",
                    entity.name, field.name
                ),
            )
            .with_entity(&entity.name)
            .with_field(&field.name)
        })?;
        check_value(&entity.name, field, value)?;
        out.push(shared(to_param(&field.ty, value)?));
    }
    Ok(out)
}

// --- statements ------------------------------------------------------------------------------

fn refs(params: &[Shared]) -> Vec<&(dyn ToSql + Sync)> {
    params.iter().map(|p| &**p as &(dyn ToSql + Sync)).collect()
}

async fn execute(
    tx: &mut PgTransaction,
    catalog: &Catalog,
    sql: &str,
    params: &[Shared],
) -> StorageResult<u64> {
    let client = tx.client()?;
    let result = client.execute(sql, &refs(params)).await;
    result.map_err(|e| write_error(tx, catalog, &e, None))
}

async fn query(
    tx: &mut PgTransaction,
    catalog: &Catalog,
    sql: &str,
    params: &[Shared],
    lock_wait: Option<Wait>,
) -> StorageResult<Vec<tokio_postgres::Row>> {
    let client = tx.client()?;
    let result = client.query(sql, &refs(params)).await;
    result.map_err(|e| write_error(tx, catalog, &e, lock_wait))
}

async fn prepare(tx: &mut PgTransaction, catalog: &Catalog, sql: &str) -> StorageResult<Statement> {
    let client = tx.client()?;
    let result = client.prepare(sql).await;
    result.map_err(|e| write_error(tx, catalog, &e, None))
}

async fn execute_prepared(
    tx: &mut PgTransaction,
    catalog: &Catalog,
    statement: &Statement,
    params: &[Shared],
) -> StorageResult<u64> {
    let client = tx.client()?;
    let result = client.execute(statement, &refs(params)).await;
    result.map_err(|e| write_error(tx, catalog, &e, None))
}

/// Records a driver error on the transaction and gives it the contract's shape: a duplicate
/// primary key is `AlreadyExists`, and a link violation names the linking entity and field.
/// `lock_wait` is the wait mode of the row lock the statement asked for, if any.
fn write_error(
    tx: &mut PgTransaction,
    catalog: &Catalog,
    error: &tokio_postgres::Error,
    lock_wait: Option<Wait>,
) -> StorageError {
    let mut mapped = tx.fail(error, lock_wait);
    let constraint = mapped.constraint.as_ref().map(|c| c.as_str().to_owned());
    match (mapped.kind, constraint) {
        (ErrorKind::UniqueViolation, Some(c)) => {
            if let Some(entity) = entity_for_pk(catalog, &c) {
                mapped.kind = ErrorKind::AlreadyExists;
                mapped.explanation = match entity.kind {
                    EntityKind::Document => "a document with this id already exists".to_owned(),
                    EntityKind::Child { .. } => {
                        "a child row with this id already exists; a row id belongs to one \
                         document only"
                            .to_owned()
                    }
                };
                mapped.entity = Some(entity.name.clone());
                mapped.constraint = None;
            }
        }
        (ErrorKind::ExclusionViolation, Some(c)) => {
            // Name the entity, the declared constraint and the range field, and say it in the
            // declaration's own words. Storage cannot know whether the caller may read the
            // conflicting record, so its id and values are never in the error (ADR-087 5.6).
            if let Some((entity, def)) = exclusion_for_constraint(catalog, &c) {
                mapped.entity = Some(entity.name.clone());
                mapped.field = Some(def.field.clone());
                mapped.constraint = Some(def.name.clone());
                mapped.explanation = def.message.clone();
            }
        }
        (ErrorKind::ReferenceViolation, Some(c)) => {
            if let Some((entity, field)) = link_for_constraint(catalog, &c) {
                mapped.entity = Some(entity);
                mapped.field = Some(field);
            }
        }
        _ => {}
    }
    mapped
}

/// A version as the `bigint` column holds it.
fn version_sql(tx: &mut PgTransaction, version: Version) -> StorageResult<i64> {
    i64::try_from(version.0).map_err(|_| {
        tx.fail_storage(StorageError::new(
            ErrorKind::InvalidValue,
            format!("version {} is out of range", version.0),
        ))
    })
}

fn stored_version(
    tx: &mut PgTransaction,
    stored: Result<i64, tokio_postgres::Error>,
) -> StorageResult<Version> {
    stored
        .ok()
        .and_then(|v| u64::try_from(v).ok())
        .map(Version)
        .ok_or_else(|| {
            tx.fail_storage(StorageError::new(
                ErrorKind::Internal,
                "a stored document has no readable version",
            ))
        })
}

/// Why an `UPDATE` or `DELETE` at `expected` matched nothing: the document does not exist
/// (`NotFound`) or it is at another version (`StaleVersion`).
async fn missing_or_stale(
    tx: &mut PgTransaction,
    catalog: &Catalog,
    entity: &Name,
    id: RecordId,
    expected: Version,
) -> StorageError {
    let sql = format!(
        "SELECT {} FROM {} WHERE {} = $1",
        quote(system::VERSION),
        quote(entity.as_str()),
        quote(system::ID)
    );
    let params: [Shared; 1] = [Arc::new(uuid_of(id))];
    let found = match query(tx, catalog, &sql, &params, None).await {
        Ok(rows) => rows.first().and_then(|r| r.try_get::<_, i64>(0).ok()),
        Err(e) => return e,
    };
    let error = match found {
        None => StorageError::new(
            ErrorKind::NotFound,
            format!("`{entity}` {id} does not exist"),
        ),
        Some(stored) => StorageError::new(
            ErrorKind::StaleVersion,
            format!(
                "`{entity}` {id} is at version {stored}, not the expected {}; reload it and retry",
                expected.0
            ),
        ),
    };
    tx.fail_storage(error.with_entity(entity))
}
