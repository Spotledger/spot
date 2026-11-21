//! The only changes made to a document's values before validation.

use spot_app_api::prelude::{Decimal, Name, Value};
use spot_app_api::{AppResult, Document};
use spot_meta::{EffectiveMeta, FieldSpec, FieldType};

use crate::lookup::resolve;

/// Applies the coercion table to every field and child row of `doc`.
///
/// There are exactly two rules, and each is exact (no digit is added, dropped or rounded):
///
/// | Field type | Value found | Becomes |
/// | --- | --- | --- |
/// | `decimal` | `Int(n)` | `Decimal(n)` |
/// | `int` | `Decimal(d)` with no fractional part | `Int(d)`, if it fits 64 bits |
///
/// Everything else is left as it is: text is not trimmed and keeps its case, a `Decimal` with a
/// fractional part stays a `Decimal` (so an int field refuses it in validation), and a value of
/// the wrong type stays wrong. Fields the DocType does not declare are not touched.
///
/// # Errors
///
/// `meta.unknown_doctype` if the document's DocType is not in `meta`.
pub fn normalise(meta: &EffectiveMeta, doc: &mut Document) -> AppResult<()> {
    let resolved = resolve(meta, doc.doctype().as_str())?;
    let changes: Vec<(Name, Value)> = doc
        .fields()
        .iter()
        .filter_map(|(key, value)| {
            let spec = resolved
                .def
                .fields
                .iter()
                .find(|f| f.name == key.as_str())?;
            coerce(spec, value).map(|v| (key.clone(), v))
        })
        .collect();
    for (key, value) in changes {
        doc.set(key, value);
    }
    for (table, child) in &resolved.tables {
        if doc.rows(&table.name).is_empty() {
            continue;
        }
        let table_name = crate::lookup::name(&table.name)?;
        for row in doc.rows_mut(table_name) {
            for (key, value) in &mut row.fields {
                if let Some(spec) = child.fields.iter().find(|f| f.name == key.as_str())
                    && let Some(coerced) = coerce(spec, value)
                {
                    *value = coerced;
                }
            }
        }
    }
    Ok(())
}

/// The coerced form of `value` for `spec`, or `None` when the table has no rule for it.
fn coerce(spec: &FieldSpec, value: &Value) -> Option<Value> {
    match (spec.ty, value) {
        (FieldType::Decimal, Value::Int(n)) => Some(Value::Decimal(Decimal::from(*n))),
        (FieldType::Int, Value::Decimal(d)) if d.fract().is_zero() => {
            i64::try_from(*d).ok().map(Value::Int)
        }
        _ => None,
    }
}
