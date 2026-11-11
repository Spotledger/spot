//! Walking a document for the link values it holds.
//!
//! A link value lives in a `Link` or `DynamicLink` field, on the document itself or on any row
//! of one of its child tables (a `table` or `table_multiselect` field; the multi-pick's link is
//! the row's own `Link` field). Each value is reported with the exact field path the engine
//! uses in field errors: `customer`, `lines[2].item`.

use std::collections::BTreeMap;

use spot_app_api::document::Document;
use spot_app_api::error::FieldPath;
use spot_app_api::meta::{DocTypeDef, FieldKind};
use spot_app_api::prelude::{Name, RecordId, Value};
use spot_meta::EffectiveMeta;

/// The DocType a link value must point at.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LinkTarget {
    /// A fixed link: the field's `options` names the target DocType.
    Fixed(String),
    /// A dynamic link: the value's own DocType comes from the record's type field. The string
    /// is the type field's text (empty when unset).
    Dynamic(String),
}

/// One link value found in a document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Occurrence {
    /// The exact field path (`customer`, or `lines[2].item`).
    pub(crate) path: FieldPath,
    /// The field holding the link (unqualified).
    pub(crate) field: String,
    /// The value pointed at.
    pub(crate) id: RecordId,
    /// What the value must point at.
    pub(crate) target: LinkTarget,
}

/// Every link value in `doc`, in document order: top-level fields first, then each child table's
/// rows in table order and row order. A null (unset) link is not an occurrence.
pub(crate) fn occurrences(
    meta: &EffectiveMeta,
    def: &DocTypeDef,
    doc: &Document,
) -> Vec<Occurrence> {
    let mut out = Vec::new();
    for f in &def.fields {
        collect(
            f,
            &f.kind,
            doc.fields(),
            FieldPath::field(f.name.as_str()),
            &mut out,
        );
    }
    for (table, child_name) in def.tables() {
        let Some(child) = meta.doctype(child_name.as_str()) else {
            continue;
        };
        for (index, row) in doc.rows(table).iter().enumerate() {
            let idx = u32::try_from(index + 1).unwrap_or(u32::MAX);
            for f in &child.fields {
                collect(
                    f,
                    &f.kind,
                    &row.fields,
                    FieldPath::row(table, idx, f.name.as_str()),
                    &mut out,
                );
            }
        }
    }
    out
}

/// A record's value for `field`, or [`Value::Null`] when it has none.
fn value_of<'a>(values: &'a BTreeMap<Name, Value>, field: &str) -> &'a Value {
    values
        .iter()
        .find(|(k, _)| k.as_str() == field)
        .map_or(&Value::Null, |(_, v)| v)
}

/// Records the link value of one field, when the field is a link and holds a value.
fn collect(
    field: &spot_app_api::meta::FieldDef,
    kind: &FieldKind,
    values: &BTreeMap<Name, Value>,
    path: FieldPath,
    out: &mut Vec<Occurrence>,
) {
    let Value::Id(id) = value_of(values, field.name.as_str()) else {
        return;
    };
    let target = match kind {
        FieldKind::Link { doctype } => LinkTarget::Fixed(doctype.as_str().to_owned()),
        FieldKind::DynamicLink { type_field } => {
            LinkTarget::Dynamic(text_of(value_of(values, type_field.as_str())))
        }
        _ => return,
    };
    out.push(Occurrence {
        path,
        field: field.name.as_str().to_owned(),
        id: *id,
        target,
    });
}

/// A value's text, or the empty string when it is not text.
fn text_of(value: &Value) -> String {
    match value {
        Value::Text(text) => text.clone(),
        _ => String::new(),
    }
}
