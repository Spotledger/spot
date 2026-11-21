//! Rule and field expressions against the new metadata.
//!
//! Every rule on the site, and every expression a property override carries (`depends_on`,
//! `mandatory_depends_on`, `read_only_depends_on` and an `=`-prefixed `default`), is parsed with
//! `spot-expr`. A string that no longer parses is reported; a parsed expression that reads a
//! field the release removes or renames is reported against that release item. Only the first
//! segment of a `doc.<field>.<child_field>` path is a field of the rule's DocType, so only it is
//! checked.

use spot_expr::{Expr, Root, parse};
use spot_meta::Property;

use crate::collect::Collector;
use crate::input::ReleaseContext;
use crate::model::{ArtefactKind, ArtefactRef, Cause, ConflictCategory};
use crate::release::ReleaseDelta;

/// Runs the rule and expression checks.
pub(crate) fn check(ctx: &ReleaseContext<'_>, delta: &ReleaseDelta, out: &mut Collector) {
    for rule in ctx.rules {
        let artefact = ArtefactRef::new(ArtefactKind::Rule, rule.name.clone());
        check_expression(
            &rule.expression,
            rule.doctype.as_deref(),
            artefact,
            delta,
            out,
        );
    }
    for over in ctx.customisations.overrides() {
        let Some(text) = expression_of(&over.property) else {
            continue;
        };
        let id = format!(
            "{}.{}#{}",
            over.doctype,
            over.field.as_deref().unwrap_or("*"),
            over.property.key()
        );
        let artefact = ArtefactRef::new(ArtefactKind::Rule, format!("override:{id}"));
        check_expression(&text, Some(&over.doctype), artefact, delta, out);
    }
}

/// The expression text of a property, when it is one.
fn expression_of(property: &Property) -> Option<String> {
    match property {
        Property::DependsOn(Some(text))
        | Property::MandatoryDependsOn(Some(text))
        | Property::ReadOnlyDependsOn(Some(text)) => Some(text.clone()),
        Property::Default(Some(text)) => text
            .strip_prefix('=')
            .filter(|rest| !rest.starts_with('='))
            .map(str::to_owned),
        _ => None,
    }
}

/// Parses `text` and reports a parse error or a reference to a removed field.
fn check_expression(
    text: &str,
    doctype: Option<&str>,
    artefact: ArtefactRef,
    delta: &ReleaseDelta,
    out: &mut Collector,
) {
    let parsed = match parse(text) {
        Ok(expr) => expr,
        Err(error) => {
            out.push(
                ConflictCategory::RuleParseError,
                Cause::new(
                    format!("release/expression/{}", artefact.id),
                    "the expression no longer parses in the shared expression language",
                ),
                artefact,
                format!(
                    "Fix the expression so it parses ({}: {}), or remove it.",
                    error.code, error.message
                ),
            );
            return;
        }
    };
    let mut roots = Vec::new();
    field_paths(&parsed, &mut roots);
    for segments in roots {
        let Some(field) = segments.first() else {
            continue;
        };
        let cause = match doctype {
            Some(doctype) => delta.field_cause(doctype, field),
            None => delta.field_cause_by_name(field),
        };
        if let Some((cause, resolution)) = cause {
            out.push(
                ConflictCategory::RuleFieldRemoved,
                cause,
                artefact.clone(),
                resolution,
            );
        }
    }
}

/// Collects the `doc.<field>...` paths of an expression, in source order.
fn field_paths(expr: &Expr, out: &mut Vec<Vec<String>>) {
    match expr {
        Expr::Path(Root::Doc, segments) => out.push(segments.clone()),
        Expr::Unary(_, inner) => field_paths(inner, out),
        Expr::Binary(_, left, right) => {
            field_paths(left, out);
            field_paths(right, out);
        }
        Expr::Ternary(condition, then, otherwise) => {
            field_paths(condition, out);
            field_paths(then, out);
            field_paths(otherwise, out);
        }
        Expr::Call(_, args) => {
            for arg in args {
                field_paths(arg, out);
            }
        }
        Expr::List(items) => {
            for item in items {
                field_paths(item, out);
            }
        }
        Expr::Path(Root::Parent, _)
        | Expr::Null
        | Expr::Bool(_)
        | Expr::Int(_)
        | Expr::Decimal(_)
        | Expr::Text(_) => {}
    }
}
