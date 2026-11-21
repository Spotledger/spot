//! Whole documents and child-table rows: `ApiDocument`, `DocumentPatchApi`, `MetaLookup`,
//! and the row-level decode/encode functions. The scalar value rules live in the parent
//! `codec` module; this layer composes them per DocType meta.

use std::collections::BTreeMap;

use serde_json::Value as Json;
use spot_storage_contract::{Name, RecordId, Value, Version};

use crate::api::meta::DocTypeMeta;
use crate::api::registry::Code;
use crate::api::service::{ApiError, FieldIssue};

use super::{
    FieldValue, checked_name, decode_scalar, encode_scalar, invalid_value, parse_record_id,
    reserved_key, valid_write_key,
};

/// One child-table row as the API layer holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChildRowDoc {
    /// The row's record id; `None` in a request body for a row the client does not identify.
    pub id: Option<RecordId>,
    /// The row's field values (`idx` is implicit in the order of the table's vec).
    pub fields: BTreeMap<Name, Value>,
}

/// The currency of one amount (contract 0.5.0 `CurrencyInfo`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CurrencyInfo {
    /// Three upper-case ASCII letters.
    pub code: String,
    /// The symbol the caller's locale shows; at most 8 characters.
    pub symbol: String,
}

/// A document as the API layer holds it: system keys plus the field map. `id` is `None` only
/// in a create request where the client did not choose one; responses always carry it.
#[derive(Clone, Debug, PartialEq)]
pub struct ApiDocument {
    /// The document's id.
    pub id: Option<RecordId>,
    /// The stored version; `None` before the first save.
    pub version: Option<Version>,
    /// The lifecycle state (`_state`) for posted DocTypes, as the contract's string.
    pub state: Option<String>,
    /// The currency each `currency`-format field is shown in, keyed by field path (contract
    /// 0.5.0 `_currency`). Presentation only: encoded on responses, never decoded from a
    /// request, never part of the field map, a patch or a version.
    pub currency: BTreeMap<String, CurrencyInfo>,
    /// Field values by storage name, tables included (order follows the meta's field order on
    /// encode).
    pub fields: BTreeMap<Name, FieldValue>,
}

impl ApiDocument {
    /// An empty document with no id (create-body starting point).
    pub fn empty() -> Self {
        ApiDocument {
            id: None,
            version: None,
            state: None,
            currency: BTreeMap::new(),
            fields: BTreeMap::new(),
        }
    }

    /// A scalar field value; `None` when absent or a table.
    pub fn scalar(&self, field: &str) -> Option<&Value> {
        let (_, value) = self
            .fields
            .iter()
            .find(|(name, _)| name.as_str() == field)?;
        match value {
            FieldValue::Scalar(v) => Some(v),
            FieldValue::Rows(_) => None,
        }
    }

    /// Interpret a decoded request body as a `PATCH`: scalar fields become field sets, tables
    /// become row replacements. `id` is dropped (the router already checked it matches the
    /// path). `_state` never reaches here (the router rejects `_`-keys first).
    pub fn into_patch(self) -> DocumentPatchApi {
        let mut patch = DocumentPatchApi::default();
        for (name, value) in self.fields {
            match value {
                FieldValue::Scalar(v) => {
                    patch.fields.insert(name, v);
                }
                FieldValue::Rows(rows) => {
                    patch.tables.insert(name, rows);
                }
            }
        }
        patch
    }
}

/// The decoded `PATCH` body (merge semantics, ADR-055): fields to set and tables to replace.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DocumentPatchApi {
    /// Fields to set; `Value::Null` sets null. Absent fields keep their values.
    pub fields: BTreeMap<Name, Value>,
    /// Child tables to replace, with the rows in the new order.
    pub tables: BTreeMap<Name, Vec<ChildRowDoc>>,
}

/// Resolve a DocType's metadata; the codec reaches child tables through it. The router
/// builds a `MetaLookup` over the per-request map of the parent's and its children's metas.
#[derive(Clone, Copy, Debug)]
pub struct MetaLookup<'a>(pub &'a std::collections::BTreeMap<String, DocTypeMeta>);

impl MetaLookup<'_> {
    /// Resolve one DocType's meta; unknown → `meta.unknown_doctype`.
    pub fn get(&self, doctype: &str) -> Result<DocTypeMeta, ApiError> {
        self.0
            .get(doctype)
            .cloned()
            .ok_or_else(|| ApiError::from_code(Code::MetaUnknownDoctype))
    }
}

static NO_METAS: std::collections::BTreeMap<String, DocTypeMeta> =
    std::collections::BTreeMap::new();

impl<'a> MetaLookup<'a> {
    /// A lookup that knows no DocTypes (scalar-only tests).
    pub fn none() -> MetaLookup<'static> {
        MetaLookup(&NO_METAS)
    }

    /// A lookup over a map owned by the caller.
    pub fn over(map: &'a std::collections::BTreeMap<String, DocTypeMeta>) -> MetaLookup<'a> {
        MetaLookup(map)
    }
}
/// Decode one child row object. Rows share the write-body rules (no `version`, no `_`-keys);
/// `id` and `idx` are the row system keys.
pub fn decode_row(
    row: &Json,
    table: &str,
    index: usize,
    child_meta: &DocTypeMeta,
) -> Result<ChildRowDoc, ApiError> {
    let Json::Object(map) = row else {
        return Err(ApiError::new(
            Code::ApiInvalidRequest,
            format!("`{table}[{index}]` must be a JSON object."),
        ));
    };
    let mut out = ChildRowDoc {
        id: None,
        fields: BTreeMap::new(),
    };
    let mut unknown: Vec<String> = Vec::new();
    for (key, value) in map {
        if reserved_key(key) {
            return Err(ApiError::new(
                Code::ApiInvalidRequest,
                format!("The key `{key}` is server metadata and cannot be sent in a request."),
            ));
        }
        if !valid_write_key(key) {
            return Err(ApiError::new(
                Code::ApiInvalidRequest,
                format!("`{table}[{index}].{key}` is not a valid field name."),
            ));
        }
        if key == "idx" {
            return Err(ApiError::new(
                Code::ApiInvalidRequest,
                format!("`{table}[{index}].idx` is assigned by the server from the row order."),
            ));
        }
        if key == "id" {
            match value {
                Json::Null => {}
                Json::String(s) => {
                    out.id = Some(parse_record_id(s).ok_or_else(|| {
                        invalid_value(&format!("{table}[{index}].id"), "a canonical UUIDv7")
                    })?);
                }
                _ => {
                    return Err(invalid_value(
                        &format!("{table}[{index}].id"),
                        "a document id string",
                    ));
                }
            }
            continue;
        }
        match child_meta.field(key) {
            None => unknown.push(format!("{table}[{index}].{key}")),
            Some(f) => {
                // Nested tables inside a child row are not part of v0.1's flat document shape;
                // the child field must be a scalar.
                let v = decode_scalar(value, f, &format!("{table}[{index}].{key}"))?;
                out.fields.insert(checked_name(key)?, v);
            }
        }
    }
    if !unknown.is_empty() {
        let mut err = ApiError::new(
            Code::DocumentUnknownField,
            "The child DocType has no such field.",
        );
        for field in &unknown {
            err.fields.push(FieldIssue::new(
                field.clone(),
                "document.unknown_field",
                "The DocType has no such field.",
            ));
        }
        return Err(err);
    }
    Ok(out)
}
/// Encode one child row for a response: `id`, `idx` (from 1), then fields by the child meta.
pub fn encode_row(
    row: &ChildRowDoc,
    index: usize,
    child_meta: &DocTypeMeta,
) -> Result<Json, ApiError> {
    let mut map = serde_json::Map::new();
    map.insert(
        "id".to_owned(),
        Json::String(row.id.map(|i| i.to_string()).unwrap_or_default()),
    );
    map.insert("idx".to_owned(), Json::from(index as u64));
    for (name, value) in &row.fields {
        let field = child_meta.field(name.as_str()).ok_or_else(|| {
            ApiError::new(
                Code::HttpInternal,
                format!("row field `{name}` is not in the child DocType meta."),
            )
        })?;
        map.insert(name.as_str().to_owned(), encode_scalar(value, field)?);
    }
    Ok(Json::Object(map))
}

/// Encode a whole document for a response (the contract's `Document`). `project` restricts the
/// field set for list/query pages (`None` = every field in the meta's order).
pub fn encode_document(
    doc: &ApiDocument,
    meta: &DocTypeMeta,
    project: Option<&[Name]>,
    meta_of: MetaLookup,
) -> Result<Json, ApiError> {
    let mut map = serde_json::Map::new();
    if let Some(id) = doc.id {
        map.insert("id".to_owned(), Json::String(id.to_string()));
    }
    if let Some(version) = doc.version {
        map.insert("version".to_owned(), Json::from(version.0));
    }
    if let Some(state) = &doc.state {
        map.insert("_state".to_owned(), Json::String(state.clone()));
    }
    if !doc.currency.is_empty() {
        map.insert("_currency".to_owned(), encode_currency(&doc.currency)?);
    }
    // Emit in the meta's field order so responses are stable.
    for field in &meta.fields {
        let Some(value) = doc.fields.get(&field.name) else {
            continue;
        };
        if let Some(project) = project
            && !project.iter().any(|p| p.as_str() == field.name.as_str())
        {
            continue;
        }
        match value {
            FieldValue::Scalar(v) => {
                map.insert(field.name.as_str().to_owned(), encode_scalar(v, field)?);
            }
            FieldValue::Rows(rows) => {
                let child_name = field
                    .child_doctype
                    .as_ref()
                    .ok_or_else(|| ApiError::from_code(Code::HttpInternal))?;
                let child_meta = meta_of.get(child_name.as_str())?;
                let mut out = Vec::with_capacity(rows.len());
                for (i, row) in rows.iter().enumerate() {
                    out.push(encode_row(row, i + 1, &child_meta)?);
                }
                map.insert(field.name.as_str().to_owned(), Json::Array(out));
            }
        }
    }
    Ok(Json::Object(map))
}

fn encode_currency(currency: &BTreeMap<String, CurrencyInfo>) -> Result<Json, ApiError> {
    let mut out = serde_json::Map::new();
    for (path, info) in currency {
        let code_ok = info.code.len() == 3 && info.code.bytes().all(|b| b.is_ascii_uppercase());
        if !super::is_field_path(path) || !code_ok || info.symbol.chars().count() > 8 {
            return Err(ApiError::new(
                Code::HttpInternal,
                "a currency entry does not fit the contract's CurrencyInfo.",
            ));
        }
        let mut entry = serde_json::Map::new();
        entry.insert("code".to_owned(), Json::String(info.code.clone()));
        entry.insert("symbol".to_owned(), Json::String(info.symbol.clone()));
        out.insert(path.clone(), Json::Object(entry));
    }
    Ok(Json::Object(out))
}
