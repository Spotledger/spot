//! Expression checks at install time: parse every expression and resolve its `doc` paths.

use spot_expr::{Expr, Root, parse};

/// What a `doc` path may read.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Scope<'a> {
    /// Fields of a known DocType, or `None` when the caller's facts do not describe the DocType
    /// (then the path is only parsed, not resolved).
    Fields(Option<&'a [String]>),
    /// A component's declared props, by name.
    Props(&'a [String]),
}

/// Why an expression is refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExprProblem {
    /// A sentence naming the parser's message and offset, or the unknown field.
    pub(crate) message: String,
}

/// Parses `source` and checks every `doc` path against `scope`.
///
/// `what` names the scope in the message: `doctype `task``, or `the component's props`.
pub(crate) fn check(source: &str, scope: &Scope<'_>, what: &str) -> Result<(), ExprProblem> {
    let expr = parse(source).map_err(|error| ExprProblem {
        message: format!("{} at offset {}", error.message, error.offset),
    })?;
    match visit(&expr, scope, what) {
        Some(message) => Err(ExprProblem { message }),
        None => Ok(()),
    }
}

fn visit(expr: &Expr, scope: &Scope<'_>, what: &str) -> Option<String> {
    match expr {
        Expr::Path(root, names) => {
            if *root != Root::Doc {
                return None;
            }
            let first = names.first()?;
            let known = match scope {
                Scope::Fields(Some(fields)) => fields.iter().any(|field| field == first),
                Scope::Fields(None) => true,
                Scope::Props(props) => props.iter().any(|prop| prop == first),
            };
            if known {
                None
            } else {
                Some(format!("`doc.{first}` is not one of {what}"))
            }
        }
        Expr::List(items) => items.iter().find_map(|item| visit(item, scope, what)),
        Expr::Unary(_, inner) => visit(inner, scope, what),
        Expr::Binary(_, left, right) => {
            visit(left, scope, what).or_else(|| visit(right, scope, what))
        }
        Expr::Ternary(condition, then, otherwise) => visit(condition, scope, what)
            .or_else(|| visit(then, scope, what))
            .or_else(|| visit(otherwise, scope, what)),
        Expr::Call(_, args) => args.iter().find_map(|arg| visit(arg, scope, what)),
        Expr::Null | Expr::Bool(_) | Expr::Int(_) | Expr::Decimal(_) | Expr::Text(_) => None,
    }
}
