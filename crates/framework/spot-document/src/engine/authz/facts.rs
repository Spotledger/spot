//! The facts the permission core decides over, built from the engine's documents: the
//! `DocFacts` a `can` question needs, the `DocumentData` a link check needs, the field changes
//! a patch carries, and the read view (hidden fields gone, masked fields masked).
//!
//! Everything here is a pure conversion between the app API's [`Document`] and the permission
//! core's plain value bags (WP-PLT-03.T2, ADR-070 section 12). No storage, no clock.

use std::collections::BTreeMap;

use spot_app_api::document::ChildRow;
use spot_app_api::prelude::{Name, Value};
use spot_app_api::{AppResult, DocTypeDef, Document, DocumentPatch, FieldKind};
use spot_permissions::{
    DocFacts, DocumentData, FieldFacts, FieldLevel, PermissionEngine, Principal, mask,
};
/// The field the owner rules compare with the acting user's name. A DocType that declares no
/// such field has no owner (an `if_owner` grant then never applies).
pub const CREATED_BY: &str = "created_by";

/// One field a create, update or rename writes: its path (`field`, or `table[idx].field`) and
/// whether the payload sets it (`true`) or echoes the stored value back unchanged (`false`).
///
/// An `Unchanged` echo is accepted for a hidden or masked field so edit forms keep working
/// (ADR-070 section 12); the authz module turns these into the permission core's
/// [`FieldChange`](spot_permissions::FieldChange). A child-row path also names the child
/// DocType the field belongs to, because a table field's name is not its child DocType's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriteChange {
    /// The field path.
    pub path: String,
    /// Whether the payload sets the field.
    pub set: bool,
    /// The child DocType of a `table[idx].field` path.
    pub child: Option<String>,
}

impl WriteChange {
    /// A top-level field the payload sets.
    pub fn set(path: impl Into<String>) -> WriteChange {
        WriteChange {
            path: path.into(),
            set: true,
            child: None,
        }
    }

    /// A child-row field the payload sets, with the child DocType it belongs to.
    fn row(path: String, child: String) -> WriteChange {
        WriteChange {
            path,
            set: true,
            child: Some(child),
        }
    }
}

/// The permission-relevant facts of a document: its DocType, id, owner, state and values.
#[must_use]
pub fn facts_of(doc: &Document) -> DocFacts {
    DocFacts {
        doctype: doc.doctype().as_str().to_owned(),
        id: (!doc.is_new()).then(|| doc.id().to_string()),
        created_by: created_by(doc),
        state: doc.state().map(|s| s.as_str().to_owned()),
        values: values_of(doc),
    }
}

/// Who created the document, read from its [`CREATED_BY`] field when it has one.
#[must_use]
pub fn created_by(doc: &Document) -> Option<String> {
    doc.fields()
        .iter()
        .find(|(name, _)| name.as_str() == CREATED_BY)
        .and_then(|(_, value)| match value {
            Value::Text(text) => Some(text.clone()),
            _ => None,
        })
}

fn values_of(doc: &Document) -> BTreeMap<String, Value> {
    doc.fields()
        .iter()
        .map(|(name, value)| (name.as_str().to_owned(), value.clone()))
        .collect()
}

/// The document as the link checker sees it: its top-level values and its child rows.
#[must_use]
pub fn data_of(doc: &Document) -> DocumentData {
    DocumentData {
        fields: values_of(doc),
        tables: doc
            .tables()
            .iter()
            .map(|(table, rows)| {
                let rows: Vec<BTreeMap<String, Value>> = rows
                    .iter()
                    .map(|row| {
                        row.fields
                            .iter()
                            .map(|(name, value)| (name.as_str().to_owned(), value.clone()))
                            .collect()
                    })
                    .collect();
                (table.as_str().to_owned(), rows)
            })
            .collect(),
        id: (!doc.is_new()).then(|| doc.id()),
        dynamic_targets: BTreeMap::new(),
    }
}

/// Every field a create sets: every value the caller supplied, and every child-row field.
#[must_use]
pub fn create_changes(doc: &Document, def: &DocTypeDef) -> Vec<WriteChange> {
    let mut out = Vec::new();
    for name in doc.fields().keys() {
        out.push(WriteChange::set(name.as_str()));
    }
    for (table, rows) in doc.tables() {
        let child = child_doctype(def, table.as_str());
        out.push(WriteChange::set(table.as_str()));
        row_changes(
            &mut out,
            table.as_str(),
            child,
            rows.iter().map(|r| &r.fields),
        );
    }
    out
}

/// Every field an update sets: the patch's top-level fields and child-row fields.
#[must_use]
pub fn patch_changes(patch: &DocumentPatch, def: &DocTypeDef) -> Vec<WriteChange> {
    let mut out = Vec::new();
    for name in patch.fields.keys() {
        out.push(WriteChange::set(name.as_str()));
    }
    for (table, rows) in &patch.tables {
        let child = child_doctype(def, table.as_str());
        out.push(WriteChange::set(table.as_str()));
        row_changes(
            &mut out,
            table.as_str(),
            child,
            rows.iter().map(|r| &r.fields),
        );
    }
    out
}

/// The child DocType a table field holds rows of, from the DocType's metadata.
#[must_use]
pub fn child_doctype(def: &DocTypeDef, table: &str) -> Option<String> {
    def.fields
        .iter()
        .find(|f| f.name.as_str() == table)
        .and_then(|f| match &f.kind {
            FieldKind::Table { doctype } | FieldKind::TableMultiselect { doctype, .. } => {
                Some(doctype.as_str().to_owned())
            }
            _ => None,
        })
}

fn row_changes<'a, I>(out: &mut Vec<WriteChange>, table: &str, child: Option<String>, rows: I)
where
    I: Iterator<Item = &'a BTreeMap<Name, Value>>,
{
    for (i, fields) in rows.enumerate() {
        for name in fields.keys() {
            if name.as_str() == "id" {
                continue;
            }
            let path = format!("{table}[{}].{}", i + 1, name.as_str());
            out.push(match &child {
                Some(child) => WriteChange::row(path, child.clone()),
                None => WriteChange::set(path),
            });
        }
    }
}

/// The child field facts of a DocType, `plain` when the metadata does not name the field.
#[must_use]
pub fn field_facts(engine: &PermissionEngine, doctype: &str, field: &str) -> FieldFacts {
    engine
        .snapshot()
        .facts(doctype)
        .as_deref()
        .and_then(|facts| facts.field(field))
        .cloned()
        .unwrap_or_else(|| FieldFacts::plain(field))
}

/// The document as `principal` may read it: a field it may not see is absent, a field it may
/// only see masked carries its masked form, and every other field keeps its value. Child rows
/// are filtered field by field against the child DocType's own facts.
///
/// The document must be a saved one (it carries a version): every read path and every write
/// result is. A hidden table (its table field hidden on the parent) is removed whole.
pub fn mask_document(
    engine: &PermissionEngine,
    principal: &Principal,
    doc: &Document,
    def: &DocTypeDef,
) -> AppResult<Document> {
    let doctype = doc.doctype().as_str().to_owned();
    let facts = facts_of(doc);
    let Some(version) = doc.version() else {
        return Ok(doc.clone());
    };

    let mut fields: BTreeMap<Name, Value> = BTreeMap::new();
    for (name, value) in doc.fields() {
        let access = engine.field_access(
            principal,
            &doctype,
            &field_facts(engine, &doctype, name.as_str()),
            &facts,
        );
        match access.level {
            FieldLevel::Hidden => {}
            FieldLevel::Masked(format) => {
                fields.insert(name.clone(), masked(value, &format));
            }
            FieldLevel::Read | FieldLevel::ReadReveal => {
                fields.insert(name.clone(), value.clone());
            }
        }
    }

    let mut tables: BTreeMap<Name, Vec<ChildRow>> = BTreeMap::new();
    for (table, rows) in doc.tables() {
        let table_access = engine.field_access(
            principal,
            &doctype,
            &field_facts(engine, &doctype, table.as_str()),
            &facts,
        );
        if table_access.level == FieldLevel::Hidden {
            continue;
        }
        let child = child_doctype(def, table.as_str()).unwrap_or_else(|| table.as_str().to_owned());
        let mut kept = Vec::with_capacity(rows.len());
        for row in rows {
            let row_facts = DocFacts {
                doctype: child.clone(),
                ..DocFacts::default()
            };
            let mut kept_fields: BTreeMap<Name, Value> = BTreeMap::new();
            for (name, value) in &row.fields {
                let access = engine.field_access(
                    principal,
                    &child,
                    &field_facts(engine, &child, name.as_str()),
                    &row_facts,
                );
                match access.level {
                    FieldLevel::Hidden => {}
                    FieldLevel::Masked(format) => {
                        kept_fields.insert(name.clone(), masked(value, &format));
                    }
                    FieldLevel::Read | FieldLevel::ReadReveal => {
                        kept_fields.insert(name.clone(), value.clone());
                    }
                }
            }
            kept.push(ChildRow {
                id: row.id,
                fields: kept_fields,
            });
        }
        tables.insert(table.clone(), kept);
    }

    Ok(Document::loaded(
        doc.doctype().clone(),
        doc.id(),
        version,
        doc.state(),
        fields,
        tables,
    ))
}

fn masked(value: &Value, format: &spot_permissions::MaskFormat) -> Value {
    match value {
        Value::Text(text) => Value::Text(mask(text, format)),
        // A non-text value has no text form to mask without leaking a shape; it is dropped to
        // null instead (the same choice `spot-graph-query` makes).
        _ => Value::Null,
    }
}
