//! Checks on written documents: entity kind, exact fields, value types, child rows.

use std::collections::BTreeSet;

use spot_storage_contract::{
    Catalog, ChildRow, DocumentData, EntityDef, EntityKind, ErrorKind, Fields, Name, RecordId,
    StorageError, StorageResult, Value,
};

/// The document entity `entity`; a child entity fails with `UnknownEntity`.
pub(crate) fn document_entity<'c>(
    catalog: &'c Catalog,
    entity: &Name,
) -> StorageResult<&'c EntityDef> {
    let def = catalog.entity(entity)?;
    if let EntityKind::Child { parent } = &def.kind {
        return Err(StorageError::new(
            ErrorKind::UnknownEntity,
            format!(
                "`{entity}` holds child rows of `{parent}` documents; read and write them \
                 through their `{parent}` document"
            ),
        )
        .with_entity(entity));
    }
    Ok(def)
}

/// Checks that `fields` holds exactly the declared fields of `def`, each with a valid value.
pub(crate) fn check_fields(def: &EntityDef, fields: &Fields) -> StorageResult<()> {
    for f in &def.fields {
        let Some(value) = fields.get(&f.name) else {
            return Err(invalid(
                def,
                &f.name,
                format!(
                    "`{}.{}` is missing; writes must set every declared field",
                    def.name, f.name
                ),
            ));
        };
        if let Err(why) = f.ty.check(value, f.nullable) {
            return Err(invalid(
                def,
                &f.name,
                format!("`{}.{}`: {why}", def.name, f.name),
            ));
        }
        if let Value::Json(text) = value
            && let Err(e) = serde_json::from_str::<serde_json::Value>(text)
        {
            return Err(invalid(
                def,
                &f.name,
                format!("`{}.{}` is not valid JSON: {e}", def.name, f.name),
            ));
        }
    }
    if let Some(extra) = fields.keys().find(|k| def.field(k).is_err()) {
        return Err(invalid(
            def,
            extra,
            format!("`{}` has no field `{extra}`", def.name),
        ));
    }
    Ok(())
}

/// Checks a whole document: parent fields, child tables and child rows. Returns the number of
/// child rows.
pub(crate) fn check_document(
    catalog: &Catalog,
    def: &EntityDef,
    doc: &DocumentData,
) -> StorageResult<u32> {
    check_fields(def, &doc.fields)?;
    let mut ids = BTreeSet::from([doc.id]);
    let mut rows = 0u32;
    for (table, children) in &doc.children {
        let child_def = child_entity(catalog, def, table)?;
        for row in children {
            check_row(child_def, row)?;
            if !ids.insert(row.id) {
                return Err(StorageError::new(
                    ErrorKind::AlreadyExists,
                    format!(
                        "record id {} is used twice in one `{}` document",
                        row.id, def.name
                    ),
                )
                .with_entity(&child_def.name));
            }
            rows = rows.saturating_add(1);
        }
    }
    Ok(rows)
}

/// The child entity stored under the child-table field `table` of `def`.
pub(crate) fn child_entity<'c>(
    catalog: &'c Catalog,
    def: &EntityDef,
    table: &Name,
) -> StorageResult<&'c EntityDef> {
    let Ok(t) = def.child_table(table) else {
        return Err(invalid(
            def,
            table,
            format!("`{}` has no child table `{table}`", def.name),
        ));
    };
    catalog.entity(&t.entity)
}

fn check_row(def: &EntityDef, row: &ChildRow) -> StorageResult<()> {
    check_fields(def, &row.fields)
}

fn invalid(def: &EntityDef, field: &Name, msg: String) -> StorageError {
    StorageError::new(ErrorKind::InvalidValue, msg)
        .with_entity(&def.name)
        .with_field(field)
}

/// `AlreadyExists` for a record id.
pub(crate) fn already_exists(entity: &Name, id: RecordId, why: &str) -> StorageError {
    StorageError::new(
        ErrorKind::AlreadyExists,
        format!("`{entity}` record {id} {why}"),
    )
    .with_entity(entity)
}

/// Field values as stored: instants in UTC.
pub(crate) fn stored(fields: &Fields) -> Fields {
    fields
        .iter()
        .map(|(k, v)| {
            let v = match v {
                Value::DateTime(t) => Value::DateTime(t.to_utc().into()),
                other => other.clone(),
            };
            (k.clone(), v)
        })
        .collect()
}
