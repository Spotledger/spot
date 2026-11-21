//! `for_read`: the principal's read rights on one DocType as one filter.

use spot_permissions::{
    AssignmentContext, DoctypeFacts, PermissionEngine, Principal, ids::parse_record_id,
};
use spot_storage_contract::{Expr, Name, RecordId, Value};

use crate::analysis::{ReadGrant, read_grant, shared_read_ids, sorted_assignments};
use crate::error::PermQueryError;
use crate::pred::Pred;

/// The longest `In` list one operand carries (ADR-069 `MAX_LIST_LEN`).
pub const MAX_IN_LIST: usize = 1_000;
/// The most expression nodes the permission filter may use; the rest of ADR-069's 10,000 is
/// the caller's.
pub const MAX_FILTER_NODES: usize = 5_000;

/// Where the filter applies: the alias of the DocType's source in the caller's query, and the
/// column that holds the creator's user name (the owner rules compare it with the user).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadTarget {
    /// The source alias in the caller's query.
    pub alias: Name,
    /// The text field holding the creating user's name.
    pub owner_field: Name,
}

/// The rows a principal may read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReadFilter {
    /// Every row.
    All,
    /// No row: answer an empty page without querying.
    None,
    /// The rows this predicate holds for; the caller ANDs it onto its own filter.
    Filter(Expr),
}

/// The permission filter operations.
pub struct PermissionFilter;

impl PermissionFilter {
    /// The rows of `doctype` the principal may read, as a function of the principal and the
    /// engine's snapshot only (cache it per principal and snapshot version).
    ///
    /// It mirrors `PermissionEngine::can(.., Action::Read, ..)` row by row:
    ///
    /// - the system reads everything; a DocType the snapshot does not know reads nothing;
    /// - each assignment whose roles grant `read` contributes `scope`, and `scope AND owner =
    ///   user` when only `if_owner` grants do;
    /// - a scope restriction on a link or select field holds for a row whose field is null
    ///   (the engine only blocks a value the document holds) or listed in `allowed`;
    /// - shares of `read` add `id IN (...)`, but only where some assignment's scope covers the
    ///   row (a scope that blocks every assignment is final), or when the principal has no
    ///   assignment at all;
    /// - `include_descendants` is ignored, as the engine ignores it.
    ///
    /// # Errors
    ///
    /// [`PermQueryError::TooManyShares`] or [`PermQueryError::FilterTooLarge`].
    pub fn for_read(
        engine: &PermissionEngine,
        principal: &Principal,
        doctype: &str,
        target: &ReadTarget,
    ) -> Result<ReadFilter, PermQueryError> {
        if principal.is_system {
            return Ok(ReadFilter::All);
        }
        let snapshot = engine.snapshot();
        let Some(facts) = snapshot.facts(doctype) else {
            return Ok(ReadFilter::None);
        };
        let assignments = sorted_assignments(principal);
        let mut scopes = Vec::new();
        let mut parts = Vec::new();
        for a in &assignments {
            let scope = scope_pred(a, doctype, &facts, target)?;
            match read_grant(snapshot, a, doctype) {
                ReadGrant::Nothing => {}
                ReadGrant::Everything => parts.push(scope.clone()),
                ReadGrant::OwnerOnly => {
                    parts.push(Pred::and(vec![
                        scope.clone(),
                        owner_pred(principal, target),
                    ]));
                }
            }
            scopes.push(scope);
        }
        let shared = shared_read_ids(snapshot, principal);
        if !shared.is_empty() {
            let gate = if assignments.is_empty() {
                Pred::True
            } else {
                Pred::or(scopes)
            };
            parts.push(Pred::and(vec![gate, in_ids(&id_column(target), &shared)?]));
        }
        Ok(match Pred::or(parts) {
            Pred::True => ReadFilter::All,
            Pred::False => ReadFilter::None,
            Pred::Expr(e) => {
                let nodes = count_nodes(&e);
                if nodes > MAX_FILTER_NODES {
                    return Err(PermQueryError::FilterTooLarge { nodes });
                }
                ReadFilter::Filter(e)
            }
        })
    }
}

pub(crate) fn id_column(target: &ReadTarget) -> Expr {
    Expr::Field(spot_storage_contract::FieldRef {
        alias: target.alias.clone(),
        field: Name::new("id").unwrap_or_else(|_| target.owner_field.clone()),
    })
}

fn owner_pred(principal: &Principal, target: &ReadTarget) -> Pred {
    if principal.user.is_empty() {
        return Pred::False;
    }
    Pred::Expr(
        Expr::field(&target.alias, &target.owner_field)
            .eq(Expr::Value(Value::Text(principal.user.clone()))),
    )
}

/// The assignment's scope: every user permission that restricts a link or select field of the
/// DocType must hold.
fn scope_pred(
    a: &AssignmentContext,
    doctype: &str,
    facts: &DoctypeFacts,
    target: &ReadTarget,
) -> Result<Pred, PermQueryError> {
    let mut ups: Vec<_> = a.scope.iter().collect();
    ups.sort();
    let mut parts = Vec::new();
    for up in ups {
        if up.doctype.as_deref().is_some_and(|d| d != doctype) {
            continue;
        }
        for lf in &facts.links {
            let tgt = lf.target.clone().unwrap_or_default();
            let path = format!("{doctype}.{}", lf.name);
            if !up.restricts(doctype, &path, &tgt) {
                continue;
            }
            let Ok(field) = Name::new(&lf.name) else {
                continue;
            };
            let column = Expr::field(&target.alias, &field);
            let allowed = if lf.select_values.is_some() {
                let mut v: Vec<Value> = up.allowed.iter().cloned().map(Value::Text).collect();
                v.sort_by(|x, y| format!("{x:?}").cmp(&format!("{y:?}")));
                v.dedup();
                in_values(&column, v)?
            } else {
                let mut ids: Vec<RecordId> = up
                    .allowed
                    .iter()
                    .filter_map(|t| parse_record_id(t).ok())
                    .collect();
                ids.sort();
                ids.dedup();
                in_ids(&column, &ids)?
            };
            parts.push(Pred::or(vec![
                Pred::Expr(Expr::IsNull(Box::new(column))),
                allowed,
            ]));
        }
    }
    Ok(Pred::and(parts))
}

fn in_ids(column: &Expr, ids: &[RecordId]) -> Result<Pred, PermQueryError> {
    if ids.len() > MAX_IN_LIST * MAX_IN_LIST {
        return Err(PermQueryError::TooManyShares { count: ids.len() });
    }
    in_values(column, ids.iter().copied().map(Value::Id).collect())
}

/// `column IN values`, split into lists of at most [`MAX_IN_LIST`] joined by `Or`.
fn in_values(column: &Expr, values: Vec<Value>) -> Result<Pred, PermQueryError> {
    let chunks: Vec<Pred> = values
        .chunks(MAX_IN_LIST)
        .map(|c| Pred::Expr(Expr::In(Box::new(column.clone()), c.to_vec())))
        .collect();
    if chunks.len() > MAX_IN_LIST {
        return Err(PermQueryError::TooManyShares {
            count: values.len(),
        });
    }
    Ok(Pred::or(chunks))
}

/// The number of nodes of an expression, counted without recursion.
pub(crate) fn count_nodes(e: &Expr) -> usize {
    let mut stack = vec![e];
    let mut n = 0;
    while let Some(e) = stack.pop() {
        n += 1;
        match e {
            Expr::Field(_) | Expr::Value(_) => {}
            Expr::Cmp(a, _, b) | Expr::Arith(a, _, b) => {
                stack.push(a);
                stack.push(b);
            }
            Expr::And(v) | Expr::Or(v) | Expr::Coalesce(v) => stack.extend(v.iter()),
            Expr::Not(x)
            | Expr::IsNull(x)
            | Expr::In(x, _)
            | Expr::Text(x, _)
            | Expr::Geo(x, _)
            | Expr::Range(x, _) => stack.push(x),
            // ADR-087: a search term names fields of the `from` entity, not field references.
            // No backend serves search yet (the fake and the compiler refuse it); the permission
            // rules for search (ADR-087 section 7.7) must land before one does.
            Expr::Search(_) | Expr::SearchRank(_) => {}
            Expr::Agg { arg, .. } => stack.extend(arg.iter().map(|a| &**a)),
        }
    }
    n
}
