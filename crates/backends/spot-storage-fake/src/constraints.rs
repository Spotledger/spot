//! Unique indexes and database-checked links.

use std::collections::{BTreeMap, BTreeSet};

use spot_storage_contract::schema::system;
use spot_storage_contract::{
    Catalog, EntityDef, EntityKind, ErrorKind, FieldType, LinkCheck, LinkTarget, Name, RecordId,
    StorageError, StorageResult, Value,
};

use crate::store::{Data, Overlay, Record, View};

/// Checks `record` (id `id`) against every unique index of `def`, over `view`. Keys with a null
/// field never conflict; equality is exact.
pub(crate) fn check_unique(
    view: &View<'_>,
    def: &EntityDef,
    id: RecordId,
    record: &Record,
) -> StorageResult<()> {
    for ix in def.indexes.iter().filter(|ix| ix.unique) {
        let key: Vec<Value> = ix
            .fields
            .iter()
            .map(|f| record.value(id, f).unwrap_or(Value::Null))
            .collect();
        if key.iter().any(Value::is_null) {
            continue;
        }
        let clash = view.rows(&def.name).any(|(other_id, other)| {
            other_id != id
                && ix
                    .fields
                    .iter()
                    .zip(&key)
                    .all(|(f, k)| other.value(other_id, f).as_ref() == Some(k))
        });
        if clash {
            return Err(StorageError::new(
                ErrorKind::UniqueViolation,
                format!(
                    "another `{}` record already has this {} (unique index `{}`)",
                    def.name,
                    ix.fields
                        .iter()
                        .map(Name::as_str)
                        .collect::<Vec<_>>()
                        .join(", "),
                    ix.name
                ),
            )
            .with_entity(&def.name)
            .with_constraint(&ix.name));
        }
    }
    Ok(())
}

/// The database-checked link edges of `def`: (field, target entity). A child entity's `parent`
/// is one of them.
fn checked_links(def: &EntityDef) -> StorageResult<Vec<(Name, Name)>> {
    let mut links: Vec<(Name, Name)> = def
        .fields
        .iter()
        .filter_map(|f| match &f.ty {
            FieldType::Link {
                target: LinkTarget::Entity(t),
                check: LinkCheck::Database,
            } => Some((f.name.clone(), t.clone())),
            _ => None,
        })
        .collect();
    if let EntityKind::Child { parent } = &def.kind {
        links.push((Name::new(system::PARENT)?, parent.clone()));
    }
    Ok(links)
}

fn violation(entity: &Name, field: &Name, msg: String) -> StorageError {
    StorageError::new(ErrorKind::ReferenceViolation, msg)
        .with_entity(entity)
        .with_field(field)
}

/// Deferred link integrity at commit, over `view` (the data as it will be after commit):
/// - every checked link written by the transaction points at an existing record;
/// - no committed record the transaction deletes is still the target of a checked link.
pub(crate) fn check_links(
    catalog: &Catalog,
    committed: &Data,
    overlay: &Overlay,
    view: &View<'_>,
) -> StorageResult<()> {
    let mut deleted: BTreeMap<&Name, BTreeSet<RecordId>> = BTreeMap::new();
    for (entity, records) in &overlay.records {
        let def = catalog.entity(entity)?;
        let links = checked_links(def)?;
        for (id, record) in records {
            let Some(record) = record else {
                if committed.get(entity, *id).is_some() {
                    deleted.entry(entity).or_default().insert(*id);
                }
                continue;
            };
            for (field, target) in &links {
                if let Some(Value::Id(to)) = record.value(*id, field)
                    && view.get(target, to).is_none()
                {
                    return Err(violation(
                        entity,
                        field,
                        format!(
                            "`{entity}.{field}` of record {id} points at `{target}` record {to}, \
                             which does not exist"
                        ),
                    ));
                }
            }
        }
    }
    if deleted.is_empty() {
        return Ok(());
    }
    for def in catalog.entities() {
        let links: Vec<(Name, &BTreeSet<RecordId>)> = checked_links(def)?
            .into_iter()
            .filter_map(|(field, target)| deleted.get(&target).map(|ids| (field, ids)))
            .collect();
        if links.is_empty() {
            continue;
        }
        for (id, record) in view.rows(&def.name) {
            for (field, ids) in &links {
                if let Some(Value::Id(to)) = record.value(id, field)
                    && ids.contains(&to)
                {
                    return Err(violation(
                        &def.name,
                        field,
                        format!(
                            "record {to} cannot be deleted: `{}.{field}` of record {id} still \
                             points at it; storage never cascades deletes",
                            def.name
                        ),
                    ));
                }
            }
        }
    }
    Ok(())
}
