//! Default values: [`apply`] fills the fields a caller left unset.

use std::str::FromStr;

use spot_app_api::error::{AppError, ErrorClass};
use spot_app_api::prelude::{Decimal, OffsetDateTime, Value};
use spot_app_api::{AppResult, Document};
use spot_meta::{EffectiveMeta, FieldSpec, FieldType};

use crate::lookup::{name, resolve};
use crate::parse;

/// The dynamic default of a date field: the date of the invocation's `now`.
pub const TODAY: &str = "today";
/// The dynamic default of a time or date-time field: the invocation's `now`.
pub const NOW: &str = "now";

/// Fills every unset field of `doc`, and of its child rows, from the field's `default`.
///
/// - A field is *unset* when the document holds no value for it. A value the caller set, an
///   explicit `Null` included, is never overwritten.
/// - Static defaults are parsed by field type exactly as `spot-meta` validates them.
/// - The dynamic defaults [`TODAY`] (date fields) and [`NOW`] (time and date-time fields) come
///   from `now`, the invocation's business time, never from the system clock, so every field
///   filled in one operation agrees.
///
/// # Errors
///
/// `meta.unknown_doctype` if the document's DocType is not in `meta`, and an internal
/// `document.bad_default` if a stored default cannot be read as its field's type (metadata
/// validates defaults, so this means the metadata is inconsistent).
pub fn apply(meta: &EffectiveMeta, doc: &mut Document, now: OffsetDateTime) -> AppResult<()> {
    let resolved = resolve(meta, doc.doctype().as_str())?;
    for spec in resolved
        .def
        .fields
        .iter()
        .filter(|f| crate::mapping::has_column(f))
    {
        let key = name(&spec.name)?;
        if let Some(default) = &spec.default
            && !doc.fields().contains_key(&key)
        {
            let value = parse_default(resolved.def.name.as_str(), spec, default, now)?;
            doc.set(key, value);
        }
    }
    for (table, child) in &resolved.tables {
        let table_name = name(&table.name)?;
        if doc.rows(&table.name).is_empty() {
            continue;
        }
        for row in doc.rows_mut(table_name) {
            for spec in child
                .fields
                .iter()
                .filter(|f| crate::mapping::has_column(f))
            {
                let key = name(&spec.name)?;
                if let Some(default) = &spec.default
                    && !row.fields.contains_key(&key)
                {
                    let value = parse_default(child.name.as_str(), spec, default, now)?;
                    row.fields.insert(key, value);
                }
            }
        }
    }
    Ok(())
}

fn parse_default(
    doctype: &str,
    spec: &FieldSpec,
    default: &str,
    now: OffsetDateTime,
) -> AppResult<Value> {
    let value = match spec.ty {
        FieldType::Bool => match default {
            "true" => Some(Value::Bool(true)),
            "false" => Some(Value::Bool(false)),
            _ => None,
        },
        FieldType::Int => default.parse::<i64>().ok().map(Value::Int),
        FieldType::Decimal => Decimal::from_str(default).ok().map(Value::Decimal),
        FieldType::Text | FieldType::LongText | FieldType::Select => {
            Some(Value::Text(default.to_owned()))
        }
        FieldType::Date if default == TODAY => Some(Value::Date(now.date())),
        FieldType::Date => parse::date(default).map(Value::Date),
        FieldType::Time if default == NOW => Some(Value::Time(now.time())),
        FieldType::Time => parse::time(default).map(Value::Time),
        FieldType::DateTime if default == NOW => Some(Value::DateTime(now)),
        FieldType::DateTime => parse::datetime(default).map(Value::DateTime),
        FieldType::Link
        | FieldType::DynamicLink
        | FieldType::Json
        | FieldType::Bytes
        // A position or a range has no synthesised default; it falls to the refusal below.
        | FieldType::Geo
        | FieldType::Range
        | FieldType::Attach
        | FieldType::Secret
        | FieldType::Table
        | FieldType::TableMultiselect => None,
    };
    value.ok_or_else(|| {
        AppError::new(
            ErrorClass::Internal,
            "document.bad_default",
            format!(
                "The default `{default}` of `{doctype}.{}` cannot be read as a {:?} value.",
                spec.name, spec.ty
            ),
        )
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn spec(ty: FieldType, default: &str) -> FieldSpec {
        let mut f = FieldSpec::new("f", ty);
        f.default = Some(default.to_owned());
        f
    }

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_790_000_000).unwrap()
    }

    // `spot-meta` does not accept `today` or `now` as defaults yet, so a registry cannot carry
    // them; the dynamic rules are checked on the field spec directly.
    #[test]
    fn dynamic_defaults_come_from_the_given_time() {
        let at = now();
        let get = |ty, d: &str| parse_default("t", &spec(ty, d), d, at).unwrap();
        assert_eq!(get(FieldType::Date, TODAY), Value::Date(at.date()));
        assert_eq!(get(FieldType::DateTime, NOW), Value::DateTime(at));
        assert_eq!(get(FieldType::Time, NOW), Value::Time(at.time()));
    }

    #[test]
    fn the_dynamic_words_mean_nothing_to_other_types_or_formats() {
        let at = now();
        for (ty, d) in [
            (FieldType::Int, "now"),
            (FieldType::Bool, "today"),
            (FieldType::Date, "now"),
            (FieldType::DateTime, "today"),
            (FieldType::Time, "today"),
        ] {
            let e = parse_default("t", &spec(ty, d), d, at).unwrap_err();
            assert_eq!(e.code, "document.bad_default", "{ty:?} {d}");
        }
        // Text keeps the words as text.
        let text = parse_default("t", &spec(FieldType::Text, "today"), "today", at).unwrap();
        assert_eq!(text, Value::Text("today".into()));
    }
}
