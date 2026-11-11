//! The draft compiled to a `spot-reports` definition and the storage query IR.
//!
//! [`to_definition`] turns a draft into the platform's report definition (data, never code);
//! [`plan`] validates the draft, compiles it against the effective metadata and the storage
//! catalog under ADR-069's limits, and keeps the chart and footer totals a report definition
//! does not carry. Every refusal — this crate's own and the report engine's — comes back as
//! [`Issue`]s in plain language, all at once.
//!
//! [`plan_for`] additionally checks every field the draft reads against the palette the
//! **author** may use ([`crate::fields`]), so a designer cannot build a report over a field
//! they could not report on. The published report is still checked for each viewer when it
//! runs; this is the builder's half of that rule.

use spot_permissions::{PermissionEngine, Principal};
use spot_reports::definition::BASE_ALIAS;
use spot_reports::definition::FilterCmp;
use spot_reports::{
    ColumnDef, CompiledReport, Compiler, EffectiveMetaSet, FilterNode, JoinDef, OrderDef,
    ParameterDef, ReportDef, ReportError, codes as report_codes,
};
use spot_storage_contract::Catalog;

use crate::chart::ChartSpec;
use crate::codes;
use crate::error::{Issue, is_blocked};
use crate::fields::offers;
use crate::report::{
    Compare, Direction, DraftFilter, DraftParamType, DraftSort, FilterTarget, Nulls, ReportDraft,
};
use crate::validate::report_issues;

/// A report draft ready to publish: its definition, its compiled form and the parts of it a
/// definition does not carry (the chart and the footer totals).
#[derive(Clone, Debug)]
pub struct ReportPlan {
    /// The report definition the draft compiles to.
    pub definition: ReportDef,
    /// The compiled report, with every field resolved and the IR validated.
    pub compiled: CompiledReport,
    /// The chart the report draws, when it declares one.
    pub chart: Option<ChartSpec>,
    /// The columns totalled in the footer.
    pub totals: Vec<String>,
    /// The warnings the draft keeps (an empty role list, for example); no error remains.
    pub issues: Vec<Issue>,
}

/// Compiles a draft against the site's metadata and catalog.
///
/// # Errors
///
/// Every issue the draft has, this crate's and the report engine's, in one list.
pub fn plan(
    draft: &ReportDraft,
    meta: &EffectiveMetaSet,
    catalog: &Catalog,
) -> Result<ReportPlan, Vec<Issue>> {
    finish(draft, meta, catalog, report_issues(draft))
}

/// As [`plan`], after checking every field the draft reads against the palette `author` may
/// use. A field the author may not read and report on is refused even though the report could
/// compile.
///
/// # Errors
///
/// Every issue the draft has, plus one per field outside the author's palette.
pub fn plan_for(
    draft: &ReportDraft,
    meta: &EffectiveMetaSet,
    catalog: &Catalog,
    engine: &PermissionEngine,
    author: &Principal,
) -> Result<ReportPlan, Vec<Issue>> {
    let mut issues = report_issues(draft);
    let palette = offers(engine, meta.meta(), author, &draft.base);
    // A link is offered under the alias of its link field, to one record type. A draft that
    // reuses an offered alias for a different record type, or a different link field, would
    // pass the field check below against the wrong palette, so the link itself must match.
    for (index, join) in draft.joins.iter().enumerate() {
        let offered = palette
            .join(&join.alias)
            .is_some_and(|offer| offer.doctype == join.doctype && offer.on == join.on);
        if !offered {
            issues.push(Issue::at(
                codes::FIELD_NOT_OFFERED,
                format!("joins[{index}]"),
                "You may not follow this link, so it cannot be part of the report.",
                "Choose a link you are offered, or ask an administrator for access.",
            ));
        }
    }
    for path in draft.used_fields() {
        if !palette.allows(&path) {
            issues.push(Issue::at(
                codes::FIELD_NOT_OFFERED,
                &path,
                "You may not read and report on this field, so it cannot be part of the report.",
                "Choose another field, or ask an administrator for access to this one.",
            ));
        }
    }
    finish(draft, meta, catalog, issues)
}

/// Compiles a draft, returning only the compiled report.
///
/// # Errors
///
/// Every issue the draft has.
pub fn compile(
    draft: &ReportDraft,
    meta: &EffectiveMetaSet,
    catalog: &Catalog,
) -> Result<CompiledReport, Vec<Issue>> {
    plan(draft, meta, catalog).map(|plan| plan.compiled)
}

fn finish(
    draft: &ReportDraft,
    meta: &EffectiveMetaSet,
    catalog: &Catalog,
    mut issues: Vec<Issue>,
) -> Result<ReportPlan, Vec<Issue>> {
    if is_blocked(&issues) {
        return Err(issues);
    }
    let definition = to_definition(draft);
    let compiler = Compiler::new(meta, catalog);
    match compiler.compile(&definition) {
        Ok(compiled) => Ok(ReportPlan {
            definition,
            compiled,
            chart: draft.chart.clone(),
            totals: draft.totals.clone(),
            issues,
        }),
        Err(errors) => {
            issues.extend(errors.iter().map(map_error));
            Err(issues)
        }
    }
}

/// The report definition a draft describes. Pure and infallible: a literal that cannot be
/// written is dropped here and refused with a friendly issue by [`plan`] before this runs.
#[must_use]
pub fn to_definition(draft: &ReportDraft) -> ReportDef {
    ReportDef {
        name: draft.name.clone(),
        label: draft.label.clone(),
        base: draft.base.clone(),
        joins: draft
            .joins
            .iter()
            .map(|join| JoinDef {
                alias: join.alias.clone(),
                doctype: join.doctype.clone(),
                on: join.on.clone(),
            })
            .collect(),
        columns: draft
            .columns
            .iter()
            .map(|column| ColumnDef {
                field: column.field.clone(),
                label: column.label.clone(),
                aggregate: column.aggregate.map(|a| a.as_str().to_owned()),
                format: column.format.clone(),
            })
            .collect(),
        computed: Vec::new(),
        parameters: draft
            .parameters
            .iter()
            .map(|parameter| ParameterDef {
                name: parameter.name.clone(),
                label: parameter.label.clone(),
                ty: parameter.ty.as_str().to_owned(),
                required: parameter.required,
                default: parameter.default.as_ref().and_then(|v| v.to_toml().ok()),
                options: (parameter.ty == DraftParamType::Select)
                    .then(|| parameter.options.clone()),
                doctype: parameter.doctype.clone(),
                for_filter: None,
            })
            .collect(),
        filters: filters_node(draft),
        group_by: draft.group_by.clone(),
        order_by: draft.sort.iter().map(order_def).collect(),
        limit: draft.limit,
        max_rows: draft.max_rows,
        roles: draft.roles.clone(),
    }
}

fn filters_node(draft: &ReportDraft) -> Option<FilterNode> {
    if draft.filters.is_empty() {
        return None;
    }
    let mut children: Vec<FilterNode> = Vec::with_capacity(draft.filters.len());
    for filter in &draft.filters {
        children.push(FilterNode::Cmp(filter_cmp(filter)));
    }
    match children.len() {
        1 => children.pop(),
        _ => Some(FilterNode::All(spot_reports::definition::FilterAll {
            all: children,
        })),
    }
}

fn filter_cmp(filter: &DraftFilter) -> FilterCmp {
    let (param, value) = match &filter.target {
        FilterTarget::Parameter { name } => (Some(name.clone()), None),
        FilterTarget::Value { value } => (None, value.to_toml().ok()),
    };
    FilterCmp {
        field: filter.field.clone(),
        op: compare_text(filter.op).to_owned(),
        param,
        value,
    }
}

fn compare_text(op: Compare) -> &'static str {
    op.as_str()
}

fn order_def(sort: &DraftSort) -> OrderDef {
    OrderDef {
        field: sort.field.clone(),
        dir: Some(
            match sort.direction {
                Direction::Asc => "asc",
                Direction::Desc => "desc",
            }
            .to_owned(),
        ),
        nulls: sort.nulls.map(|nulls| {
            match nulls {
                Nulls::First => "first",
                Nulls::Last => "last",
            }
            .to_owned()
        }),
    }
}

/// One report-engine refusal, as the designer reads it.
fn map_error(error: &ReportError) -> Issue {
    let (message, remedy) = match error.code() {
        report_codes::BASE_MISSING => (
            "This report is built on a kind of record the site does not have.".to_owned(),
            "Choose a kind of record this site has.".to_owned(),
        ),
        report_codes::UNKNOWN_FIELD => (
            "This report uses a field the record type does not have.".to_owned(),
            "Pick a field the record type still has.".to_owned(),
        ),
        report_codes::UNKNOWN_JOIN => (
            "This report follows a link that is not declared, or that no longer leads anywhere."
                .to_owned(),
            "Add the link again, or remove the fields and conditions that use it.".to_owned(),
        ),
        report_codes::UNKNOWN_PARAMETER => (
            "A condition uses a value the report does not offer.".to_owned(),
            "Add the value, or choose one the report already offers.".to_owned(),
        ),
        report_codes::DUPLICATE => (
            "Two things in this report share a name.".to_owned(),
            "Give each column, value and link its own name.".to_owned(),
        ),
        report_codes::BAD_TYPE => (
            "A value, total or summary does not fit the kind of field it uses.".to_owned(),
            "Choose a field of the right kind, or change the value.".to_owned(),
        ),
        report_codes::TOO_LARGE | report_codes::JOIN_LIMIT => (
            "This report is too large to run safely.".to_owned(),
            "Remove some columns, links or conditions.".to_owned(),
        ),
        report_codes::INVALID_DEFINITION | report_codes::EXPRESSION => (
            error.message().to_owned(),
            "Fix what the sentence names and try again.".to_owned(),
        ),
        report_codes::FIELD_DENIED => (
            "You may not report on one of the fields this report uses.".to_owned(),
            "Remove the field, or ask an administrator for access.".to_owned(),
        ),
        _ => (
            error.message().to_owned(),
            "Fix what the sentence names and try again.".to_owned(),
        ),
    };
    let issue = match error.path() {
        Some(path) => Issue::at(report_code(error), path, message, remedy),
        None => Issue::error(report_code(error), message, remedy),
    };
    issue.from_source(error.code())
}

/// The friendly code for a report-engine refusal.
fn report_code(error: &ReportError) -> &'static str {
    match error.code() {
        report_codes::BASE_MISSING => codes::UNKNOWN_DOCTYPE,
        report_codes::UNKNOWN_FIELD => codes::UNKNOWN_FIELD,
        report_codes::UNKNOWN_JOIN => codes::UNKNOWN_JOIN,
        report_codes::UNKNOWN_PARAMETER => codes::UNKNOWN_PARAMETER,
        report_codes::DUPLICATE => codes::DUPLICATE,
        report_codes::BAD_TYPE => codes::BAD_TYPE,
        report_codes::TOO_LARGE | report_codes::JOIN_LIMIT => codes::TOO_LARGE,
        report_codes::FIELD_DENIED => codes::FIELD_NOT_OFFERED,
        _ => codes::BAD_VALUE,
    }
}

/// The base alias a compiled report uses, re-exported so a test can name it without depending
/// on the report engine.
#[must_use]
pub fn base_alias() -> &'static str {
    BASE_ALIAS
}
