//! The report draft and its edits: a base record type, columns (including fields reached
//! through a link), filters with typed parameters, grouping with aggregates, sorting, footer
//! totals and a chart — changed one pure [`ReportOp`] at a time.
//!
//! [`apply_op`] is pure: it takes a [`ReportDraft`] and an op and returns the changed draft, or
//! an [`OpError`] that says what a designer must fix. Nothing is written, no site is touched
//! and no query runs. [`ReportEditor`] holds a draft and its op log, so undo and redo are
//! exact: the editor can replay the log from the initial draft and reach the same state, which
//! is what proves [`apply_op`] pure.

use spot_storage_contract::Name;

use crate::chart::ChartSpec;
use serde::{Deserialize, Serialize};

/// The most columns one report may declare (ADR-069's select-column limit).
pub const REPORT_MAX_COLUMNS: usize = 256;
/// The most links one report may follow (ADR-069's join limit).
pub const REPORT_MAX_JOINS: usize = 16;
/// The largest row limit the platform's report contract allows.
pub const ABSOLUTE_MAX_ROWS: u32 = 100_000;
/// The row limit when a draft declares none.
pub const DEFAULT_LIMIT: u32 = 1_000;
/// The largest row limit when a draft declares none.
pub const DEFAULT_MAX_ROWS: u32 = 10_000;

/// Where a draft is in its life.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DraftStatus {
    /// Being edited.
    #[default]
    Editing,
    /// Validated; ready to publish.
    Ready,
    /// Published to the site.
    Published,
    /// Discarded; it is never published.
    Discarded,
}

/// An aggregate a column carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Aggregate {
    /// Sum of the values.
    Sum,
    /// Number of rows (or of non-empty values).
    Count,
    /// Smallest value.
    Min,
    /// Largest value.
    Max,
    /// Exact average.
    Avg,
}

impl Aggregate {
    /// The name as written in a draft.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sum => "sum",
            Self::Count => "count",
            Self::Min => "min",
            Self::Max => "max",
            Self::Avg => "avg",
        }
    }

    /// Parses the written name.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "sum" => Some(Self::Sum),
            "count" => Some(Self::Count),
            "min" => Some(Self::Min),
            "max" => Some(Self::Max),
            "avg" => Some(Self::Avg),
            _ => None,
        }
    }
}

/// One output column: a field of the base record type (`field`) or of a link
/// (`alias.field`), optionally aggregated.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DraftColumn {
    /// The field path.
    pub field: String,
    /// The heading; the field's own label when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The aggregate; a plain value when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aggregate: Option<Aggregate>,
    /// A presentation hint (`money`, `date`, ...); it changes nothing stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
}

impl DraftColumn {
    /// A plain column over `field`.
    #[must_use]
    pub fn new(field: &str) -> Self {
        Self {
            field: field.to_owned(),
            label: None,
            aggregate: None,
            format: None,
        }
    }

    /// The same column as an aggregate.
    #[must_use]
    pub fn aggregated(mut self, aggregate: Aggregate) -> Self {
        self.aggregate = Some(aggregate);
        self
    }

    /// The same column with a heading.
    #[must_use]
    pub fn with_label(mut self, label: &str) -> Self {
        self.label = Some(label.to_owned());
        self
    }

    /// The same column with a presentation hint.
    #[must_use]
    pub fn with_format(mut self, format: &str) -> Self {
        self.format = Some(format.to_owned());
        self
    }

    /// The output name: the field path with `.` replaced by `_`, so `acct.title` becomes
    /// `acct_title` and a base field keeps its name.
    #[must_use]
    pub fn name(&self) -> String {
        self.field.replace('.', "_")
    }

    /// The heading a person reads.
    #[must_use]
    pub fn label_or_name(&self) -> String {
        self.label.clone().unwrap_or_else(|| self.name())
    }

    /// Whether the column aggregates.
    #[must_use]
    pub fn is_aggregate(&self) -> bool {
        self.aggregate.is_some()
    }
}

/// A link the report follows: `on` is the link field on the base record type (`account`) or on
/// an earlier link (`cmp.company`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DraftJoin {
    /// The alias column paths use (`acct.title`).
    pub alias: String,
    /// The record type the link points at.
    pub doctype: String,
    /// The link field the join follows.
    pub on: String,
}

impl DraftJoin {
    /// A link under `alias` to `doctype` following `on`.
    #[must_use]
    pub fn new(alias: &str, doctype: &str, on: &str) -> Self {
        Self {
            alias: alias.to_owned(),
            doctype: doctype.to_owned(),
            on: on.to_owned(),
        }
    }
}

/// The comparison a filter carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Compare {
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

impl Compare {
    /// The name as written in a draft.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Eq => "eq",
            Self::Ne => "ne",
            Self::Lt => "lt",
            Self::Le => "le",
            Self::Gt => "gt",
            Self::Ge => "ge",
        }
    }

    /// Parses the written name.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "eq" => Some(Self::Eq),
            "ne" => Some(Self::Ne),
            "lt" => Some(Self::Lt),
            "le" => Some(Self::Le),
            "gt" => Some(Self::Gt),
            "ge" => Some(Self::Ge),
            _ => None,
        }
    }
}

/// A literal a filter compares with or a parameter defaults to. Decimals are text, never
/// floats (ADR-12).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum FilterValue {
    /// Text.
    Text {
        /// The text.
        text: String,
    },
    /// A whole number.
    Int {
        /// The number.
        value: i64,
    },
    /// A yes/no value.
    Bool {
        /// The value.
        value: bool,
    },
    /// An exact decimal, written as text.
    Decimal {
        /// The decimal, for example `12.50`.
        text: String,
    },
    /// A calendar date, written as `YYYY-MM-DD`.
    Date {
        /// The date.
        text: String,
    },
}

impl FilterValue {
    /// A text value.
    #[must_use]
    pub fn text(value: &str) -> Self {
        Self::Text {
            text: value.to_owned(),
        }
    }

    /// The storage literal a definition carries.
    ///
    /// # Errors
    ///
    /// A plain-language message when a decimal is not exact or a date is malformed.
    pub fn to_toml(&self) -> Result<toml::Value, String> {
        Ok(match self {
            Self::Text { text } => toml::Value::String(text.clone()),
            Self::Int { value } => toml::Value::Integer(*value),
            Self::Bool { value } => toml::Value::Boolean(*value),
            Self::Decimal { text } => {
                if !is_exact_decimal(text) {
                    return Err("that is not an exact number".to_owned());
                }
                toml::Value::String(text.clone())
            }
            Self::Date { text } => {
                let parsed = text
                    .parse::<toml::value::Datetime>()
                    .map_err(|_| "that is not a date (use YYYY-MM-DD)".to_owned())?;
                toml::Value::Datetime(parsed)
            }
        })
    }
}

/// The right side of a filter: a value the viewer supplies, or a fixed literal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum FilterTarget {
    /// One of the report's parameters, by name.
    Parameter {
        /// The parameter.
        name: String,
    },
    /// A fixed literal.
    Value {
        /// The literal.
        value: FilterValue,
    },
}

impl FilterTarget {
    /// The target is the parameter called `name`.
    #[must_use]
    pub fn parameter(name: &str) -> Self {
        Self::Parameter {
            name: name.to_owned(),
        }
    }

    /// The target is a fixed text.
    #[must_use]
    pub fn text(text: &str) -> Self {
        Self::Value {
            value: FilterValue::text(text),
        }
    }

    /// The target is a fixed value.
    #[must_use]
    pub fn value(value: FilterValue) -> Self {
        Self::Value { value }
    }
}

/// One condition the report keeps: a field compared with a parameter or a literal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DraftFilter {
    /// The field path the condition reads.
    pub field: String,
    /// The comparison.
    pub op: Compare,
    /// What the field is compared with.
    pub target: FilterTarget,
}

impl DraftFilter {
    /// A condition.
    #[must_use]
    pub fn new(field: &str, op: Compare, target: FilterTarget) -> Self {
        Self {
            field: field.to_owned(),
            op,
            target,
        }
    }
}

/// The type of a parameter's value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DraftParamType {
    /// Text.
    Text,
    /// A whole number.
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

impl DraftParamType {
    /// The name as written in a draft.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Int => "int",
            Self::Decimal => "decimal",
            Self::Date => "date",
            Self::Link => "link",
            Self::Select => "select",
        }
    }

    /// Parses the written name.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "text" => Some(Self::Text),
            "int" => Some(Self::Int),
            "decimal" => Some(Self::Decimal),
            "date" => Some(Self::Date),
            "link" => Some(Self::Link),
            "select" => Some(Self::Select),
            _ => None,
        }
    }
}

/// One value a viewer may supply to the report.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DraftParameter {
    /// The name a run keys by.
    pub name: String,
    /// The heading the viewer reads.
    pub label: String,
    /// The value's type.
    pub ty: DraftParamType,
    /// The viewer must supply it.
    #[serde(default)]
    pub required: bool,
    /// The value used when the viewer supplies none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<FilterValue>,
    /// The allowed values of a select parameter.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    /// The record type a link parameter points at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub doctype: Option<String>,
}

impl DraftParameter {
    /// A text parameter.
    #[must_use]
    pub fn text(name: &str, label: &str) -> Self {
        Self {
            name: name.to_owned(),
            label: label.to_owned(),
            ty: DraftParamType::Text,
            required: false,
            default: None,
            options: Vec::new(),
            doctype: None,
        }
    }

    /// A date parameter.
    #[must_use]
    pub fn date(name: &str, label: &str) -> Self {
        Self {
            ty: DraftParamType::Date,
            ..Self::text(name, label)
        }
    }

    /// A select parameter with its allowed values.
    #[must_use]
    pub fn select(name: &str, label: &str, options: &[&str]) -> Self {
        Self {
            ty: DraftParamType::Select,
            options: options.iter().map(|o| (*o).to_owned()).collect(),
            ..Self::text(name, label)
        }
    }

    /// The same parameter, required.
    #[must_use]
    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }

    /// The same parameter with a default.
    #[must_use]
    pub fn defaulting(mut self, value: FilterValue) -> Self {
        self.default = Some(value);
        self
    }
}

/// A sort direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    /// Smallest first.
    Asc,
    /// Largest first.
    Desc,
}

/// Where empty values sort.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Nulls {
    /// Empty values first.
    First,
    /// Empty values last.
    Last,
}

/// One sort key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DraftSort {
    /// A column name or a field path.
    pub field: String,
    /// The direction.
    pub direction: Direction,
    /// Where empty values sort; the direction's natural place when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nulls: Option<Nulls>,
}

/// A draft report: everything a business user builds before it is published.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportDraft {
    /// The draft's id.
    pub id: String,
    /// Who owns it.
    pub owner: String,
    /// The report's storage name once published.
    pub name: String,
    /// The heading a person reads.
    pub label: String,
    /// The base record type.
    pub base: String,
    /// Where the draft is in its life.
    #[serde(default)]
    pub status: DraftStatus,
    /// The output columns.
    #[serde(default)]
    pub columns: Vec<DraftColumn>,
    /// The links the columns and filters may follow.
    #[serde(default)]
    pub joins: Vec<DraftJoin>,
    /// The values a viewer may supply.
    #[serde(default)]
    pub parameters: Vec<DraftParameter>,
    /// The conditions every row must meet.
    #[serde(default)]
    pub filters: Vec<DraftFilter>,
    /// The fields the report groups by.
    #[serde(default)]
    pub group_by: Vec<String>,
    /// The sort keys.
    #[serde(default)]
    pub sort: Vec<DraftSort>,
    /// The columns totalled in the footer.
    #[serde(default)]
    pub totals: Vec<String>,
    /// The chart the report draws.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chart: Option<ChartSpec>,
    /// The default row limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// The largest row limit a run honours.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rows: Option<u32>,
    /// The roles that may open the report once published.
    #[serde(default)]
    pub roles: Vec<String>,
}

impl ReportDraft {
    /// An empty draft over `base`, owned by `owner`.
    #[must_use]
    pub fn new(id: &str, owner: &str, name: &str, label: &str, base: &str) -> Self {
        Self {
            id: id.to_owned(),
            owner: owner.to_owned(),
            name: name.to_owned(),
            label: label.to_owned(),
            base: base.to_owned(),
            status: DraftStatus::Editing,
            columns: Vec::new(),
            joins: Vec::new(),
            parameters: Vec::new(),
            filters: Vec::new(),
            group_by: Vec::new(),
            sort: Vec::new(),
            totals: Vec::new(),
            chart: None,
            limit: None,
            max_rows: None,
            roles: Vec::new(),
        }
    }

    /// The column over `field`, when the draft has one.
    #[must_use]
    pub fn column(&self, field: &str) -> Option<&DraftColumn> {
        self.columns.iter().find(|c| c.field == field)
    }

    /// Every output column name, in column order.
    #[must_use]
    pub fn column_names(&self) -> Vec<String> {
        self.columns.iter().map(DraftColumn::name).collect()
    }

    /// The parameter called `name`, when the draft has one.
    #[must_use]
    pub fn parameter(&self, name: &str) -> Option<&DraftParameter> {
        self.parameters.iter().find(|p| p.name == name)
    }

    /// The link under `alias`, when the draft has one.
    #[must_use]
    pub fn join(&self, alias: &str) -> Option<&DraftJoin> {
        self.joins.iter().find(|j| j.alias == alias)
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

    /// Every field path the draft reads: columns, filters, grouping and sorting.
    #[must_use]
    pub fn used_fields(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut add = |path: &str| {
            if !path.is_empty() && !out.iter().any(|p| p == path) {
                out.push(path.to_owned());
            }
        };
        for column in &self.columns {
            add(&column.field);
        }
        for filter in &self.filters {
            add(&filter.field);
        }
        for field in &self.group_by {
            add(field);
        }
        for sort in &self.sort {
            add(&sort.field);
        }
        out
    }

    /// A deterministic text form of the draft, used for a publish's digest: the same content
    /// always gives the same text.
    #[must_use]
    pub fn canonical_text(&self) -> String {
        let mut out = String::new();
        out.push_str("report\n");
        out.push_str(&format!("name={}\n", self.name));
        out.push_str(&format!("label={}\n", self.label));
        out.push_str(&format!("base={}\n", self.base));
        for column in &self.columns {
            out.push_str(&format!(
                "column={}|{}|{}|{}\n",
                column.field,
                column.label.clone().unwrap_or_default(),
                column.aggregate.map(Aggregate::as_str).unwrap_or_default(),
                column.format.clone().unwrap_or_default()
            ));
        }
        for join in &self.joins {
            out.push_str(&format!(
                "join={}|{}|{}\n",
                join.alias, join.doctype, join.on
            ));
        }
        for parameter in &self.parameters {
            out.push_str(&format!(
                "parameter={}|{}|{}|{}|{}\n",
                parameter.name,
                parameter.label,
                parameter.ty.as_str(),
                parameter.required,
                parameter.default.is_some()
            ));
        }
        for filter in &self.filters {
            out.push_str(&format!(
                "filter={}|{}|{}\n",
                filter.field,
                filter.op.as_str(),
                filter_target_text(&filter.target)
            ));
        }
        for field in &self.group_by {
            out.push_str(&format!("group_by={field}\n"));
        }
        for sort in &self.sort {
            out.push_str(&format!(
                "sort={}|{}|{}\n",
                sort.field,
                match sort.direction {
                    Direction::Asc => "asc",
                    Direction::Desc => "desc",
                },
                sort.nulls
                    .map(|n| match n {
                        Nulls::First => "first",
                        Nulls::Last => "last",
                    })
                    .unwrap_or("")
            ));
        }
        for total in &self.totals {
            out.push_str(&format!("total={total}\n"));
        }
        if let Some(chart) = &self.chart {
            out.push_str(&format!(
                "chart={}|{}|{}\n",
                chart.kind.as_str(),
                chart.x.clone().unwrap_or_default(),
                chart
                    .series
                    .iter()
                    .map(|s| s.column.clone())
                    .collect::<Vec<_>>()
                    .join(",")
            ));
        }
        out
    }
}

fn filter_target_text(target: &FilterTarget) -> String {
    match target {
        FilterTarget::Parameter { name } => format!("parameter:{name}"),
        FilterTarget::Value { value } => match value {
            FilterValue::Text { text }
            | FilterValue::Decimal { text }
            | FilterValue::Date { text } => text.clone(),
            FilterValue::Int { value } => value.to_string(),
            FilterValue::Bool { value } => value.to_string(),
        },
    }
}

pub use crate::report_edit::{ReportEditor, ReportOp, apply_op};

/// Whether `text` is a valid record-type or field name.
#[must_use]
pub fn is_name(text: &str) -> bool {
    Name::new(text).is_ok()
}

// A tiny exact-decimal check: the report engine re-parses the value when it compiles, so this
// only refuses text that could never be a decimal (no exponent, no comma, no float).

/// Whether `text` is a plain exact decimal: an optional sign, digits and at most one point
/// with at least one digit on each side.
#[must_use]
pub fn is_exact_decimal(text: &str) -> bool {
    let digits = text.strip_prefix('-').unwrap_or(text);
    let mut parts = digits.split('.');
    let whole = parts.next().unwrap_or_default();
    let fraction = parts.next();
    if parts.next().is_some() {
        return false;
    }
    !whole.is_empty()
        && whole.chars().all(|c| c.is_ascii_digit())
        && fraction.is_none_or(|f| !f.is_empty() && f.chars().all(|c| c.is_ascii_digit()))
}
