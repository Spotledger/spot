//! The Document node's config, and the mapping it evaluates.
//!
//! The config is the compiled node's JSON (the DAG layer keeps node data opaque). It carries:
//!
//! - `operation` (or the process model's `action`): `create_from`, `update`, `submit`, `cancel`
//!   or `map`;
//! - `doctype`: the DocType's storage name;
//! - `mapping`: an object of `field -> expression` (each an ADR-28 expression over the Case's
//!   variables, read under the `doc` root), or a string naming a path in the Case's variables
//!   that holds an object of literal values;
//! - `document`: an expression giving the record id an `update`, `submit` or `cancel` targets;
//! - `expected`: an expression giving the version the caller expects (an `update`, `submit` or
//!   `cancel` loads the current version when this is absent);
//! - `principal`: a named service principal, overriding the Case's actor;
//! - `output`: the variable path the receipt is written to (default: the node id);
//! - `compensation`: the inverse operation the node declares (default: the operation's own
//!   inverse, `cancel` for `submit`).
//!
//! A mapping value may be a typed object `{ "expr": "...", "as": "link" }` when the target field
//! needs a type the expression language does not carry (a `link` is the common case): `as` is one
//! of `text`, `int`, `decimal`, `bool`, `date`, `datetime`, `link` or `json`.

use std::collections::BTreeMap;

use serde_json::Value as Json;
use spot_app_api::prelude::{Date, Decimal, Month, Name, OffsetDateTime, RecordId, Value, Version};
use spot_bpm_engine::NodeCtx;
use spot_expr::Value as ExprValue;

use crate::codes;
use crate::failure::DocumentFailure;
use crate::ids::parse_record_id;
use crate::operation::Operation;

/// A parsed Document node config.
#[derive(Clone, Debug)]
pub(crate) struct NodeConfig {
    pub(crate) operation: Operation,
    pub(crate) doctype: Name,
    mapping: BTreeMap<Name, Mapping>,
    fields_from: Option<String>,
    document: Option<String>,
    principal: Option<String>,
    expected: Option<String>,
    pub(crate) output: String,
    pub(crate) compensation: Option<Operation>,
}

/// One mapping entry: an expression, or an expression with a target type.
#[derive(Clone, Debug)]
enum Mapping {
    /// The expression's value, converted to the storage type it fits.
    Expr(String),
    /// The expression's value, converted to a declared target type.
    Typed { expr: String, target: Target },
}

/// A target field type a mapping may declare.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Target {
    Text,
    Int,
    Decimal,
    Bool,
    Date,
    DateTime,
    Link,
    Json,
}

impl Target {
    fn parse(token: &str) -> Option<Self> {
        match token {
            "text" => Some(Self::Text),
            "int" => Some(Self::Int),
            "decimal" => Some(Self::Decimal),
            "bool" => Some(Self::Bool),
            "date" => Some(Self::Date),
            "datetime" => Some(Self::DateTime),
            "link" => Some(Self::Link),
            "json" => Some(Self::Json),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Int => "int",
            Self::Decimal => "decimal",
            Self::Bool => "bool",
            Self::Date => "date",
            Self::DateTime => "datetime",
            Self::Link => "link",
            Self::Json => "json",
        }
    }
}

impl NodeConfig {
    /// Parses a node config; `default_output` is the node id, the default receipt path.
    pub(crate) fn parse(config: &Json, default_output: &str) -> Result<Self, DocumentFailure> {
        let object = config
            .as_object()
            .ok_or_else(|| config_error("the node config is not an object"))?;
        let operation = match object.get("operation").and_then(Json::as_str) {
            Some(token) => Operation::parse(token)
                .ok_or_else(|| config_error(format!("`{token}` is not a document operation")))?,
            None => match object.get("action").and_then(Json::as_str) {
                Some(action) => Operation::parse(action)
                    .ok_or_else(|| config_error(format!("`{action}` is not a document action")))?,
                None => return Err(config_error("the node names no operation")),
            },
        };
        let doctype_raw = object
            .get("doctype")
            .and_then(Json::as_str)
            .ok_or_else(|| config_error("the node names no DocType"))?;
        let doctype = Name::new(doctype_raw)
            .map_err(|_| config_error(format!("`{doctype_raw}` is not a storage DocType name")))?;
        let (mapping, fields_from) = parse_mapping(object.get("mapping"))?;
        let document = string_field(object, "document");
        let principal = string_field(object, "principal");
        let expected = string_field(object, "expected");
        let output = string_field(object, "output").unwrap_or_else(|| default_output.to_owned());
        let compensation = match object.get("compensation").and_then(Json::as_str) {
            Some(token) => Some(Operation::parse(token).ok_or_else(|| {
                config_error(format!("`{token}` is not a compensation operation"))
            })?),
            None => operation.compensation(),
        };
        Ok(Self {
            operation,
            doctype,
            mapping,
            fields_from,
            document,
            principal,
            expected,
            output,
            compensation,
        })
    }

    /// The named service principal, when the node overrides the Case's actor.
    pub(crate) fn principal(&self) -> Option<&str> {
        self.principal.as_deref()
    }

    /// The field values the mapping produces, for a create or an update.
    pub(crate) fn fields(
        &self,
        ctx: &NodeCtx<'_>,
    ) -> Result<BTreeMap<Name, Value>, DocumentFailure> {
        if let Some(path) = &self.fields_from {
            return self.literal_fields(ctx, path);
        }
        let mut out = BTreeMap::new();
        for (field, mapping) in &self.mapping {
            let value = match mapping {
                Mapping::Expr(expr) => value_of(&evaluate(expr, ctx)?),
                Mapping::Typed { expr, target } => {
                    let evaluated = evaluate(expr, ctx)?;
                    typed_value(&evaluated, *target).ok_or_else(|| {
                        DocumentFailure::new(
                            codes::EXPRESSION_INVALID,
                            format!(
                                "the mapping for `{}` is not {}",
                                field.as_str(),
                                target.name()
                            ),
                        )
                    })?
                }
            };
            out.insert(field.clone(), value);
        }
        Ok(out)
    }

    /// The mapped values as canonical JSON, for a `map` operation that writes no document.
    pub(crate) fn mapping_json(
        &self,
        ctx: &NodeCtx<'_>,
    ) -> Result<BTreeMap<Name, Json>, DocumentFailure> {
        let mut out = BTreeMap::new();
        for (field, mapping) in &self.mapping {
            let expr = match mapping {
                Mapping::Expr(expr) | Mapping::Typed { expr, .. } => expr,
            };
            out.insert(field.clone(), to_json_or_null(&evaluate(expr, ctx)?));
        }
        Ok(out)
    }

    /// The document an `update`, `submit` or `cancel` targets.
    pub(crate) fn target_document(&self, ctx: &NodeCtx<'_>) -> Result<RecordId, DocumentFailure> {
        let expr = self.document.as_ref().ok_or_else(|| {
            DocumentFailure::new(
                codes::DOCUMENT_MISSING,
                "the node names no document to change",
            )
        })?;
        match evaluate(expr, ctx)? {
            ExprValue::Text(text) => parse_record_id(&text).ok_or_else(|| {
                DocumentFailure::new(
                    codes::DOCUMENT_MISSING,
                    format!("`{expr}` did not name a document"),
                )
            }),
            _ => Err(DocumentFailure::new(
                codes::DOCUMENT_MISSING,
                format!("`{expr}` did not name a document"),
            )),
        }
    }

    /// The version the caller expects, when the node names one.
    pub(crate) fn expected_version(
        &self,
        ctx: &NodeCtx<'_>,
    ) -> Result<Option<Version>, DocumentFailure> {
        let Some(expr) = &self.expected else {
            return Ok(None);
        };
        match evaluate(expr, ctx)? {
            ExprValue::Int(value) => {
                u64::try_from(value)
                    .ok()
                    .map(Version)
                    .map(Some)
                    .ok_or_else(|| {
                        DocumentFailure::new(
                            codes::EXPRESSION_INVALID,
                            format!("`{expr}` is not a version"),
                        )
                    })
            }
            _ => Err(DocumentFailure::new(
                codes::EXPRESSION_INVALID,
                format!("`{expr}` is not a version"),
            )),
        }
    }

    /// The fields from a path in the Case's variables that holds an object of literals.
    fn literal_fields(
        &self,
        ctx: &NodeCtx<'_>,
        path: &str,
    ) -> Result<BTreeMap<Name, Value>, DocumentFailure> {
        let object = ctx
            .variable(path)
            .and_then(Json::as_object)
            .ok_or_else(|| {
                DocumentFailure::new(
                    codes::CONFIG_INVALID,
                    format!("the mapping path `{path}` holds no object"),
                )
            })?;
        let mut out = BTreeMap::new();
        for (field, value) in object {
            let name = Name::new(field)
                .map_err(|_| config_error(format!("`{field}` is not a storage field name")))?;
            let converted = json_to_value(value).ok_or_else(|| {
                DocumentFailure::new(
                    codes::CONFIG_INVALID,
                    format!("the value for `{field}` is not a storage value"),
                )
            })?;
            out.insert(name, converted);
        }
        Ok(out)
    }
}

/// A string config field.
fn string_field(object: &serde_json::Map<String, Json>, key: &str) -> Option<String> {
    object.get(key).and_then(Json::as_str).map(str::to_owned)
}

/// Parses the `mapping` field: an object of expressions, or a path to literal fields.
fn parse_mapping(
    mapping: Option<&Json>,
) -> Result<(BTreeMap<Name, Mapping>, Option<String>), DocumentFailure> {
    match mapping {
        None | Some(Json::Null) => Ok((BTreeMap::new(), None)),
        Some(Json::String(path)) => Ok((BTreeMap::new(), Some(path.clone()))),
        Some(Json::Object(object)) => {
            let mut out = BTreeMap::new();
            for (field, value) in object {
                let name = Name::new(field)
                    .map_err(|_| config_error(format!("`{field}` is not a storage field name")))?;
                out.insert(name, parse_mapping_value(field, value)?);
            }
            Ok((out, None))
        }
        Some(_) => Err(config_error(
            "the mapping is neither an object nor a variable path",
        )),
    }
}

/// Parses one mapping value: an expression string or a typed object.
fn parse_mapping_value(field: &str, value: &Json) -> Result<Mapping, DocumentFailure> {
    match value {
        Json::String(expr) => Ok(Mapping::Expr(expr.clone())),
        Json::Object(object) => {
            let expr = object
                .get("expr")
                .and_then(Json::as_str)
                .ok_or_else(|| config_error(format!("the mapping for `{field}` needs an `expr`")))?
                .to_owned();
            let target = match object.get("as").and_then(Json::as_str) {
                Some(token) => Target::parse(token).ok_or_else(|| {
                    config_error(format!("`{token}` is not a mapping target type"))
                })?,
                None => Target::Text,
            };
            Ok(Mapping::Typed { expr, target })
        }
        _ => Err(config_error(format!(
            "the mapping for `{field}` is not an expression"
        ))),
    }
}

/// Evaluates one expression against the Case's variables.
fn evaluate(expr: &str, ctx: &NodeCtx<'_>) -> Result<ExprValue, DocumentFailure> {
    spot_bpm_engine::expr::evaluate(expr, ctx.variables, ctx.now).map_err(|detail| {
        DocumentFailure::new(
            codes::EXPRESSION_INVALID,
            format!("`{expr}` could not be evaluated: {detail}"),
        )
    })
}

/// Converts an expression value to the storage value it fits.
fn value_of(value: &ExprValue) -> Value {
    match value {
        ExprValue::Null => Value::Null,
        ExprValue::Bool(flag) => Value::Bool(*flag),
        ExprValue::Int(number) => Value::Int(*number),
        ExprValue::Decimal(decimal) => Value::Decimal(*decimal),
        ExprValue::Text(text) => Value::Text(text.clone()),
        ExprValue::Date(date) => Value::Date(time_date(*date)),
        ExprValue::DateTime(instant) => Value::DateTime(offset_datetime(*instant)),
        ExprValue::List(items) => Value::Json(json_array(items).to_string()),
    }
}

/// Converts an expression value to a declared target type, when it fits.
fn typed_value(value: &ExprValue, target: Target) -> Option<Value> {
    match target {
        Target::Text => match value {
            ExprValue::Text(text) => Some(Value::Text(text.clone())),
            _ => None,
        },
        Target::Int => match value {
            ExprValue::Int(number) => Some(Value::Int(*number)),
            _ => None,
        },
        Target::Decimal => match value {
            ExprValue::Decimal(decimal) => Some(Value::Decimal(*decimal)),
            ExprValue::Int(number) => Some(Value::Decimal(Decimal::from(*number))),
            _ => None,
        },
        Target::Bool => match value {
            ExprValue::Bool(flag) => Some(Value::Bool(*flag)),
            _ => None,
        },
        Target::Date => match value {
            ExprValue::Date(date) => Some(Value::Date(time_date(*date))),
            _ => None,
        },
        Target::DateTime => match value {
            ExprValue::DateTime(instant) => Some(Value::DateTime(offset_datetime(*instant))),
            _ => None,
        },
        Target::Link => match value {
            ExprValue::Text(text) => parse_record_id(text).map(Value::Id),
            _ => None,
        },
        Target::Json => Some(Value::Json(to_json_or_null(value).to_string())),
    }
}

/// A JSON array from expression values.
fn json_array(items: &[ExprValue]) -> Json {
    Json::Array(items.iter().map(to_json_or_null).collect())
}

/// An expression value as canonical JSON; an unrepresentable value is null.
fn to_json_or_null(value: &ExprValue) -> Json {
    spot_bpm_engine::expr::to_json(value).unwrap_or(Json::Null)
}

/// A `spot-expr` date as a storage date.
fn time_date(date: spot_expr::Date) -> Date {
    let (year, month, day) = date.ymd();
    let month = u8::try_from(month)
        .ok()
        .and_then(|raw| Month::try_from(raw).ok())
        .unwrap_or(Month::January);
    let day = u8::try_from(day).unwrap_or(1);
    Date::from_calendar_date(year, month, day).unwrap_or(Date::MIN)
}

/// A `spot-expr` instant as an `OffsetDateTime`.
fn offset_datetime(instant: spot_expr::DateTime) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(instant.micros()) * 1_000)
        .unwrap_or(OffsetDateTime::UNIX_EPOCH)
}

/// A JSON literal as a storage value.
fn json_to_value(value: &Json) -> Option<Value> {
    match value {
        Json::Null => Some(Value::Null),
        Json::Bool(flag) => Some(Value::Bool(*flag)),
        Json::Number(number) => number
            .as_i64()
            .or_else(|| number.as_u64().and_then(|raw| i64::try_from(raw).ok()))
            .map(Value::Int),
        Json::String(text) => Some(Value::Text(text.clone())),
        Json::Array(_) | Json::Object(_) => Some(Value::Json(value.to_string())),
    }
}

/// A malformed-config failure.
fn config_error(detail: impl Into<String>) -> DocumentFailure {
    DocumentFailure::new(codes::CONFIG_INVALID, detail)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn operations_parse_with_aliases() {
        assert_eq!(Operation::parse("create"), Some(Operation::CreateFrom));
        assert_eq!(Operation::parse("post"), Some(Operation::Submit));
        assert_eq!(Operation::parse("nope"), None);
    }

    #[test]
    fn a_config_parses_its_operation_and_mapping() {
        let config = json!({
            "operation": "create_from",
            "doctype": "sales_order",
            "mapping": { "customer": { "expr": "doc.party.id", "as": "link" }, "title": "doc.title" },
        });
        let parsed = NodeConfig::parse(&config, "order").unwrap();
        assert_eq!(parsed.operation, Operation::CreateFrom);
        assert_eq!(parsed.doctype.as_str(), "sales_order");
        assert_eq!(parsed.output, "order");
        assert_eq!(parsed.compensation, None);
    }

    #[test]
    fn submit_declares_cancel_as_its_compensation() {
        let config = json!({ "operation": "submit", "doctype": "sales_order" });
        let parsed = NodeConfig::parse(&config, "n1").unwrap();
        assert_eq!(parsed.compensation, Some(Operation::Cancel));
    }

    #[test]
    fn record_ids_parse_hyphenated_and_compact() {
        let id = crate::ids::record_id_for("x");
        assert_eq!(parse_record_id(&id.to_string()), Some(id));
        let compact = id.to_string().replace('-', "");
        assert_eq!(parse_record_id(&compact), Some(id));
        assert_eq!(parse_record_id("not-a-uuid"), None);
    }
}
