//! Structural validation: what makes a received record storable, as opposed to what makes it
//! *true* (ADR-062 G3).
//!
//! The apply path validates **shape** only: the entity exists and is a document entity, the
//! fields are exactly the declared ones, every value fits its declared type, and every child row
//! belongs to a declared child table. It never runs a business rule, a hook, a controller or a
//! script; a value that is the wrong type is refused, a value that is *wrong business* is stored.
//! A missing dependency is a closure problem (ADR-064, WP-SYN-03/04), not a validation one.

use std::collections::BTreeSet;

use spot_storage_contract::{Catalog, DocumentData, EntityDef, EntityKind, Name, Value};

use crate::error::Reason;
use crate::record::{OriginFields, ReceivedRecord};

/// Validates every document in the bundle: the root and its entries, and their child rows.
///
/// # Errors
/// The first structural problem found, as a [`Reason`].
pub fn validate_record(catalog: &Catalog, record: &ReceivedRecord) -> Result<(), Reason> {
    if record.version == 0 {
        return Err(Reason::InvalidIdentifier {
            what: "version".to_owned(),
        });
    }
    if record.seq == 0 {
        return Err(Reason::InvalidIdentifier {
            what: "sequence".to_owned(),
        });
    }
    let mut ids: BTreeSet<spot_storage_contract::RecordId> = BTreeSet::new();
    for doc in record.documents() {
        validate_document(catalog, doc)?;
        for id in document_ids(doc) {
            if !ids.insert(id) {
                return Err(Reason::DuplicateId { id: id.to_string() });
            }
        }
    }
    Ok(())
}

/// Validates one document against the catalog: entity kind, exact fields and value types.
///
/// # Errors
/// The first structural problem found, as a [`Reason`].
pub fn validate_document(catalog: &Catalog, doc: &DocumentData) -> Result<(), Reason> {
    let def = document_entity(catalog, &doc.entity)?;
    check_fields(def, &doc.fields)?;
    for (table, rows) in &doc.children {
        let child_def = child_entity(catalog, def, table)?;
        for row in rows {
            check_fields(child_def, &row.fields)?;
        }
    }
    Ok(())
}

/// Checks that a mirrored record's entity declares the origin fields, so the marking can be
/// written. A non-mirror needs no origin fields.
///
/// # Errors
/// [`Reason::UnknownField`] naming the first origin field the entity does not declare.
pub fn validate_origin_fields(def: &EntityDef, origin: &OriginFields) -> Result<(), Reason> {
    for field in [&origin.node, &origin.id, &origin.version] {
        if def
            .field(&Name::new(field).map_err(|_| Reason::InvalidOriginField {
                field: field.clone(),
            })?)
            .is_err()
        {
            return Err(Reason::UnknownField {
                entity: def.name.as_str().to_owned(),
                field: field.clone(),
            });
        }
    }
    Ok(())
}

/// The document entity `entity`; a child entity is refused (child rows travel inside their
/// parent, ADR-063 L5).
fn document_entity<'c>(catalog: &'c Catalog, entity: &Name) -> Result<&'c EntityDef, Reason> {
    let def = catalog.entity(entity).map_err(|_| Reason::UnknownEntity {
        entity: entity.as_str().to_owned(),
    })?;
    if let EntityKind::Child { .. } = def.kind {
        return Err(Reason::ChildEntity {
            entity: entity.as_str().to_owned(),
        });
    }
    Ok(def)
}

/// The child entity stored under a child-table field of `def`.
fn child_entity<'c>(
    catalog: &'c Catalog,
    def: &EntityDef,
    table: &Name,
) -> Result<&'c EntityDef, Reason> {
    let child = def
        .child_table(table)
        .map_err(|_| Reason::UnknownChildTable {
            entity: def.name.as_str().to_owned(),
            table: table.as_str().to_owned(),
        })?;
    catalog
        .entity(&child.entity)
        .map_err(|_| Reason::UnknownEntity {
            entity: child.entity.as_str().to_owned(),
        })
}

/// Checks that `fields` holds exactly the declared fields, each a value that fits its type.
fn check_fields(def: &EntityDef, fields: &spot_storage_contract::Fields) -> Result<(), Reason> {
    for field in &def.fields {
        let Some(value) = fields.get(&field.name) else {
            return Err(Reason::MissingField {
                entity: def.name.as_str().to_owned(),
                field: field.name.as_str().to_owned(),
            });
        };
        if let Err(why) = field.ty.check(value, field.nullable) {
            return Err(Reason::InvalidValue {
                entity: def.name.as_str().to_owned(),
                field: field.name.as_str().to_owned(),
                why,
            });
        }
        if let Value::Json(text) = value
            && serde_json::from_str::<serde_json::Value>(text).is_err()
        {
            return Err(Reason::InvalidValue {
                entity: def.name.as_str().to_owned(),
                field: field.name.as_str().to_owned(),
                why: "the value is not valid JSON".to_owned(),
            });
        }
    }
    if let Some(extra) = fields.keys().find(|k| def.field(k).is_err()) {
        return Err(Reason::UnknownField {
            entity: def.name.as_str().to_owned(),
            field: extra.as_str().to_owned(),
        });
    }
    Ok(())
}

/// Every record id a document owns: its own and its child rows'.
fn document_ids(doc: &DocumentData) -> Vec<spot_storage_contract::RecordId> {
    let mut ids = vec![doc.id];
    for rows in doc.children.values() {
        ids.extend(rows.iter().map(|row| row.id));
    }
    ids
}
