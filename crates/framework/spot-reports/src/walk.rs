//! Walking the storage query IR without recursion (ADR-069): the size and depth limits the
//! contract's amendment fixes.
//!
//! The IR has no visitor, so this module owns the one place that matches on every [`Expr`]
//! variant for these walks. The limits are enforced here because the storage contract's
//! `Query::validate` does not yet carry ADR-069's check (the contract amendment is a separate
//! work package): a report that would exceed a limit is refused **when it is loaded**.

use spot_storage_contract::{Expr, Query};

use crate::error::{ReportError, codes};

/// The depth of any one `Expr` tree (a leaf is depth 1), ADR-069 `MAX_EXPR_DEPTH`.
pub const MAX_EXPR_DEPTH: usize = 64;
/// The total `Expr` nodes in one `Query`, ADR-069 `MAX_QUERY_NODES`.
pub const MAX_QUERY_NODES: usize = 10_000;
/// The operands of one `And`/`Or`/`Coalesce` or the entries of one `In`, ADR-069 `MAX_LIST_LEN`.
pub const MAX_LIST_LEN: usize = 1_000;
/// `select`, `group_by` and `order_by` entries, each, ADR-069 `MAX_SELECT_COLUMNS`.
pub const MAX_SELECT_COLUMNS: usize = 256;
/// The joins one query may carry, ADR-069 `MAX_JOINS`.
pub const MAX_JOINS: usize = 16;

/// Whether `e` fits ADR-069's depth, node and list-length limits.
fn check_expr(e: &Expr, nodes: &mut usize) -> Result<(), ReportError> {
    let mut stack = vec![(e, 1usize)];
    while let Some((e, depth)) = stack.pop() {
        *nodes += 1;
        if *nodes > MAX_QUERY_NODES {
            return Err(ReportError::new(
                codes::TOO_LARGE,
                format!("a query may use at most {MAX_QUERY_NODES} expression nodes"),
            ));
        }
        if depth > MAX_EXPR_DEPTH {
            return Err(ReportError::new(
                codes::TOO_LARGE,
                format!("an expression nests at most {MAX_EXPR_DEPTH} deep"),
            ));
        }
        match e {
            Expr::Field(_) | Expr::Value(_) => {}
            Expr::Cmp(a, _, b) | Expr::Arith(a, _, b) => {
                stack.push((a, depth + 1));
                stack.push((b, depth + 1));
            }
            Expr::And(v) | Expr::Or(v) | Expr::Coalesce(v) => {
                if v.len() > MAX_LIST_LEN {
                    return Err(ReportError::new(
                        codes::TOO_LARGE,
                        format!("an expression list may hold at most {MAX_LIST_LEN} operands"),
                    ));
                }
                stack.extend(v.iter().map(|x| (x, depth + 1)));
            }
            Expr::Not(x)
            | Expr::IsNull(x)
            | Expr::Text(x, _)
            | Expr::Geo(x, _)
            | Expr::Range(x, _) => stack.push((x, depth + 1)),
            // ADR-087: a search term is one leaf.
            Expr::Search(_) | Expr::SearchRank(_) => {}
            Expr::In(x, values) => {
                if values.len() > MAX_LIST_LEN {
                    return Err(ReportError::new(
                        codes::TOO_LARGE,
                        format!("an IN list may hold at most {MAX_LIST_LEN} values"),
                    ));
                }
                stack.push((x, depth + 1));
            }
            Expr::Agg { arg, .. } => {
                if let Some(a) = arg {
                    stack.push((a, depth + 1));
                }
            }
        }
    }
    Ok(())
}

/// Refuses a query that exceeds an ADR-069 limit. Runs **before** [`Query::validate`], whose
/// walk recurses and is only safe on a tree within the limits.
///
/// # Errors
///
/// [`codes::TOO_LARGE`] or [`codes::JOIN_LIMIT`].
pub fn check_limits(query: &Query) -> Result<(), ReportError> {
    if query.joins.len() > MAX_JOINS {
        return Err(ReportError::new(
            codes::JOIN_LIMIT,
            format!("a report may join at most {MAX_JOINS} DocTypes"),
        ));
    }
    for (what, len) in [
        ("select", query.select.len()),
        ("group_by", query.group_by.len()),
        ("order_by", query.order_by.len()),
    ] {
        if len > MAX_SELECT_COLUMNS {
            return Err(ReportError::new(
                codes::TOO_LARGE,
                format!("`{what}` may hold at most {MAX_SELECT_COLUMNS} entries"),
            ));
        }
    }
    let mut nodes = 0usize;
    for e in query.filter.iter() {
        check_expr(e, &mut nodes)?;
    }
    for e in query.having.iter() {
        check_expr(e, &mut nodes)?;
    }
    for e in &query.group_by {
        check_expr(e, &mut nodes)?;
    }
    for o in &query.order_by {
        check_expr(&o.expr, &mut nodes)?;
    }
    for s in &query.select {
        check_expr(&s.expr, &mut nodes)?;
    }
    for j in &query.joins {
        check_expr(&j.on, &mut nodes)?;
    }
    Ok(())
}
