//! The document as the triggering user sees it, the server's diff, and applying the
//! endpoint's and the scripts' patches.

use super::ids::stable_id;
use super::*;

/// The document as the triggering user sees it: hidden fields and hidden child columns removed.
pub(super) fn visible_document(
    def: &DocTypeDef,
    doc: &Document,
    meta: &dyn FormMeta,
    security: &dyn FormSecurity,
    principal: &Principal,
    facts: &DocFacts,
) -> Json {
    let mut object = Map::new();
    object.insert("id".to_owned(), Json::String(doc.id().to_string()));
    if let Some(version) = doc.version() {
        object.insert("version".to_owned(), json!(version.0));
    }
    if let Some(state) = doc.state() {
        object.insert("_state".to_owned(), Json::String(state.as_str().to_owned()));
    }
    for field in &def.fields {
        let name = field.name.as_str();
        if !security
            .access(def.name.as_str(), name, principal, facts)
            .visible
        {
            continue;
        }
        match &field.kind {
            FieldKind::Table { doctype } | FieldKind::TableMultiselect { doctype, .. } => {
                let child = meta.doctype(doctype.as_str());
                let rows: Vec<Json> = doc
                    .rows(name)
                    .iter()
                    .map(|row| {
                        let mut entry = Map::new();
                        entry.insert("id".to_owned(), Json::String(row.id.to_string()));
                        let row_facts = row_facts(doctype.as_str(), row);
                        if let Some(child) = &child {
                            for column in &child.fields {
                                if !security
                                    .access(
                                        doctype.as_str(),
                                        column.name.as_str(),
                                        principal,
                                        &row_facts,
                                    )
                                    .visible
                                {
                                    continue;
                                }
                                entry.insert(
                                    column.name.as_str().to_owned(),
                                    value_json(
                                        row.fields
                                            .get(&column.name)
                                            .unwrap_or(&spot_app_api::prelude::Value::Null),
                                    ),
                                );
                            }
                        }
                        Json::Object(entry)
                    })
                    .collect();
                object.insert(name.to_owned(), Json::Array(rows));
            }
            _ => {
                object.insert(name.to_owned(), value_json(doc.get(name)));
            }
        }
    }
    Json::Object(object)
}

/// The server's diff between `before` and `after`, over what the user may read.
pub(super) fn document_diff(
    def: &DocTypeDef,
    before: &Document,
    after: &Document,
    meta: &dyn FormMeta,
    security: &dyn FormSecurity,
    principal: &Principal,
    facts: &DocFacts,
) -> Json {
    let mut patch = Map::new();
    for field in &def.fields {
        let name = field.name.as_str();
        if !security
            .access(def.name.as_str(), name, principal, facts)
            .visible
        {
            continue;
        }
        match &field.kind {
            FieldKind::Table { doctype } | FieldKind::TableMultiselect { doctype, .. } => {
                let old = before.rows(name);
                let new = after.rows(name);
                if same_rows(old, new) {
                    continue;
                }
                let child = meta.doctype(doctype.as_str());
                let rows: Vec<Json> = new
                    .iter()
                    .enumerate()
                    .map(|(index, row)| {
                        let mut entry = Map::new();
                        entry.insert("id".to_owned(), Json::String(row.id.to_string()));
                        entry.insert("idx".to_owned(), Json::from(index as u64 + 1));
                        let row_facts = row_facts(doctype.as_str(), row);
                        if let Some(child) = &child {
                            for column in &child.fields {
                                if !security
                                    .access(
                                        doctype.as_str(),
                                        column.name.as_str(),
                                        principal,
                                        &row_facts,
                                    )
                                    .visible
                                {
                                    continue;
                                }
                                entry.insert(
                                    column.name.as_str().to_owned(),
                                    value_json(
                                        row.fields
                                            .get(&column.name)
                                            .unwrap_or(&spot_app_api::prelude::Value::Null),
                                    ),
                                );
                            }
                        }
                        Json::Object(entry)
                    })
                    .collect();
                patch.insert(name.to_owned(), Json::Array(rows));
            }
            _ => {
                let old = before.get(name);
                let new = after.get(name);
                if old != new {
                    patch.insert(name.to_owned(), value_json(new));
                }
            }
        }
    }
    Json::Object(patch)
}

/// Whether two row lists are the same, ids and values included, in order.
pub(super) fn same_rows(old: &[ChildRow], new: &[ChildRow]) -> bool {
    old.len() == new.len()
        && old
            .iter()
            .zip(new)
            .all(|(a, b)| a.id == b.id && a.fields == b.fields)
}

/// Applies the endpoint's patch to the draft.
pub(super) fn apply_patch(
    document: &Document,
    def: &DocTypeDef,
    patch: &Json,
) -> Result<Document, String> {
    if patch.is_null() || patch.as_object().is_some_and(Map::is_empty) {
        return Ok(document.clone());
    }
    let seed = format!("form_event|{}", document.id());
    let mut next = 0_u64;
    let patch = patch_from_json(def, patch, || {
        let id = stable_id(&format!("{seed}|{next}"));
        next += 1;
        id
    })
    .map_err(|e| e.message.clone())?;
    let mut document = document.clone();
    document
        .apply_patch(&patch, RecordId::new_v7)
        .map_err(|e| e.message.clone())?;
    Ok(document)
}

/// Applies one script's patch. New rows get ids derived from the script and the row's position,
/// never from a clock, so the same request gives the same document.
pub(super) fn apply_script_patch(
    document: &Document,
    def: &DocTypeDef,
    patch: &Json,
    script: &str,
) -> Result<Document, String> {
    let seed = format!("{script}|{}", document.id());
    let mut next = 0_u64;
    let patch = patch_from_json(def, patch, || {
        let id = stable_id(&format!("{seed}|{next}"));
        next += 1;
        id
    })
    .map_err(|e| e.message.clone())?;
    let mut document = document.clone();
    document
        .apply_patch(&patch, RecordId::new_v7)
        .map_err(|e| e.message.clone())?;
    Ok(document)
}

/// The merged patch: the endpoint's members, overridden by the scripts' members.
pub(super) fn merge_patches(server: &Json, script: &Json) -> Json {
    let mut merged = server.as_object().cloned().unwrap_or_default();
    if let Some(object) = script.as_object() {
        for (key, value) in object {
            merged.insert(key.clone(), value.clone());
        }
    }
    Json::Object(merged)
}

/// Whether the user may see `path`; used to keep a validation message from naming a hidden field.
#[allow(clippy::too_many_arguments)]
pub(super) fn path_visible(
    def: &DocTypeDef,
    document: &Document,
    meta: &dyn FormMeta,
    security: &dyn FormSecurity,
    principal: &Principal,
    facts: &DocFacts,
    path: &str,
) -> bool {
    let Ok(path) = crate::path::TargetPath::parse(path) else {
        return false;
    };
    match (&path.table, path.index) {
        (None, _) => match def.field(&path.field) {
            Some(_) => {
                security
                    .access(def.name.as_str(), &path.field, principal, facts)
                    .visible
            }
            None => false,
        },
        (Some(table), Some(index)) => {
            let Some(table_field) = def.field(table) else {
                return false;
            };
            let child = match &table_field.kind {
                FieldKind::Table { doctype } | FieldKind::TableMultiselect { doctype, .. } => {
                    doctype
                }
                _ => return false,
            };
            if !security
                .access(def.name.as_str(), table, principal, facts)
                .visible
            {
                return false;
            }
            let Some(row) = document.rows(table).get(index - 1) else {
                return false;
            };
            let Some(child_def) = meta.doctype(child.as_str()) else {
                return false;
            };
            if child_def.field(&path.field).is_none() {
                return false;
            }
            let facts = row_facts(child.as_str(), row);
            security
                .access(child.as_str(), &path.field, principal, &facts)
                .visible
        }
        (Some(_), None) => false,
    }
}

pub(super) fn row_facts(child: &str, row: &ChildRow) -> DocFacts {
    DocFacts {
        doctype: child.to_owned(),
        values: row
            .fields
            .iter()
            .map(|(name, value)| (name.as_str().to_owned(), value.clone()))
            .collect(),
        ..DocFacts::default()
    }
}

/// Whether the user may write every field a script's returned patch names.
///
/// A key must be a field of the DocType that the user can see **and** write; for a table, every
/// row key must likewise be a visible, writable column of the child DocType (judged on the
/// existing row with the same id, or on an empty row for a new one). An unknown, hidden or
/// read-only key fails the whole patch: the answer is one bit, so it cannot say which. `id`,
/// `version` and `_state` are ignored by the patch application and always pass. A patch that
/// is not an object is left to the application, which refuses it.
pub(super) fn patch_is_writable(
    def: &DocTypeDef,
    document: &Document,
    meta: &dyn FormMeta,
    security: &dyn FormSecurity,
    principal: &Principal,
    facts: &DocFacts,
    patch: &Json,
) -> bool {
    let Some(object) = patch.as_object() else {
        return true;
    };
    for (name, value) in object {
        if matches!(name.as_str(), "id" | "version" | "_state") {
            continue;
        }
        let Some(field) = def.field(name) else {
            return false;
        };
        let access = security.access(def.name.as_str(), name, principal, facts);
        if !access.visible || !access.writable {
            return false;
        }
        let (FieldKind::Table { doctype } | FieldKind::TableMultiselect { doctype, .. }) =
            &field.kind
        else {
            continue;
        };
        let Some(rows) = value.as_array() else {
            continue; // the application refuses a non-array table value
        };
        let Some(child) = meta.doctype(doctype.as_str()) else {
            return false;
        };
        for row in rows {
            let Some(entries) = row.as_object() else {
                continue;
            };
            let existing = entries
                .get("id")
                .and_then(Json::as_str)
                .and_then(|id| document.rows(name).iter().find(|r| r.id.to_string() == id));
            let facts = existing.map_or_else(
                || DocFacts {
                    doctype: doctype.as_str().to_owned(),
                    ..DocFacts::default()
                },
                |row| row_facts(doctype.as_str(), row),
            );
            for column in entries.keys() {
                if column == "id" {
                    continue;
                }
                if child.field(column).is_none() {
                    return false;
                }
                let access = security.access(doctype.as_str(), column, principal, &facts);
                if !access.visible || !access.writable {
                    return false;
                }
            }
        }
    }
    true
}
