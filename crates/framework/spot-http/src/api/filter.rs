//! The contract's `Filter` JSON parsed into a typed AST and validated against `DocTypeMeta`
//! (ADR-055). Conversion to the storage query IR is **not** here — the service does it (the
//! AST maps one to one onto the IR, ADR-053).
//!
//! Limits (the brief): depth 16, 200 nodes, 1,000 `in` values. Violations are
//! `api.invalid_request`; an unknown field is `document.unknown_field`; an operator that does
//! not fit the field type is `api.invalid_request`; the value encoding is checked per field
//! type with the codec's rules.

use serde_json::Value as Json;
use spot_storage_contract::{CmpOp, Name, Nulls, OrderDir, TextMode};

use super::codec::{MetaLookup, parse_decimal_strict, parse_record_id};
use super::meta::{DocTypeMeta, FieldType};
use super::registry::Code;
use super::service::ApiError;

/// Maximum nesting depth of `and`/`or`/`not`.
pub const MAX_DEPTH: usize = 16;
/// Maximum node count in one filter tree.
pub const MAX_NODES: usize = 200;
/// Maximum values in one `in` operator.
pub const MAX_IN_VALUES: usize = 1_000;
/// Maximum order keys per query (contract `QueryRequest.order.maxItems`).
pub const MAX_ORDER_KEYS: usize = 8;

/// A comparison operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// `=`
    Eq,
    /// `<>`
    Ne,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
    /// `in`
    In,
    /// Literal prefix match.
    Prefix,
    /// Literal contains match.
    Contains,
    /// Literal suffix match.
    Suffix,
    /// The field is null.
    IsNull,
    /// The field is not null.
    IsNotNull,
}

impl Op {
    /// The contract's string form.
    pub fn as_str(self) -> &'static str {
        match self {
            Op::Eq => "eq",
            Op::Ne => "ne",
            Op::Lt => "lt",
            Op::Le => "le",
            Op::Gt => "gt",
            Op::Ge => "ge",
            Op::In => "in",
            Op::Prefix => "prefix",
            Op::Contains => "contains",
            Op::Suffix => "suffix",
            Op::IsNull => "is_null",
            Op::IsNotNull => "is_not_null",
        }
    }

    /// Parse the contract's string form.
    fn from_str(s: &str) -> Option<Op> {
        Some(match s {
            "eq" => Op::Eq,
            "ne" => Op::Ne,
            "lt" => Op::Lt,
            "le" => Op::Le,
            "gt" => Op::Gt,
            "ge" => Op::Ge,
            "in" => Op::In,
            "prefix" => Op::Prefix,
            "contains" => Op::Contains,
            "suffix" => Op::Suffix,
            "is_null" => Op::IsNull,
            "is_not_null" => Op::IsNotNull,
            _ => return None,
        })
    }

    /// The storage IR comparison, when this is one.
    pub fn cmp(self) -> Option<CmpOp> {
        match self {
            Op::Eq => Some(CmpOp::Eq),
            Op::Ne => Some(CmpOp::Ne),
            Op::Lt => Some(CmpOp::Lt),
            Op::Le => Some(CmpOp::Le),
            Op::Gt => Some(CmpOp::Gt),
            Op::Ge => Some(CmpOp::Ge),
            _ => None,
        }
    }

    /// The text-match mode, when this is one.
    pub fn text_mode(self) -> Option<TextMode> {
        match self {
            Op::Prefix => Some(TextMode::Prefix),
            Op::Contains => Some(TextMode::Contains),
            Op::Suffix => Some(TextMode::Suffix),
            _ => None,
        }
    }
}

/// One comparison: a field, an operator and the values, with the scalar(s) decoded per the
/// field's type.
#[derive(Clone, Debug, PartialEq)]
pub struct Comparison {
    /// The field's storage name.
    pub field: Name,
    /// The operator.
    pub op: Op,
    /// The decoded value(s). One for scalar ops; many for `in`; none for null tests.
    pub values: Vec<FilterValue>,
    /// Literal text matching may be case-insensitive.
    pub case_insensitive: bool,
}

/// A validated filter value. The scalar form is checked against the field type by the
/// validator; this keeps the raw JSON so services can map it onto the IR with full fidelity.
#[derive(Clone, Debug, PartialEq)]
pub enum FilterValue {
    /// A JSON string.
    Text(String),
    /// A JSON integer.
    Int(i64),
    /// A JSON boolean.
    Bool(bool),
}

impl FilterValue {
    /// The contract's JSON form.
    pub fn to_json(&self) -> Json {
        match self {
            FilterValue::Text(s) => Json::String(s.clone()),
            FilterValue::Int(i) => Json::from(*i),
            FilterValue::Bool(b) => Json::from(*b),
        }
    }
}

/// A node of the filter AST.
#[derive(Clone, Debug, PartialEq)]
pub enum FilterAst {
    /// Conjunction of at least one node.
    And(Vec<FilterAst>),
    /// Disjunction of at least one node.
    Or(Vec<FilterAst>),
    /// Negation of one node.
    Not(Box<FilterAst>),
    /// A field comparison.
    Cmp(Comparison),
}

/// An order key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrderKey {
    /// The field's storage name.
    pub field: Name,
    /// Direction.
    pub dir: OrderDir,
    /// Null placement; `None` uses the contract's default (last for asc, first for desc).
    pub nulls: Option<Nulls>,
}

/// The parsed contract `OrderKey` list (max 8; validated against the meta).
pub fn parse_order(value: &Json, meta: &DocTypeMeta) -> Result<Vec<OrderKey>, ApiError> {
    let Json::Array(items) = value else {
        return Err(invalid_request("`order` must be an array of order keys."));
    };
    if items.len() > MAX_ORDER_KEYS {
        return Err(invalid_request(format!(
            "`order` has {} keys; the maximum is {MAX_ORDER_KEYS}.",
            items.len()
        )));
    }
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let Json::Object(map) = item else {
            return Err(invalid_request("each order key must be an object."));
        };
        let field = match map.get("field").and_then(Json::as_str) {
            Some(f) => f,
            None => return Err(invalid_request("an order key needs a `field`.")),
        };
        let name = checked_field(field, meta)?;
        let dir = match map.get("dir").and_then(Json::as_str) {
            None | Some("asc") => OrderDir::Asc,
            Some("desc") => OrderDir::Desc,
            Some(other) => {
                return Err(invalid_request(format!(
                    "`dir` must be `asc` or `desc`, got `{other}`."
                )));
            }
        };
        let nulls = match map.get("nulls").and_then(Json::as_str) {
            None => None,
            Some("first") => Some(Nulls::First),
            Some("last") => Some(Nulls::Last),
            Some(other) => {
                return Err(invalid_request(format!(
                    "`nulls` must be `first` or `last`, got `{other}`."
                )));
            }
        };
        out.push(OrderKey {
            field: name,
            dir,
            nulls,
        });
    }
    Ok(out)
}

/// Parse and validate a `Filter` against `meta`. `in` values are counted per node.
pub fn parse_filter(
    value: &Json,
    meta: &DocTypeMeta,
    meta_of: MetaLookup,
) -> Result<FilterAst, ApiError> {
    let mut nodes = 0usize;
    let ast = parse_node(value, meta, meta_of, 1, &mut nodes)?;
    Ok(ast)
}

fn parse_node(
    value: &Json,
    meta: &DocTypeMeta,
    meta_of: MetaLookup,
    depth: usize,
    nodes: &mut usize,
) -> Result<FilterAst, ApiError> {
    if depth > MAX_DEPTH {
        return Err(invalid_request(format!(
            "the filter tree is deeper than {MAX_DEPTH} levels."
        )));
    }
    *nodes += 1;
    if *nodes > MAX_NODES {
        return Err(invalid_request(format!(
            "the filter tree has more than {MAX_NODES} nodes."
        )));
    }
    let Json::Object(map) = value else {
        return Err(invalid_request("a filter node must be a JSON object."));
    };
    // Exactly one of: and, or, not, or a comparison (field/op/...).
    let keys: Vec<&String> = map.keys().collect();
    if let Some(items) = map.get("and") {
        reject_extra_keys(&keys, &["and"])?;
        let Json::Array(list) = items else {
            return Err(invalid_request("`and` must be an array."));
        };
        if list.is_empty() {
            return Err(invalid_request("`and` must have at least one item."));
        }
        let mut out = Vec::with_capacity(list.len());
        for item in list {
            out.push(parse_node(item, meta, meta_of, depth + 1, nodes)?);
        }
        return Ok(FilterAst::And(out));
    }
    if let Some(items) = map.get("or") {
        reject_extra_keys(&keys, &["or"])?;
        let Json::Array(list) = items else {
            return Err(invalid_request("`or` must be an array."));
        };
        if list.is_empty() {
            return Err(invalid_request("`or` must have at least one item."));
        }
        let mut out = Vec::with_capacity(list.len());
        for item in list {
            out.push(parse_node(item, meta, meta_of, depth + 1, nodes)?);
        }
        return Ok(FilterAst::Or(out));
    }
    if let Some(inner) = map.get("not") {
        reject_extra_keys(&keys, &["not"])?;
        let node = parse_node(inner, meta, meta_of, depth + 1, nodes)?;
        return Ok(FilterAst::Not(Box::new(node)));
    }
    // A comparison node.
    let field = map
        .get("field")
        .and_then(Json::as_str)
        .ok_or_else(|| invalid_request("a filter comparison needs `field` and `op`."))?;
    let name = checked_field(field, meta)?;
    let op = map
        .get("op")
        .and_then(Json::as_str)
        .ok_or_else(|| invalid_request("a filter comparison needs `field` and `op`."))?;
    let op = Op::from_str(op)
        .ok_or_else(|| invalid_request(format!("unknown filter operator `{op}`.")))?;
    let mut allowed_keys = vec!["field", "op"];
    if matches!(op, Op::IsNull | Op::IsNotNull) {
        // no value
    } else {
        allowed_keys.push("value");
    }
    if matches!(op, Op::Prefix | Op::Contains | Op::Suffix) {
        allowed_keys.push("case_insensitive");
    }
    reject_extra_keys(&keys, &allowed_keys)?;

    let meta_field = meta.field(field);
    match op {
        Op::IsNull | Op::IsNotNull => Ok(FilterAst::Cmp(Comparison {
            field: name,
            op,
            values: Vec::new(),
            case_insensitive: false,
        })),
        Op::In => {
            let Json::Array(items) = map
                .get("value")
                .ok_or_else(|| invalid_request("the `in` operator needs a `value` array."))?
            else {
                return Err(invalid_request("the `in` operator needs a `value` array."));
            };
            if items.is_empty() {
                return Err(invalid_request("`in` needs at least one value."));
            }
            if items.len() > MAX_IN_VALUES {
                return Err(invalid_request(format!(
                    "`in` has {} values; the maximum is {MAX_IN_VALUES}.",
                    items.len()
                )));
            }
            if !op_fits_type(op, meta_field.map(|f| &f.field_type)) {
                return Err(op_does_not_fit(field, op));
            }
            let mut values = Vec::with_capacity(items.len());
            for item in items {
                values.push(scalar_filter_value(item, meta_field, field, meta_of)?);
            }
            Ok(FilterAst::Cmp(Comparison {
                field: name,
                op,
                values,
                case_insensitive: false,
            }))
        }
        Op::Prefix | Op::Contains | Op::Suffix => {
            if !op_fits_type(op, meta_field.map(|f| &f.field_type)) {
                return Err(op_does_not_fit(field, op));
            }
            let text = map
                .get("value")
                .and_then(Json::as_str)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    invalid_request("text operators need a non-empty string `value`.")
                })?;
            let case_insensitive = match map.get("case_insensitive") {
                None => false,
                Some(Json::Bool(b)) => *b,
                Some(_) => {
                    return Err(invalid_request("`case_insensitive` must be a boolean."));
                }
            };
            Ok(FilterAst::Cmp(Comparison {
                field: name,
                op,
                values: vec![FilterValue::Text(text.to_owned())],
                case_insensitive,
            }))
        }
        _ => {
            // comparison ops: value must be a JSON string, integer or boolean (never null,
            // never object/array; the contract's Filter comparison schema).
            if !op_fits_type(op, meta_field.map(|f| &f.field_type)) {
                return Err(op_does_not_fit(field, op));
            }
            let value = map
                .get("value")
                .ok_or_else(|| invalid_request("a filter comparison needs a `value`."))?;
            let v = scalar_filter_value(value, meta_field, field, meta_of)?;
            Ok(FilterAst::Cmp(Comparison {
                field: name,
                op,
                values: vec![v],
                case_insensitive: false,
            }))
        }
    }
}

fn checked_field(field: &str, meta: &DocTypeMeta) -> Result<Name, ApiError> {
    if meta.field(field).is_some() {
        Name::new(field).map_err(|_| invalid_request(format!("`{field}` is not a field name.")))
    } else {
        let mut err = ApiError::new(
            Code::DocumentUnknownField,
            format!("The DocType `{}` has no such field.", meta.doctype.as_str()),
        );
        err.fields.push(super::service::FieldIssue::new(
            field.to_owned(),
            "document.unknown_field",
            "The DocType has no such field.",
        ));
        Err(err)
    }
}

fn reject_extra_keys(keys: &[&String], allowed: &[&str]) -> Result<(), ApiError> {
    for key in keys {
        if !allowed.contains(&key.as_str()) {
            return Err(invalid_request(format!(
                "the filter node key `{key}` is not allowed here."
            )));
        }
    }
    Ok(())
}

fn op_does_not_fit(field: &str, op: Op) -> ApiError {
    invalid_request(format!(
        "the operator `{}` does not fit the type of field `{field}`.",
        op.as_str()
    ))
}

/// Which operators fit which field types. Comparisons fit every non-table scalar; text
/// matching only `text`/`link`; decimals are compared as decimal strings but the JSON form is
/// a string per the Values table.
fn op_fits_type(op: Op, ty: Option<&FieldType>) -> bool {
    let Some(ty) = ty else {
        return false;
    };
    match op {
        Op::IsNull | Op::IsNotNull => true,
        Op::Prefix | Op::Contains | Op::Suffix => matches!(ty, FieldType::Text),
        Op::Eq | Op::Ne | Op::Lt | Op::Le | Op::Gt | Op::Ge | Op::In => {
            !matches!(ty, FieldType::Table | FieldType::Json)
        }
    }
}

/// Decode one filter value per the field's type (the codec's value rules; `jsonschema`-level
/// string/integer/boolean already checked by the contract).
fn scalar_filter_value(
    value: &Json,
    meta_field: Option<&super::meta::MetaField>,
    field: &str,
    _meta_of: MetaLookup,
) -> Result<FilterValue, ApiError> {
    let ty = meta_field.map(|f| &f.field_type);
    match value {
        Json::String(s) => match ty {
            Some(FieldType::Decimal { .. }) => {
                if parse_decimal_strict(s).is_some() {
                    Ok(FilterValue::Text(s.clone()))
                } else {
                    Err(invalid_request(format!(
                        "the decimal value for `{field}` must be a decimal string."
                    )))
                }
            }
            Some(FieldType::Link) => {
                if parse_record_id(s).is_some() {
                    Ok(FilterValue::Text(s.clone()))
                } else {
                    Err(invalid_request(format!(
                        "the link value for `{field}` must be a canonical UUIDv7."
                    )))
                }
            }
            Some(FieldType::Date) => {
                if super::codec::parse_date(s).is_some() {
                    Ok(FilterValue::Text(s.clone()))
                } else {
                    Err(invalid_request(format!(
                        "the date value for `{field}` must be `YYYY-MM-DD`."
                    )))
                }
            }
            Some(FieldType::DateTime) => {
                if super::codec::parse_datetime(s).is_some() {
                    Ok(FilterValue::Text(s.clone()))
                } else {
                    Err(invalid_request(format!(
                        "the datetime value for `{field}` must be RFC 3339."
                    )))
                }
            }
            Some(FieldType::Time) => {
                if super::codec::parse_time(s).is_some() {
                    Ok(FilterValue::Text(s.clone()))
                } else {
                    Err(invalid_request(format!(
                        "the time value for `{field}` must be `HH:MM:SS`."
                    )))
                }
            }
            Some(FieldType::Bytes) => Err(invalid_request(
                "bytes fields cannot be filtered by value in v0.1.",
            )),
            // An Int or Bool field's value is an integer or a boolean (the Values table);
            // a string never stands in for it, or a type error would surface in storage
            // instead of at the API edge.
            Some(FieldType::Int | FieldType::Bool) => Err(invalid_request(format!(
                "the value for the `{ty}` field `{field}` must be its JSON type, not a string.",
                ty = if matches!(meta_field.map(|f| &f.field_type), Some(FieldType::Int)) {
                    "int"
                } else {
                    "bool"
                }
            ))),
            _ => Ok(FilterValue::Text(s.clone())),
        },
        Json::Number(n) => {
            if ty.is_some_and(|t| matches!(t, FieldType::Decimal { .. })) {
                return Err(invalid_request(format!(
                    "a decimal filter value must be a string, not a number (field `{field}`)."
                )));
            }
            if let Some(i) = n.as_i64() {
                if ty.is_some_and(|t| {
                    matches!(
                        t,
                        FieldType::Text
                            | FieldType::Date
                            | FieldType::Time
                            | FieldType::DateTime
                            | FieldType::Link
                    )
                }) {
                    return Err(invalid_request(format!(
                        "the value for `{field}` must be a string of its type."
                    )));
                }
                Ok(FilterValue::Int(i))
            } else {
                Err(invalid_request(format!(
                    "`{field}` needs a 64-bit integer or a decimal string."
                )))
            }
        }
        Json::Bool(b) => {
            if ty.is_some_and(|t| !matches!(t, FieldType::Bool)) {
                return Err(invalid_request(format!(
                    "a boolean value fits only a bool field (`{field}`)."
                )));
            }
            Ok(FilterValue::Bool(*b))
        }
        Json::Null => Err(invalid_request(
            "a filter value is never null; use `is_null`.",
        )),
        _ => Err(invalid_request(format!(
            "the filter value for `{field}` must be a string, integer or boolean."
        ))),
    }
}

fn invalid_request(message: impl Into<String>) -> ApiError {
    ApiError::new(Code::ApiInvalidRequest, message)
}

#[cfg(test)]
mod tests {
    use super::super::meta::MetaField;
    use super::*;
    use crate::Must;

    fn name(s: &str) -> Name {
        Name::new(s).must()
    }

    fn field(n: &str, t: FieldType) -> MetaField {
        MetaField {
            name: name(n),
            label: n.to_owned(),
            field_type: t,
            required: false,
            read_only: false,
            scale: None,
            precision: None,
            link_to: None,
            link_type_field: None,
            child_doctype: None,
            link_field: None,
            select: None,
            max_chars: None,
            default: None,
            format: None,
            format_params: std::collections::BTreeMap::new(),
            lookup: None,
            hints: super::super::meta::FieldHints::default(),
            has_hints: false,
            multiple: false,
        }
    }

    fn meta() -> DocTypeMeta {
        DocTypeMeta {
            doctype: name("sales_invoice"),
            label: "Sales Invoice".to_owned(),
            version: 1,
            is_posted: true,
            fields: vec![
                field("posting_date", FieldType::Date),
                field("grand_total", FieldType::Decimal { scale: 2 }),
                field("customer", FieldType::Link),
                field("remarks", FieldType::Text),
                field("is_paid", FieldType::Bool),
                field("count", FieldType::Int),
                field("items", FieldType::Table),
            ],
        }
    }

    fn ok(json: &str) -> FilterAst {
        parse_filter(
            &serde_json::from_str(json).must(),
            &meta(),
            MetaLookup::none(),
        )
        .must()
    }

    fn err(json: &str) -> ApiError {
        parse_filter(
            &serde_json::from_str(json).must(),
            &meta(),
            MetaLookup::none(),
        )
        .must_err_owned()
    }

    #[test]
    fn parses_the_contract_example() {
        let ast = ok(r#"{
            "and": [
                {"field": "posting_date", "op": "ge", "value": "2026-09-01"},
                {"field": "grand_total", "op": "gt", "value": "1000.00"},
                {"not": {"field": "customer", "op": "in", "value": ["0192f3c4-0000-7000-8000-00000000c009"]}}
            ]
        }"#);
        let FilterAst::And(items) = ast else {
            panic!("expected and");
        };
        assert_eq!(items.len(), 3);
    }

    #[test]
    fn unknown_field_is_unknown_field_error() {
        let e = err(r#"{"field": "ghost", "op": "eq", "value": "x"}"#);
        assert_eq!(e.code.as_str(), "document.unknown_field");
    }

    #[test]
    fn prefix_on_decimal_rejected() {
        let e = err(r#"{"field": "grand_total", "op": "prefix", "value": "1"}"#);
        assert_eq!(e.code.as_str(), "api.invalid_request");
    }

    #[test]
    fn text_op_on_link_and_bool_rejected() {
        assert_eq!(
            err(r#"{"field": "customer", "op": "contains", "value": "a"}"#)
                .code
                .as_str(),
            "api.invalid_request"
        );
        assert_eq!(
            err(r#"{"field": "is_paid", "op": "prefix", "value": "t"}"#)
                .code
                .as_str(),
            "api.invalid_request"
        );
    }

    #[test]
    fn table_and_json_not_comparable() {
        assert_eq!(
            err(r#"{"field": "items", "op": "eq", "value": "x"}"#)
                .code
                .as_str(),
            "api.invalid_request"
        );
    }

    #[test]
    fn json_number_for_decimal_value_rejected() {
        let e = err(r#"{"field": "grand_total", "op": "gt", "value": 1000}"#);
        assert_eq!(e.code.as_str(), "api.invalid_request");
    }

    #[test]
    fn null_filter_value_rejected() {
        assert_eq!(
            err(r#"{"field": "remarks", "op": "eq", "value": null}"#)
                .code
                .as_str(),
            "api.invalid_request"
        );
    }

    #[test]
    fn bad_encoding_per_field_type() {
        assert!(
            err(r#"{"field": "posting_date", "op": "eq", "value": "2026-13-01"}"#)
                .code
                .as_str()
                == "api.invalid_request"
        );
        assert!(
            err(r#"{"field": "customer", "op": "eq", "value": "not-a-uuid"}"#)
                .code
                .as_str()
                == "api.invalid_request"
        );
        assert!(
            err(r#"{"field": "grand_total", "op": "eq", "value": "1e3"}"#)
                .code
                .as_str()
                == "api.invalid_request"
        );
    }

    #[test]
    fn depth_limit() {
        // 17 nested not()s exceed the 16-level maximum.
        let mut json = String::from(r#"{"field": "remarks", "op": "is_not_null"}"#);
        for _ in 0..MAX_DEPTH {
            json = format!(r#"{{"not": {json}}}"#);
        }
        assert_eq!(err(&json).code.as_str(), "api.invalid_request");
        // one fewer is fine
        let mut json = String::from(r#"{"field": "remarks", "op": "is_not_null"}"#);
        for _ in 0..MAX_DEPTH - 2 {
            json = format!(r#"{{"not": {json}}}"#);
        }
        ok(&json);
    }

    #[test]
    fn node_limit() {
        let items: Vec<String> = (0..(MAX_NODES + 1))
            .map(|_| r#"{"field": "remarks", "op": "is_not_null"}"#.to_owned())
            .collect();
        let json = format!(r#"{{"and": [{}]}}"#, items.join(","));
        assert_eq!(err(&json).code.as_str(), "api.invalid_request");
    }

    #[test]
    fn in_values_limit() {
        let vals: Vec<String> = (0..(MAX_IN_VALUES + 1))
            .map(|i| format!("\"v{i}\""))
            .collect();
        let json = format!(
            r#"{{"field": "remarks", "op": "in", "value": [{}]}}"#,
            vals.join(",")
        );
        assert_eq!(err(&json).code.as_str(), "api.invalid_request");
    }

    #[test]
    fn empty_and_or_in_rejected() {
        assert_eq!(err(r#"{"and": []}"#).code.as_str(), "api.invalid_request");
        assert_eq!(err(r#"{"or": []}"#).code.as_str(), "api.invalid_request");
        assert_eq!(
            err(r#"{"field": "count", "op": "in", "value": []}"#)
                .code
                .as_str(),
            "api.invalid_request"
        );
    }

    #[test]
    fn extra_keys_rejected() {
        assert_eq!(
            err(r#"{"field": "count", "op": "eq", "value": 1, "junk": 2}"#)
                .code
                .as_str(),
            "api.invalid_request"
        );
        assert_eq!(
            err(r#"{"and": [{"field": "count", "op": "is_not_null"}], "or": []}"#)
                .code
                .as_str(),
            "api.invalid_request"
        );
    }

    #[test]
    fn bool_value_only_for_bool_field() {
        ok(r#"{"field": "is_paid", "op": "eq", "value": true}"#);
        assert_eq!(
            err(r#"{"field": "count", "op": "eq", "value": true}"#)
                .code
                .as_str(),
            "api.invalid_request"
        );
    }

    #[test]
    fn case_insensitive_flag() {
        let ast = ok(
            r#"{"field": "remarks", "op": "contains", "value": "late", "case_insensitive": true}"#,
        );
        let FilterAst::Cmp(c) = ast else {
            panic!("expected cmp");
        };
        assert!(c.case_insensitive);
        assert_eq!(
            err(
                r#"{"field": "remarks", "op": "contains", "value": "x", "case_insensitive": "yes"}"#
            )
            .code
            .as_str(),
            "api.invalid_request"
        );
    }

    #[test]
    fn order_parsing() {
        let keys = parse_order(
            &serde_json::json!([{"field": "posting_date", "dir": "desc", "nulls": "last"}]),
            &meta(),
        )
        .must();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].dir, OrderDir::Desc);
        assert_eq!(keys[0].nulls, Some(Nulls::Last));
        assert!(
            parse_order(&serde_json::json!([{"field": "ghost"}]), &meta())
                .must_err_owned()
                .code
                .as_str()
                == "document.unknown_field"
        );
        let too_many: Vec<Json> = (0..(MAX_ORDER_KEYS + 1))
            .map(|_| serde_json::json!({"field": "count"}))
            .collect();
        assert!(parse_order(&Json::Array(too_many), &meta()).is_err());
    }

    // helper on Result for owned error unwrap
    trait MustErrOwned<T, E> {
        fn must_err_owned(self) -> E;
    }
    impl<T: std::fmt::Debug, E> MustErrOwned<T, E> for Result<T, E> {
        fn must_err_owned(self) -> E {
            match self {
                Ok(v) => panic!("unexpected Ok: {v:?}"),
                Err(e) => e,
            }
        }
    }
}
