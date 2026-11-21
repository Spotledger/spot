//! The mapping between a [`Document`] and the storage contract's documents.

use std::collections::BTreeMap;

use spot_app_api::document::ChildRow;
use spot_app_api::error::{AppError, ErrorClass};
use spot_app_api::prelude::{Name, Value};
use spot_app_api::{AppResult, DocState, Document};
use spot_meta::{DOC_STATE, EffectiveMeta, FieldSpec};
use spot_storage_contract::document::{ChildRow as StoredRow, DocumentData};
use spot_storage_contract::{Document as StoredDocument, Fields};

use crate::lookup::{name, resolve};

/// Whether a field is held in a column of its own row: a table or `table_multiselect` holds
/// child rows and a `secret` has no column (ADR-068 §2). A secret's sealed bytes live in the
/// engine-managed columns of [`secret_columns`], never in a document.
pub(crate) fn has_column(f: &FieldSpec) -> bool {
    !f.ty.is_table() && f.ty != spot_meta::FieldType::Secret
}

/// The engine-managed storage columns of one `secret` field: `<field>__sealed` and, for a
/// `lookup = true` field, `<field>__bidx` (WP-SEC-03.T2).
pub(crate) fn secret_columns(spec: &FieldSpec) -> AppResult<Vec<Name>> {
    let mut columns = vec![name(&spot_meta::sealed_column(&spec.name))?];
    if spec.lookup_index() {
        columns.push(name(&spot_meta::blind_index_column(&spec.name))?);
    }
    Ok(columns)
}

/// The `secret` fields of a definition, in field order.
pub(crate) fn secret_fields(def: &spot_meta::Definition) -> impl Iterator<Item = &FieldSpec> {
    def.fields
        .iter()
        .filter(|f| f.ty == spot_meta::FieldType::Secret)
}

/// The storage text of a document state: the `draft`, `posted` or `cancelled` kept in the
/// engine-managed field `doc_state`.
pub fn state_text(state: DocState) -> &'static str {
    state.as_str()
}

/// The state a stored `doc_state` text stands for, or `None` for any other text.
pub fn parse_state(text: &str) -> Option<DocState> {
    [DocState::Draft, DocState::Posted, DocState::Cancelled]
        .into_iter()
        .find(|s| s.as_str() == text)
}

/// The full write of `doc`: every declared field present (`Null` when unset), every declared
/// child table present (empty when the document has no rows), child rows with their ids in
/// order.
///
/// A `secret` field has no value on a document, but storage writes are full, so its sealed and
/// blind-index columns are present here as `Null`. **The engine overwrites them** with the
/// stored bytes (or a freshly sealed value) in its write stage; a caller that writes this
/// straight to storage would unset the secret.
///
/// For a posted DocType, `doc_state` is written from the document's state (a document that has
/// none yet is a draft), never from a field: [`validate::check`](crate::validate::check)
/// refuses a document that sets `doc_state` itself. Fields the DocType does not declare are not
/// written; validate first to have them reported.
///
/// # Errors
///
/// `meta.unknown_doctype` if the document's DocType is not in `meta`.
pub fn to_storage(meta: &EffectiveMeta, doc: &Document) -> AppResult<DocumentData> {
    let resolved = resolve(meta, doc.doctype().as_str())?;
    let mut fields = Fields::new();
    for spec in resolved.def.fields.iter().filter(|f| has_column(f)) {
        let value = if spec.name == DOC_STATE {
            Value::Text(state_text(doc.state().unwrap_or(DocState::Draft)).to_owned())
        } else {
            doc.get(&spec.name).clone()
        };
        fields.insert(name(&spec.name)?, value);
    }
    for spec in secret_fields(resolved.def) {
        for column in secret_columns(spec)? {
            fields.insert(column, Value::Null);
        }
    }
    let mut children = BTreeMap::new();
    for (table, child) in &resolved.tables {
        let mut rows = Vec::new();
        for row in doc.rows(&table.name) {
            let mut values = Fields::new();
            for spec in child.fields.iter().filter(|f| has_column(f)) {
                let key = name(&spec.name)?;
                let value = row.fields.get(&key).cloned().unwrap_or(Value::Null);
                values.insert(key, value);
            }
            for spec in secret_fields(child) {
                for column in secret_columns(spec)? {
                    values.insert(column, Value::Null);
                }
            }
            rows.push(StoredRow {
                id: row.id,
                fields: values,
            });
        }
        children.insert(name(&table.name)?, rows);
    }
    Ok(DocumentData {
        entity: doc.doctype().clone(),
        id: doc.id(),
        fields,
        children,
    })
}

/// The document a stored one stands for, built with [`Document::loaded`], so that change
/// tracking starts from what storage holds.
///
/// `doc_state` becomes the document's state and is not kept as a field. Stored values for fields
/// the metadata does not declare are dropped.
///
/// # Errors
///
/// `meta.unknown_doctype` if the entity is not in `meta`, and an internal
/// `document.invalid_state_value` if `doc_state` holds anything but `draft`, `posted` or
/// `cancelled`.
pub fn from_storage(meta: &EffectiveMeta, stored: StoredDocument) -> AppResult<Document> {
    let StoredDocument { data, version } = stored;
    let resolved = resolve(meta, data.entity.as_str())?;
    let mut fields = BTreeMap::new();
    let mut state = None;
    for spec in resolved.def.fields.iter().filter(|f| has_column(f)) {
        let key = name(&spec.name)?;
        let Some(value) = data.fields.get(&key) else {
            continue;
        };
        if spec.name == DOC_STATE {
            state = match value {
                Value::Text(text) => Some(parse_state(text).ok_or_else(|| bad_state(text))?),
                Value::Null => None,
                _ => return Err(bad_state("(not text)")),
            };
        } else {
            fields.insert(key, value.clone());
        }
    }
    let mut tables: BTreeMap<Name, Vec<ChildRow>> = BTreeMap::new();
    for (table, child) in &resolved.tables {
        let key = name(&table.name)?;
        let rows = data.children.get(&key).map_or(&[][..], Vec::as_slice);
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let mut values = BTreeMap::new();
            for spec in child.fields.iter().filter(|f| has_column(f)) {
                let field = name(&spec.name)?;
                if let Some(value) = row.fields.get(&field) {
                    values.insert(field, value.clone());
                }
            }
            out.push(ChildRow {
                id: row.id,
                fields: values,
            });
        }
        tables.insert(key, out);
    }
    Ok(Document::loaded(
        data.entity,
        data.id,
        version,
        state,
        fields,
        tables,
    ))
}

fn bad_state(found: &str) -> AppError {
    AppError::new(
        ErrorClass::Internal,
        "document.invalid_state_value",
        format!("The stored document state `{found}` is not draft, posted or cancelled."),
    )
}
