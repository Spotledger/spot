//! The rule graph and rule evaluation (WP-MET-02.T2).
//!
//! The rule properties of a field — `depends_on`, `mandatory_depends_on`, `read_only_depends_on`,
//! `fetch_from` and the `=`-prefixed expression default — are parsed **once**, when the plan is
//! compiled ([`crate::DocPlan::compile`]). This module then:
//!
//! - walks each parsed expression to find which fields it reads ([`RuleDependencyGraph::reads`])
//!   and, in reverse, which fields a change affects ([`RuleDependencyGraph::affects`]), so the
//!   form layer re-evaluates only what changed;
//! - refuses a rule that reads a field the DocType does not have (`plan.expr.unknown_field`,
//!   with the position in the expression) and a cycle among defaults and `fetch_from`
//!   (`plan.expr.cycle`, with the cycle listed);
//! - evaluates `is_mandatory`, `is_read_only` and `is_visible` over a
//!   [`spot_expr::Context`] built from a [`Document`], and computes a deterministic
//!   [`evaluation_order`].
//!
//! Everything here is pure: no clock, no I/O, no float arithmetic.

use std::collections::{BTreeMap, BTreeSet};

use spot_app_api::document::Document;
use spot_app_api::prelude::Value as StoredValue;
use spot_expr::{DateTime, Expr, MapContext, Root, Value as ExprValue};

use crate::error::PlanError;
use crate::plan::{DocPlan, FieldIx};

/// The rule kind, for diagnostics.
type RuleName = &'static str;

/// Tracks which fields each expression reads and the reverse map of what a change affects.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RuleDependencyGraph {
    /// For each field, the fields its rules read.
    pub reads: BTreeMap<FieldIx, BTreeSet<FieldIx>>,
    /// The reverse map: a field, and the fields whose rules read it.
    pub affects: BTreeMap<FieldIx, BTreeSet<FieldIx>>,
}

impl RuleDependencyGraph {
    /// Builds the graph for `plan`, refusing an unknown field in a rule and a cycle among the
    /// value-producing rules (defaults and `fetch_from`).
    ///
    /// # Errors
    ///
    /// Every [`PlanError`] found, all at once.
    pub fn build(plan: &DocPlan) -> Result<Self, Vec<PlanError>> {
        let doctype = plan.name().as_str().to_owned();
        let mut errors: Vec<PlanError> = Vec::new();
        let mut reads: BTreeMap<FieldIx, BTreeSet<FieldIx>> = BTreeMap::new();
        // Value-producing rules only: a default's reads and a fetch_from's link field.
        let mut value_reads: BTreeMap<FieldIx, BTreeSet<FieldIx>> = BTreeMap::new();

        for (index, field) in plan.fields().iter().enumerate() {
            let field_ix = FieldIx::new(index);
            let mut field_reads = BTreeSet::new();
            let mut default_reads = BTreeSet::new();

            let rules: [(RuleName, Option<&Expr>, Option<&String>); 3] = [
                (
                    "depends_on",
                    field.depends_on.as_deref(),
                    field.sources.depends_on.as_ref(),
                ),
                (
                    "mandatory_depends_on",
                    field.mandatory_depends_on.as_deref(),
                    field.sources.mandatory_depends_on.as_ref(),
                ),
                (
                    "read_only_depends_on",
                    field.read_only_depends_on.as_deref(),
                    field.sources.read_only_depends_on.as_ref(),
                ),
            ];
            for (rule, expr, source) in rules {
                if let Some(expr) = expr {
                    collect(
                        expr,
                        source.map_or("", String::as_str),
                        plan,
                        &doctype,
                        field.name.as_str(),
                        rule,
                        &mut field_reads,
                        &mut errors,
                    );
                }
            }
            if let Some(expr) = &field.default_expr {
                collect(
                    expr,
                    field.sources.default.as_deref().unwrap_or(""),
                    plan,
                    &doctype,
                    field.name.as_str(),
                    "default",
                    &mut default_reads,
                    &mut errors,
                );
                field_reads.extend(default_reads.iter().copied());
            }
            if let Some(fetch) = &field.fetch_from {
                match plan.field_ix(&fetch.link_field) {
                    Some(link_ix) => {
                        field_reads.insert(link_ix);
                        default_reads.insert(link_ix);
                    }
                    None => errors.push(PlanError::ExprInvalid {
                        doctype: doctype.clone(),
                        field: field.name.as_str().to_owned(),
                        rule: "fetch_from".to_owned(),
                        position: 0,
                        message: format!("link field `{}` does not exist", fetch.link_field),
                    }),
                }
            }

            if !field_reads.is_empty() {
                reads.insert(field_ix, field_reads);
            }
            if !default_reads.is_empty() {
                value_reads.insert(field_ix, default_reads);
            }
        }

        if !errors.is_empty() {
            return Err(errors);
        }
        if let Some(cycle) = detect_cycle(&value_reads) {
            return Err(vec![PlanError::ExprCycle {
                doctype,
                cycle: describe_cycle(plan, &cycle),
            }]);
        }

        let mut affects: BTreeMap<FieldIx, BTreeSet<FieldIx>> = BTreeMap::new();
        for (field_ix, field_reads) in &reads {
            for read in field_reads {
                affects.entry(*read).or_default().insert(*field_ix);
            }
        }
        Ok(Self { reads, affects })
    }

    /// Every field affected when `changed` fields change, following the rules transitively.
    #[must_use]
    pub fn affected_by_changes(&self, changed: &[FieldIx]) -> BTreeSet<FieldIx> {
        let mut affected = BTreeSet::new();
        let mut stack: Vec<FieldIx> = changed.to_vec();
        while let Some(field) = stack.pop() {
            if affected.insert(field)
                && let Some(dependents) = self.affects.get(&field)
            {
                for dependent in dependents {
                    if !affected.contains(dependent) {
                        stack.push(*dependent);
                    }
                }
            }
        }
        affected
    }
}

/// Collects the fields an expression reads, reporting an unknown field with its position.
#[allow(clippy::too_many_arguments)]
fn collect(
    expr: &Expr,
    source: &str,
    plan: &DocPlan,
    doctype: &str,
    field: &str,
    rule: RuleName,
    reads: &mut BTreeSet<FieldIx>,
    errors: &mut Vec<PlanError>,
) {
    match expr {
        Expr::Null | Expr::Bool(_) | Expr::Int(_) | Expr::Decimal(_) | Expr::Text(_) => {}
        Expr::List(items) => {
            for item in items {
                collect(item, source, plan, doctype, field, rule, reads, errors);
            }
        }
        Expr::Path(root, parts) => {
            // Only `doc.` reads a field of this DocType; `parent.` reads the parent of a child
            // row, which this plan cannot see, so it is not validated here.
            if *root == Root::Doc
                && let Some(name) = parts.first()
            {
                match plan.field_ix(name) {
                    Some(ix) => {
                        reads.insert(ix);
                    }
                    None => errors.push(PlanError::ExprUnknownField {
                        doctype: doctype.to_owned(),
                        field: field.to_owned(),
                        rule: rule.to_owned(),
                        unknown_field: name.clone(),
                        position: field_offset(source, name),
                    }),
                }
            }
        }
        Expr::Unary(_, arg) => collect(arg, source, plan, doctype, field, rule, reads, errors),
        Expr::Binary(_, left, right) => {
            collect(left, source, plan, doctype, field, rule, reads, errors);
            collect(right, source, plan, doctype, field, rule, reads, errors);
        }
        Expr::Ternary(cond, then_expr, else_expr) => {
            collect(cond, source, plan, doctype, field, rule, reads, errors);
            collect(then_expr, source, plan, doctype, field, rule, reads, errors);
            collect(else_expr, source, plan, doctype, field, rule, reads, errors);
        }
        Expr::Call(_, args) => {
            for arg in args {
                collect(arg, source, plan, doctype, field, rule, reads, errors);
            }
        }
    }
}

/// The character offset of `doc.<name>` in an expression's source, or 0 when it is not found
/// (a path the parser built from a call rather than a written reference).
fn field_offset(source: &str, name: &str) -> usize {
    let needle = format!("doc.{name}");
    source.find(&needle).map_or(0, |at| at + 4)
}

/// Finds a cycle among value-producing rules, as the fields on the cycle.
fn detect_cycle(value_reads: &BTreeMap<FieldIx, BTreeSet<FieldIx>>) -> Option<Vec<FieldIx>> {
    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        Visiting,
        Done,
    }
    let mut marks: BTreeMap<FieldIx, Mark> = BTreeMap::new();
    let mut path: Vec<FieldIx> = Vec::new();

    fn visit(
        node: FieldIx,
        value_reads: &BTreeMap<FieldIx, BTreeSet<FieldIx>>,
        marks: &mut BTreeMap<FieldIx, Mark>,
        path: &mut Vec<FieldIx>,
    ) -> Option<Vec<FieldIx>> {
        match marks.get(&node) {
            Some(Mark::Done) => return None,
            Some(Mark::Visiting) => {
                let start = path.iter().position(|&n| n == node).unwrap_or(0);
                return Some(path[start..].to_vec());
            }
            None => {}
        }
        marks.insert(node, Mark::Visiting);
        path.push(node);
        if let Some(edges) = value_reads.get(&node) {
            for edge in edges {
                if let Some(cycle) = visit(*edge, value_reads, marks, path) {
                    return Some(cycle);
                }
            }
        }
        path.pop();
        marks.insert(node, Mark::Done);
        None
    }

    for start in value_reads.keys() {
        if let Some(cycle) = visit(*start, value_reads, &mut marks, &mut path) {
            return Some(cycle);
        }
    }
    None
}

/// Renders a cycle as `field -> field -> field`.
fn describe_cycle(plan: &DocPlan, cycle: &[FieldIx]) -> String {
    let mut names: Vec<&str> = cycle
        .iter()
        .filter_map(|ix| plan.fields().get(ix.as_usize()))
        .map(|field| field.name.as_str())
        .collect();
    if let Some(first) = names.first().copied() {
        names.push(first);
    }
    names.join(" -> ")
}

/// A deterministic evaluation order: every field affected by `changed`, with each field after
/// the fields its rules read.
#[must_use]
pub fn evaluation_order(
    plan: &DocPlan,
    graph: &RuleDependencyGraph,
    changed: &[FieldIx],
) -> Vec<FieldIx> {
    let affected = graph.affected_by_changes(changed);
    let mut visited = BTreeSet::new();
    let mut visiting = BTreeSet::new();
    let mut ordered = Vec::new();
    for field in &affected {
        visit_field(*field, graph, &mut visited, &mut visiting, &mut ordered);
    }
    let _ = plan;
    ordered
}

fn visit_field(
    field: FieldIx,
    graph: &RuleDependencyGraph,
    visited: &mut BTreeSet<FieldIx>,
    visiting: &mut BTreeSet<FieldIx>,
    ordered: &mut Vec<FieldIx>,
) {
    if visited.contains(&field) || !visiting.insert(field) {
        return;
    }
    if let Some(dependencies) = graph.reads.get(&field) {
        for dependency in dependencies {
            visit_field(*dependency, graph, visited, visiting, ordered);
        }
    }
    visiting.remove(&field);
    visited.insert(field);
    ordered.push(field);
}

/// Whether a field is mandatory for `doc` at `now`: its `mandatory_depends_on` rule when it has one,
/// otherwise its static `required` flag.
///
/// # Errors
///
/// [`PlanError::ExprEval`] when the rule does not evaluate to a boolean or null.
pub fn is_mandatory(
    plan: &DocPlan,
    field: FieldIx,
    doc: &Document,
    now: DateTime,
) -> Result<bool, PlanError> {
    let planned = planned_field(plan, field)?;
    match &planned.mandatory_depends_on {
        Some(expr) => evaluate_flag(plan, planned, "mandatory_depends_on", expr, doc, now),
        None => Ok(planned.required),
    }
}

/// Whether a field is read-only for `doc`: its `read_only_depends_on` rule when it has one,
/// otherwise its static `read_only` flag.
///
/// # Errors
///
/// [`PlanError::ExprEval`] when the rule does not evaluate to a boolean or null.
pub fn is_read_only(
    plan: &DocPlan,
    field: FieldIx,
    doc: &Document,
    now: DateTime,
) -> Result<bool, PlanError> {
    let planned = planned_field(plan, field)?;
    match &planned.read_only_depends_on {
        Some(expr) => evaluate_flag(plan, planned, "read_only_depends_on", expr, doc, now),
        None => Ok(planned.read_only),
    }
}

/// Whether a field is visible for `doc`: hidden when its `depends_on` rule evaluates to false
/// (or null); visible when there is no rule.
///
/// # Errors
///
/// [`PlanError::ExprEval`] when the rule does not evaluate to a boolean or null.
pub fn is_visible(
    plan: &DocPlan,
    field: FieldIx,
    doc: &Document,
    now: DateTime,
) -> Result<bool, PlanError> {
    let planned = planned_field(plan, field)?;
    match &planned.depends_on {
        Some(expr) => evaluate_flag(plan, planned, "depends_on", expr, doc, now),
        None => Ok(true),
    }
}

/// The planned field at `field`, or a synthetic error for an out-of-range index.
fn planned_field(plan: &DocPlan, field: FieldIx) -> Result<&crate::plan::PlanField, PlanError> {
    plan.fields()
        .get(field.as_usize())
        .ok_or_else(|| PlanError::UnknownField {
            doctype: plan.name().as_str().to_owned(),
            field: field.as_usize().to_string(),
        })
}

/// Evaluates a boolean rule over `doc`.
fn evaluate_flag(
    plan: &DocPlan,
    field: &crate::plan::PlanField,
    rule: &str,
    expr: &Expr,
    doc: &Document,
    now: DateTime,
) -> Result<bool, PlanError> {
    let context = context(doc, now);
    match expr.eval(&context) {
        Ok(ExprValue::Bool(value)) => Ok(value),
        Ok(ExprValue::Null) => Ok(false),
        Ok(other) => Err(PlanError::ExprEval {
            doctype: plan.name().as_str().to_owned(),
            field: field.name.as_str().to_owned(),
            rule: rule.to_owned(),
            message: format!("expected a boolean, got {other:?}"),
        }),
        Err(error) => Err(PlanError::ExprEval {
            doctype: plan.name().as_str().to_owned(),
            field: field.name.as_str().to_owned(),
            rule: rule.to_owned(),
            message: format!("{}: {}", error.code, error.message),
        }),
    }
}

/// Builds the evaluation context from a document: every field under `doc.<name>`, with `now()`
/// and `today()` reading the caller's `now`. The caller supplies it because a rule that reads the
/// clock must never be answered from a made-up instant (the Unix epoch would silently hide or
/// require fields wrongly).
fn context(doc: &Document, now: DateTime) -> MapContext {
    let mut context = MapContext::new(now);
    for (name, value) in doc.fields() {
        context = context.with(&format!("doc.{}", name.as_str()), expr_value(value));
    }
    context
}

/// Converts a stored value to an expression value. A time (and bytes, a geo point, a range) has no expression type,
/// so it reads as `null`; an id reads as its text form.
fn expr_value(value: &StoredValue) -> ExprValue {
    match value {
        StoredValue::Null => ExprValue::Null,
        StoredValue::Bool(value) => ExprValue::Bool(*value),
        StoredValue::Int(value) => ExprValue::Int(*value),
        StoredValue::Decimal(value) => ExprValue::Decimal(*value),
        StoredValue::Text(value) => ExprValue::Text(value.clone()),
        StoredValue::Date(value) => {
            spot_expr::Date::parse(&value.to_string()).map_or(ExprValue::Null, ExprValue::Date)
        }
        StoredValue::DateTime(value) => {
            let micros = value
                .unix_timestamp()
                .saturating_mul(1_000_000)
                .saturating_add(i64::from(value.microsecond()));
            spot_expr::DateTime::from_micros(micros).map_or(ExprValue::Null, ExprValue::DateTime)
        }
        StoredValue::Time(_) => ExprValue::Null,
        StoredValue::Id(value) => ExprValue::Text(value.to_string()),
        StoredValue::Json(value) => ExprValue::Text(value.clone()),
        StoredValue::Bytes(_) | StoredValue::GeoPoint(_) | StoredValue::Range(_) => ExprValue::Null,
    }
}

impl DocPlan {
    /// Whether `field` is mandatory for `doc`; see [`is_mandatory`].
    ///
    /// # Errors
    ///
    /// [`PlanError::ExprEval`] when the rule does not evaluate to a boolean or null.
    pub fn is_mandatory(
        &self,
        field: FieldIx,
        doc: &Document,
        now: DateTime,
    ) -> Result<bool, PlanError> {
        is_mandatory(self, field, doc, now)
    }

    /// Whether `field` is read-only for `doc`; see [`is_read_only`].
    ///
    /// # Errors
    ///
    /// [`PlanError::ExprEval`] when the rule does not evaluate to a boolean or null.
    pub fn is_read_only(
        &self,
        field: FieldIx,
        doc: &Document,
        now: DateTime,
    ) -> Result<bool, PlanError> {
        is_read_only(self, field, doc, now)
    }

    /// Whether `field` is visible for `doc`; see [`is_visible`].
    ///
    /// # Errors
    ///
    /// [`PlanError::ExprEval`] when the rule does not evaluate to a boolean or null.
    pub fn is_visible(
        &self,
        field: FieldIx,
        doc: &Document,
        now: DateTime,
    ) -> Result<bool, PlanError> {
        is_visible(self, field, doc, now)
    }

    /// The deterministic order in which to evaluate the rules affected by `changed`.
    #[must_use]
    pub fn evaluation_order(&self, changed: &[FieldIx]) -> Vec<FieldIx> {
        evaluation_order(self, self.rules(), changed)
    }
}
