//! Report definitions: the TOML an app ships (or a site authors at runtime).
//!
//! A definition is data, never code and never SQL: columns, typed parameters, a filter tree,
//! grouping, ordering and a row limit over one base DocType and its link-joins. Parsing is
//! strict (`deny_unknown_fields`); [`ReportDef::validate`] reports **every** semantic problem
//! at once with `report.*` codes, so an author fixes a report in one pass. Field resolution
//! against the effective metadata happens in [`crate::Compiler`], not here.

use std::path::Path;

use serde::Deserialize;
use spot_storage_contract::{Name, Value};
use time::Date;

use crate::error::{ReportError, codes};

/// The largest `limit` the contract's request allows, and the ceiling on a report's maximum.
pub const ABSOLUTE_MAX_ROWS: u32 = 100_000;
/// The row limit when a definition declares none.
pub const DEFAULT_LIMIT: u32 = 1_000;
/// The maximum when a definition declares none.
pub const DEFAULT_MAX_ROWS: u32 = 10_000;

/// One report definition, as TOML.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ReportDef {
    /// The report's storage name (lowercase letters, digits and underscores).
    pub name: String,
    /// Human label.
    pub label: String,
    /// The base DocType.
    pub base: String,
    /// The link-joins, each aliased.
    #[serde(default)]
    pub joins: Vec<JoinDef>,
    /// Output columns over the base or a joined alias.
    pub columns: Vec<ColumnDef>,
    /// Computed columns, evaluated after the query in the shared expression language.
    #[serde(default)]
    pub computed: Vec<ComputedDef>,
    /// The typed parameters a run may supply.
    #[serde(default)]
    pub parameters: Vec<ParameterDef>,
    /// The filter tree, over fields and parameters.
    #[serde(default)]
    pub filters: Option<FilterNode>,
    /// Grouping keys; required when any column aggregates.
    #[serde(default)]
    pub group_by: Vec<String>,
    /// Sort keys.
    #[serde(default)]
    pub order_by: Vec<OrderDef>,
    /// The default row limit (when the request sends none).
    #[serde(default)]
    pub limit: Option<u32>,
    /// The largest row limit a run honours.
    #[serde(default)]
    pub max_rows: Option<u32>,
    /// The roles a caller must hold to see the report exists (the data stays filtered).
    #[serde(default)]
    pub roles: Vec<String>,
}

/// A join to a linked DocType.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct JoinDef {
    /// The alias used by column and filter field paths.
    pub alias: String,
    /// The joined DocType.
    pub doctype: String,
    /// The link field the join follows: `field` on the base, or `alias.field` on an earlier
    /// joined source.
    pub on: String,
}

/// One output column.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ColumnDef {
    /// A field path: `field` (base) or `alias.field`.
    pub field: String,
    /// Human label; the field name when absent.
    #[serde(default)]
    pub label: Option<String>,
    /// An aggregate: `sum`, `count`, `min`, `max` or `avg`.
    #[serde(default)]
    pub aggregate: Option<String>,
    /// A format hint for the client (`money`, `date`, ...); it changes nothing stored.
    #[serde(default)]
    pub format: Option<String>,
}

/// One computed column, evaluated over the row after the query.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ComputedDef {
    /// The output column name.
    pub name: String,
    /// Human label; the name when absent.
    #[serde(default)]
    pub label: Option<String>,
    /// The result's contract type (`int`, `decimal`, `text`, ...).
    #[serde(rename = "type", default)]
    pub ty: Option<String>,
    /// The expression, in the shared expression language (`doc.qty * doc.price`).
    pub expr: String,
    /// A format hint for the client.
    #[serde(default)]
    pub format: Option<String>,
}

/// One typed parameter.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ParameterDef {
    /// The parameter name a request's `filters` keys by.
    pub name: String,
    /// Human label.
    pub label: String,
    /// The parameter type: `text`, `int`, `decimal`, `date`, `link` or `select`.
    #[serde(rename = "type")]
    pub ty: String,
    /// Must be supplied; a required parameter cannot carry a default.
    #[serde(default)]
    pub required: bool,
    /// The value used when the caller sends none.
    #[serde(default)]
    pub default: Option<toml::Value>,
    /// The allowed values of a `select` parameter.
    #[serde(default)]
    pub options: Option<Vec<String>>,
    /// The DocType a `link` parameter points at.
    #[serde(default)]
    pub doctype: Option<String>,
    /// The name filters bind by; the parameter name when absent.
    #[serde(default)]
    pub for_filter: Option<String>,
}

/// One sort key.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct OrderDef {
    /// A column name or a field path.
    pub field: String,
    /// `asc` (default) or `desc`.
    #[serde(default)]
    pub dir: Option<String>,
    /// `first` or `last`; `first` for ascending, `last` for descending when absent.
    #[serde(default)]
    pub nulls: Option<String>,
}

/// A node of the filter tree: an AND, an OR, a negation, or one comparison.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(untagged)]
pub enum FilterNode {
    /// Every child must hold.
    All(FilterAll),
    /// At least one child must hold.
    Any(FilterAny),
    /// The child must not hold.
    Not(FilterNot),
    /// One comparison.
    Cmp(FilterCmp),
}

/// An AND node.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FilterAll {
    /// The children.
    pub all: Vec<FilterNode>,
}

/// An OR node.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FilterAny {
    /// The children.
    pub any: Vec<FilterNode>,
}

/// A negation.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FilterNot {
    /// The negated child.
    pub not: Box<FilterNode>,
}

/// One comparison against a parameter or a literal.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FilterCmp {
    /// The field path.
    pub field: String,
    /// `eq`, `ne`, `lt`, `le`, `gt` or `ge`.
    pub op: String,
    /// The parameter this comparison binds to.
    #[serde(default)]
    pub param: Option<String>,
    /// A literal value (mutually exclusive with `param`).
    #[serde(default)]
    pub value: Option<toml::Value>,
}

/// A parsed parameter type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParamType {
    /// Text.
    Text,
    /// A 64-bit integer.
    Int,
    /// An exact decimal.
    Decimal,
    /// A calendar date.
    Date,
    /// A record id.
    Link,
    /// One of a listed set of texts.
    Select,
}

impl ParamType {
    /// The name as written in TOML.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ParamType::Text => "text",
            ParamType::Int => "int",
            ParamType::Decimal => "decimal",
            ParamType::Date => "date",
            ParamType::Link => "link",
            ParamType::Select => "select",
        }
    }

    /// Parses the written name.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "text" => ParamType::Text,
            "int" => ParamType::Int,
            "decimal" => ParamType::Decimal,
            "date" => ParamType::Date,
            "link" => ParamType::Link,
            "select" => ParamType::Select,
            _ => return None,
        })
    }
}

/// The aggregate a column carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AggKind {
    /// Sum of numbers.
    Sum,
    /// Number of rows or non-null values.
    Count,
    /// Smallest value.
    Min,
    /// Largest value.
    Max,
    /// An exact average, returned as a `[sum, count]` decimal pair.
    Avg,
}

impl AggKind {
    /// The name as written in TOML.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            AggKind::Sum => "sum",
            AggKind::Count => "count",
            AggKind::Min => "min",
            AggKind::Max => "max",
            AggKind::Avg => "avg",
        }
    }

    /// Parses the written name.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "sum" => AggKind::Sum,
            "count" => AggKind::Count,
            "min" => AggKind::Min,
            "max" => AggKind::Max,
            "avg" => AggKind::Avg,
            _ => return None,
        })
    }
}

/// The comparison a filter carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompareOp {
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
}

impl CompareOp {
    /// The name as written in TOML.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            CompareOp::Eq => "eq",
            CompareOp::Ne => "ne",
            CompareOp::Lt => "lt",
            CompareOp::Le => "le",
            CompareOp::Gt => "gt",
            CompareOp::Ge => "ge",
        }
    }

    /// Parses the written name.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "eq" => CompareOp::Eq,
            "ne" => CompareOp::Ne,
            "lt" => CompareOp::Lt,
            "le" => CompareOp::Le,
            "gt" => CompareOp::Gt,
            "ge" => CompareOp::Ge,
            _ => return None,
        })
    }

    /// The storage operator.
    #[must_use]
    pub fn to_cmp(self) -> spot_storage_contract::CmpOp {
        use spot_storage_contract::CmpOp;
        match self {
            CompareOp::Eq => CmpOp::Eq,
            CompareOp::Ne => CmpOp::Ne,
            CompareOp::Lt => CmpOp::Lt,
            CompareOp::Le => CmpOp::Le,
            CompareOp::Gt => CmpOp::Gt,
            CompareOp::Ge => CmpOp::Ge,
        }
    }
}

impl ReportDef {
    /// Parses a definition from TOML text.
    ///
    /// # Errors
    ///
    /// One [`codes::INVALID_DEFINITION`] error carrying the TOML parser's message.
    pub fn from_toml(text: &str) -> Result<Self, ReportError> {
        toml::from_str(text).map_err(|e| ReportError::new(codes::INVALID_DEFINITION, e.to_string()))
    }

    /// Loads a definition from a `.toml` file.
    ///
    /// # Errors
    ///
    /// The read error or the parse error, as [`codes::INVALID_DEFINITION`].
    pub fn load_file(path: impl AsRef<Path>) -> Result<Self, ReportError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| ReportError::new(codes::INVALID_DEFINITION, e.to_string()))?;
        Self::from_toml(&text)
    }

    /// The default row limit.
    #[must_use]
    pub fn default_limit(&self) -> u32 {
        self.limit.unwrap_or(DEFAULT_LIMIT)
    }

    /// The largest row limit a run honours.
    #[must_use]
    pub fn max_rows(&self) -> u32 {
        self.max_rows.unwrap_or(DEFAULT_MAX_ROWS)
    }

    /// Checks the definition's structure and semantics, reporting every problem at once.
    /// Nothing here needs the metadata: field resolution is the compiler's job.
    #[must_use]
    pub fn validate(&self) -> Vec<ReportError> {
        let mut errors = Vec::new();
        check_name(&mut errors, &self.name, "name");
        check_name(&mut errors, &self.base, "base");
        if self.label.trim().is_empty() {
            errors.push(ReportError::at(
                codes::INVALID_DEFINITION,
                "label",
                "a report needs a label",
            ));
        }
        if self.columns.is_empty() {
            errors.push(ReportError::new(
                codes::INVALID_DEFINITION,
                "a report needs at least one column",
            ));
        }
        let default = self.default_limit();
        let max = self.max_rows();
        if !(1..=ABSOLUTE_MAX_ROWS).contains(&default) {
            errors.push(ReportError::at(
                codes::INVALID_DEFINITION,
                "limit",
                format!("the default limit must be between 1 and {ABSOLUTE_MAX_ROWS}"),
            ));
        }
        if !(1..=ABSOLUTE_MAX_ROWS).contains(&max) {
            errors.push(ReportError::at(
                codes::INVALID_DEFINITION,
                "max_rows",
                format!("the maximum must be between 1 and {ABSOLUTE_MAX_ROWS}"),
            ));
        }
        if max < default {
            errors.push(ReportError::at(
                codes::INVALID_DEFINITION,
                "max_rows",
                "the maximum must not be below the default limit",
            ));
        }

        let mut aliases = vec![BASE_ALIAS.to_owned()];
        for (i, join) in self.joins.iter().enumerate() {
            check_name(&mut errors, &join.alias, &format!("joins[{i}].alias"));
            check_name(&mut errors, &join.doctype, &format!("joins[{i}].doctype"));
            if aliases.contains(&join.alias) {
                errors.push(ReportError::at(
                    codes::DUPLICATE,
                    format!("joins[{i}].alias"),
                    format!("the alias `{}` is already used", join.alias),
                ));
            }
            aliases.push(join.alias.clone());
            if join.on.trim().is_empty() {
                errors.push(ReportError::at(
                    codes::INVALID_DEFINITION,
                    format!("joins[{i}].on"),
                    "a join needs the link field it follows",
                ));
            }
        }

        let mut names: Vec<String> = Vec::new();
        for (i, column) in self.columns.iter().enumerate() {
            let at = format!("columns[{i}]");
            if column.field.trim().is_empty() {
                errors.push(ReportError::at(
                    codes::INVALID_DEFINITION,
                    &at,
                    "a column needs a field",
                ));
            }
            if let Some(agg) = &column.aggregate
                && AggKind::parse(agg).is_none()
            {
                errors.push(ReportError::at(
                    codes::BAD_TYPE,
                    &at,
                    format!("`{agg}` is not an aggregate (sum, count, min, max, avg)"),
                ));
            }
            let name = default_column_name(&column.field);
            if names.contains(&name) {
                errors.push(ReportError::at(
                    codes::DUPLICATE,
                    &at,
                    format!("the column name `{name}` is already used"),
                ));
            }
            names.push(name);
        }
        for (i, computed) in self.computed.iter().enumerate() {
            let at = format!("computed[{i}]");
            check_name(&mut errors, &computed.name, &format!("{at}.name"));
            if computed.expr.trim().is_empty() {
                errors.push(ReportError::at(
                    codes::INVALID_DEFINITION,
                    &at,
                    "a computed column needs an expression",
                ));
            }
            if names.contains(&computed.name) {
                errors.push(ReportError::at(
                    codes::DUPLICATE,
                    &at,
                    format!("the column name `{}` is already used", computed.name),
                ));
            }
            names.push(computed.name.clone());
            if let Some(ty) = &computed.ty
                && crate::encode::FieldType::parse(ty).is_none()
            {
                errors.push(ReportError::at(
                    codes::BAD_TYPE,
                    &at,
                    format!("`{ty}` is not a field type"),
                ));
            }
        }

        let mut params: Vec<String> = Vec::new();
        let mut bindings: Vec<String> = Vec::new();
        for (i, p) in self.parameters.iter().enumerate() {
            let at = format!("parameters[{i}]");
            check_name(&mut errors, &p.name, &format!("{at}.name"));
            if params.contains(&p.name) {
                errors.push(ReportError::at(
                    codes::DUPLICATE,
                    &at,
                    format!("the parameter `{}` is already declared", p.name),
                ));
            }
            params.push(p.name.clone());
            let Some(ty) = ParamType::parse(&p.ty) else {
                errors.push(ReportError::at(
                    codes::BAD_TYPE,
                    &at,
                    format!(
                        "`{}` is not a parameter type (text, int, decimal, date, link, select)",
                        p.ty
                    ),
                ));
                continue;
            };
            let binding = p.for_filter.clone().unwrap_or_else(|| p.name.clone());
            if bindings.contains(&binding) {
                errors.push(ReportError::at(
                    codes::DUPLICATE,
                    &at,
                    format!("the filter binding `{binding}` is already used"),
                ));
            }
            bindings.push(binding);
            if ty == ParamType::Select && p.options.as_ref().is_none_or(Vec::is_empty) {
                errors.push(ReportError::at(
                    codes::INVALID_DEFINITION,
                    &at,
                    "a select parameter needs its options",
                ));
            }
            if ty == ParamType::Link {
                if let Some(doctype) = &p.doctype {
                    check_name(&mut errors, doctype, &format!("{at}.doctype"));
                } else {
                    errors.push(ReportError::at(
                        codes::INVALID_DEFINITION,
                        &at,
                        "a link parameter needs its target DocType",
                    ));
                }
            }
            if p.required && p.default.is_some() {
                errors.push(ReportError::at(
                    codes::INVALID_DEFINITION,
                    &at,
                    "a required parameter cannot also carry a default",
                ));
            }
            if let Some(default) = &p.default {
                if let Err(message) = literal_to_value(default) {
                    errors.push(ReportError::at(codes::BAD_TYPE, &at, message));
                } else if !default_fits(ty, default) {
                    errors.push(ReportError::at(
                        codes::BAD_TYPE,
                        &at,
                        format!("the default does not fit a `{}` parameter", ty.as_str()),
                    ));
                }
            }
        }

        if let Some(filter) = &self.filters {
            validate_filter(&mut errors, filter, "filters");
        }
        for (i, g) in self.group_by.iter().enumerate() {
            if g.trim().is_empty() {
                errors.push(ReportError::at(
                    codes::INVALID_DEFINITION,
                    format!("group_by[{i}]"),
                    "a grouping key needs a field",
                ));
            }
        }
        for (i, o) in self.order_by.iter().enumerate() {
            let at = format!("order_by[{i}]");
            if o.field.trim().is_empty() {
                errors.push(ReportError::at(
                    codes::INVALID_DEFINITION,
                    &at,
                    "a sort key needs a field",
                ));
            }
            if let Some(dir) = &o.dir
                && !matches!(dir.as_str(), "asc" | "desc")
            {
                errors.push(ReportError::at(
                    codes::INVALID_DEFINITION,
                    &at,
                    format!("`{dir}` is not a sort direction (asc, desc)"),
                ));
            }
            if let Some(nulls) = &o.nulls
                && !matches!(nulls.as_str(), "first" | "last")
            {
                errors.push(ReportError::at(
                    codes::INVALID_DEFINITION,
                    &at,
                    format!("`{nulls}` is not a null placement (first, last)"),
                ));
            }
        }
        errors
    }
}

/// The alias the base DocType carries in a compiled query.
pub const BASE_ALIAS: &str = "base";

/// The default output name of a column: the field path with `.` replaced by `_`, so a base
/// field keeps its name and a joined field is prefixed by its alias (`acct_title`). Unique
/// unless two columns name the same path.
#[must_use]
pub fn default_column_name(field: &str) -> String {
    field.replace('.', "_")
}

fn check_name(errors: &mut Vec<ReportError>, value: &str, at: &str) {
    if Name::new(value).is_err() {
        errors.push(ReportError::at(
            codes::INVALID_DEFINITION,
            at,
            format!(
                "`{value}` is not a valid name: lowercase letters, digits and underscores, \
                 starting with a letter"
            ),
        ));
    }
}

fn validate_filter(errors: &mut Vec<ReportError>, node: &FilterNode, at: &str) {
    match node {
        FilterNode::All(group) => {
            for (i, child) in group.all.iter().enumerate() {
                validate_filter(errors, child, &format!("{at}.all[{i}]"));
            }
        }
        FilterNode::Any(group) => {
            for (i, child) in group.any.iter().enumerate() {
                validate_filter(errors, child, &format!("{at}.any[{i}]"));
            }
        }
        FilterNode::Not(group) => validate_filter(errors, &group.not, &format!("{at}.not")),
        FilterNode::Cmp(cmp) => {
            if CompareOp::parse(&cmp.op).is_none() {
                errors.push(ReportError::at(
                    codes::INVALID_DEFINITION,
                    at,
                    format!("`{}` is not a comparison (eq, ne, lt, le, gt, ge)", cmp.op),
                ));
            }
            if cmp.field.trim().is_empty() {
                errors.push(ReportError::at(
                    codes::INVALID_DEFINITION,
                    at,
                    "a comparison needs a field",
                ));
            }
            if cmp.param.is_some() == cmp.value.is_some() {
                errors.push(ReportError::at(
                    codes::INVALID_DEFINITION,
                    at,
                    "a comparison binds to exactly one of `param` or `value`",
                ));
            }
            if let Some(value) = &cmp.value
                && let Err(message) = literal_to_value(value)
            {
                errors.push(ReportError::at(codes::BAD_TYPE, at, message));
            }
        }
    }
}

/// Converts a TOML literal to a storage value. Floats are refused (ADR-12); so are arrays and
/// tables, which a comparison cannot carry.
///
/// # Errors
///
/// A plain-language message naming what is wrong.
pub fn literal_to_value(value: &toml::Value) -> Result<Value, String> {
    Ok(match value {
        toml::Value::String(s) => Value::Text(s.clone()),
        toml::Value::Integer(i) => Value::Int(*i),
        toml::Value::Boolean(b) => Value::Bool(*b),
        toml::Value::Datetime(dt) => match dt.date {
            Some(date) => {
                let month =
                    time::Month::try_from(date.month).map_err(|_| "invalid month".to_owned())?;
                let date = Date::from_calendar_date(i32::from(date.year), month, date.day)
                    .map_err(|_| "invalid date".to_owned())?;
                Value::Date(date)
            }
            None => return Err("a datetime literal must be a calendar date".to_owned()),
        },
        toml::Value::Float(_) => {
            return Err("floats are not allowed; use an exact decimal as text".to_owned());
        }
        toml::Value::Array(_) => return Err("a list literal cannot be compared".to_owned()),
        toml::Value::Table(_) => return Err("a table literal cannot be compared".to_owned()),
    })
}

fn default_fits(ty: ParamType, value: &toml::Value) -> bool {
    matches!(
        (ty, value),
        (ParamType::Text, toml::Value::String(_))
            | (ParamType::Int, toml::Value::Integer(_))
            | (
                ParamType::Decimal,
                toml::Value::String(_) | toml::Value::Integer(_)
            )
            | (ParamType::Date, toml::Value::Datetime(_))
            | (ParamType::Link, toml::Value::String(_))
            | (ParamType::Select, toml::Value::String(_))
    )
}
