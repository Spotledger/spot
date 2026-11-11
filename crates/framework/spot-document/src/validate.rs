//! Validation of a document against its effective metadata.

use std::collections::BTreeMap;

use spot_app_api::error::FieldPath;
use spot_app_api::prelude::{Name, Value};
use spot_app_api::{AppError, AppResult, Document};
use spot_meta::{DOC_STATE, Definition, EffectiveMeta, FieldSpec, FieldType};

use crate::lookup::resolve;

/// A value that does not fit its field: wrong type, too many decimal places, too many digits.
pub const INVALID_VALUE: &str = "document.invalid_value";
/// A required field (or a required table) has no value.
pub const REQUIRED: &str = "document.required";
/// The document holds a field or table the DocType does not declare.
pub const UNKNOWN_FIELD: &str = "document.unknown_field";
/// A text value is longer than the field's `max_chars`.
pub const TOO_LONG: &str = "document.too_long";
/// A select value is not one of the field's options.
pub const INVALID_OPTION: &str = "document.invalid_option";
/// The caller set a field only the engine may set (`doc_state`).
pub const READ_ONLY_FIELD: &str = "document.read_only_field";

/// One problem found.
struct Problem {
    path: FieldPath,
    code: &'static str,
    message: String,
}

/// Checks `doc` against the effective metadata of its DocType and reports **every** problem in
/// one error: `document.validation_failed`, class `Validation`, with one field error per problem
/// and the exact path (`customer`, `items[2].qty`, rows counted from 1).
///
/// What is checked, in each row of each table as well as on the document itself:
///
/// - the value's type fits the field ([`INVALID_VALUE`]);
/// - a decimal has no more decimal places than the field's scale and no more integer digits than
///   its precision allows: **never rounded** ([`INVALID_VALUE`], ADR-12);
/// - text is at most `max_chars` long ([`TOO_LONG`]);
/// - a select value is one of the options ([`INVALID_OPTION`]);
/// - a required field is not null ([`REQUIRED`]); a required table has at least one row;
/// - nothing undeclared is present ([`UNKNOWN_FIELD`]); a table's name is not used as a value
///   and a plain field's name is not used as a table ([`INVALID_VALUE`]);
/// - the engine-managed field `doc_state` is not set by the caller ([`READ_ONLY_FIELD`]): the
///   state belongs to the document ([`Document::state`]), not to its fields.
///
/// The check is pure: it changes nothing. Run [`coerce::normalise`](crate::coerce::normalise)
/// first when the values come from outside.
///
/// # Errors
///
/// The validation error above, or `meta.unknown_doctype` if the DocType is not in `meta`.
pub fn check(meta: &EffectiveMeta, doc: &Document) -> AppResult<()> {
    let resolved = resolve(meta, doc.doctype().as_str())?;
    let mut problems = Vec::new();
    check_values(
        resolved.def,
        doc.fields(),
        &|f| FieldPath::field(f),
        true,
        &mut problems,
    );

    for key in doc.tables().keys() {
        if resolved.table(key.as_str()).is_none() {
            let (code, message) = if resolved.def.fields.iter().any(|f| f.name == key.as_str()) {
                (
                    INVALID_VALUE,
                    format!(
                        "{} is not a table, so it cannot hold rows.",
                        label_of(resolved.def, key)
                    ),
                )
            } else {
                (UNKNOWN_FIELD, unknown(resolved.def, key))
            };
            problems.push(Problem {
                path: FieldPath::field(key.as_str()),
                code,
                message,
            });
        }
    }
    for (table, child) in &resolved.tables {
        let rows = doc.rows(&table.name);
        if table.required && rows.is_empty() {
            problems.push(Problem {
                path: FieldPath::field(&table.name),
                code: REQUIRED,
                message: format!("{} needs at least one row.", table.label_or_name()),
            });
        }
        for (i, row) in rows.iter().enumerate() {
            let index = u32::try_from(i + 1).unwrap_or(u32::MAX);
            let path = |field: &str| FieldPath::row(&table.name, index, field);
            check_values(child, &row.fields, &path, false, &mut problems);
        }
    }

    let mut problems = problems.into_iter();
    let Some(first) = problems.next() else {
        return Ok(());
    };
    let mut error = AppError::field(first.path, first.code, first.message);
    let mut count = 1;
    for p in problems {
        error = error.with_field(p.path, p.code, p.message);
        count += 1;
    }
    if count > 1 {
        error.message = format!("{count} fields are invalid.");
    }
    Err(error)
}

fn label_of(def: &Definition, name: &Name) -> String {
    def.fields
        .iter()
        .find(|f| f.name == name.as_str())
        .map_or_else(|| name.to_string(), |f| f.label_or_name().to_owned())
}

fn unknown(def: &Definition, name: &Name) -> String {
    format!("There is no field `{name}` on {}.", def.label_or_name())
}

/// Checks the values of one record (the document or one row) against `def`'s scalar fields.
fn check_values(
    def: &Definition,
    values: &BTreeMap<Name, Value>,
    path: &dyn Fn(&str) -> FieldPath,
    top_level: bool,
    problems: &mut Vec<Problem>,
) {
    for key in values.keys() {
        let spec = def.fields.iter().find(|f| f.name == key.as_str());
        let problem = match spec {
            _ if top_level && key.as_str() == DOC_STATE => Some((
                READ_ONLY_FIELD,
                "The document state is managed by the engine and cannot be set; post, cancel or amend the document instead.".to_owned(),
            )),
            None => Some((UNKNOWN_FIELD, unknown(def, key))),
            Some(f) if f.ty.is_table() => Some((
                INVALID_VALUE,
                format!("{} is a table: it holds rows, not a value.", f.label_or_name()),
            )),
            Some(f) if f.ty == FieldType::Secret => Some((
                INVALID_VALUE,
                format!(
                    "{} is a secret: the secrets service holds it, not the document.",
                    f.label_or_name()
                ),
            )),
            Some(_) => None,
        };
        if let Some((code, message)) = problem {
            problems.push(Problem {
                path: path(key.as_str()),
                code,
                message,
            });
        }
    }
    for spec in def
        .fields
        .iter()
        .filter(|f| crate::mapping::has_column(f) && !(top_level && f.name == DOC_STATE))
    {
        let value = values
            .iter()
            .find(|(k, _)| k.as_str() == spec.name)
            .map_or(&Value::Null, |(_, v)| v);
        if let Some((code, message)) = check_value(spec, value) {
            problems.push(Problem {
                path: path(&spec.name),
                code,
                message,
            });
        }
    }
}

/// The first problem with `value` in `spec`, as `(code, message)`.
fn check_value(spec: &FieldSpec, value: &Value) -> Option<(&'static str, String)> {
    let label = spec.label_or_name();
    if value.is_null() {
        return spec
            .required
            .then(|| (REQUIRED, format!("{label} is required.")));
    }
    if !fits(spec.ty, value) {
        return Some((
            INVALID_VALUE,
            format!(
                "{label} must be {}, but it holds {}.",
                expected(spec.ty),
                described(value)
            ),
        ));
    }
    match value {
        Value::Decimal(d) => decimal_problem(spec, label, *d),
        Value::Text(text) => match spec.ty {
            FieldType::Text => {
                let max = spec.max_chars?;
                let length = text.chars().count();
                (length > max as usize).then(|| {
                    (
                        TOO_LONG,
                        format!("{label} can have at most {max} characters, but it has {length}."),
                    )
                })
            }
            FieldType::Select => {
                let options = spec.select_values();
                (!options.contains(text)).then(|| {
                    (
                        INVALID_OPTION,
                        format!(
                            "'{text}' is not an allowed value for {label}; choose one of: {}.",
                            options.join(", ")
                        ),
                    )
                })
            }
            _ => None,
        },
        _ => None,
    }
}

/// A decimal with more decimal places than the scale, or more integer digits than the precision
/// leaves room for. Trailing zeros are not digits: `1.50` fits scale 1.
fn decimal_problem(
    spec: &FieldSpec,
    label: &str,
    d: spot_app_api::prelude::Decimal,
) -> Option<(&'static str, String)> {
    let scale = spec.scale.unwrap_or(0);
    let precision = spec.precision.unwrap_or(18);
    if d.normalize().scale() > u32::from(scale) {
        return Some((
            INVALID_VALUE,
            format!(
                "{label} can have at most {scale} decimal places, but {d} has more; round it before saving."
            ),
        ));
    }
    let integer_digits = d.trunc().abs().to_string().trim_start_matches('0').len();
    let room = usize::from(precision.saturating_sub(scale));
    (integer_digits > room).then(|| {
        (
            INVALID_VALUE,
            format!(
                "{label} is too large: at most {room} digits are allowed before the decimal point."
            ),
        )
    })
}

/// Whether a non-null value has the right type for the field type.
fn fits(ty: FieldType, value: &Value) -> bool {
    matches!(
        (ty, value),
        (FieldType::Bool, Value::Bool(_))
            | (FieldType::Int, Value::Int(_))
            | (FieldType::Decimal, Value::Decimal(_))
            | (
                FieldType::Text | FieldType::LongText | FieldType::Select,
                Value::Text(_)
            )
            | (FieldType::Date, Value::Date(_))
            | (FieldType::Time, Value::Time(_))
            | (FieldType::DateTime, Value::DateTime(_))
            | (
                FieldType::Link | FieldType::DynamicLink | FieldType::Attach,
                Value::Id(_)
            )
            | (FieldType::Json, Value::Json(_))
            | (FieldType::Bytes, Value::Bytes(_))
            | (FieldType::Geo, Value::GeoPoint(_))
            | (FieldType::Range, Value::Range(_))
    )
}

fn expected(ty: FieldType) -> &'static str {
    match ty {
        FieldType::Bool => "true or false",
        FieldType::Int => "a whole number",
        FieldType::Decimal => "a decimal number",
        FieldType::Text | FieldType::LongText | FieldType::Select => "text",
        FieldType::Date => "a date",
        FieldType::Time => "a time of day",
        FieldType::DateTime => "a date and time",
        FieldType::Link | FieldType::DynamicLink => "a link to another document",
        FieldType::Attach => "a link to a file record",
        FieldType::Json => "JSON",
        FieldType::Bytes => "binary data",
        FieldType::Geo => "a geo point",
        FieldType::Range => "a range",
        FieldType::Table | FieldType::TableMultiselect => "rows",
        FieldType::Secret => "nothing (secrets are not stored on documents)",
    }
}

fn described(value: &Value) -> &'static str {
    match value {
        Value::Null => "nothing",
        Value::Bool(_) => "true or false",
        Value::Int(_) => "a whole number",
        Value::Decimal(_) => "a decimal number",
        Value::Text(_) => "text",
        Value::Date(_) => "a date",
        Value::Time(_) => "a time of day",
        Value::DateTime(_) => "a date and time",
        Value::Id(_) => "a link",
        Value::Json(_) => "JSON",
        Value::Bytes(_) => "binary data",
        Value::GeoPoint(_) => "a geo point",
        Value::Range(_) => "a range",
    }
}

#[cfg(test)]
mod geo_range_tests {
    use super::*;

    #[test]
    fn fits_accepts_only_the_matching_value() {
        assert!(!fits(FieldType::Geo, &Value::Text("x".into())));
        assert!(!fits(FieldType::Range, &Value::Int(1)));
        assert!(!fits(FieldType::Geo, &Value::Int(1)));
        assert_eq!(expected(FieldType::Geo), "a geo point");
        assert_eq!(expected(FieldType::Range), "a range");
    }
}
