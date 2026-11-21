//! `check_query`: refusing the queries that would leak through filtering, sorting or
//! aggregating (ADR-070 section 12, rule 7).

use std::collections::BTreeMap;

use spot_permissions::{PermissionEngine, Principal};
use spot_storage_contract::{Expr, FieldRef, Name, Query};

use crate::error::PermQueryError;
use crate::{PermissionFilter, read::count_nodes};

/// The most nodes of a caller's query the checker walks (ADR-069).
const MAX_WALK: usize = 10_000;

/// The framework's own system columns: every entity has them, no DocType may declare one
/// (`spot_storage_contract::schema::system` reserves the names), and the permission snapshot
/// never lists them because they are not fields. They carry no business data, so they are the
/// one set of columns every principal may select, filter, sort, group and join on.
///
/// The storage contract owns the list, so a new system column reaches the checker with the
/// contract that defines it instead of drifting from a copy kept here.
pub(crate) const SYSTEM_COLUMNS: [&str; 5] = spot_storage_contract::schema::system::ALL;

/// Whether a field name is a storage system column, which carries no business data.
pub(crate) fn is_system(field: &Name) -> bool {
    SYSTEM_COLUMNS.contains(&field.as_str())
}

/// Every field reference in `e`, found without recursion.
pub(crate) fn field_refs(e: &Expr) -> Vec<&FieldRef> {
    let mut stack = vec![e];
    let mut out = Vec::new();
    while let Some(e) = stack.pop() {
        match e {
            Expr::Field(r) => out.push(r),
            Expr::Value(_) => {}
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
    out
}

impl PermissionFilter {
    /// Refuses a query that filters, sorts, groups, aggregates or joins on a field the
    /// principal may not read unmasked, or that computes a select column from one.
    ///
    /// Plain select columns are not refused here: [`PermissionFilter::restrict_columns`] drops
    /// or masks them, and drops a name the snapshot does not know for every principal.
    /// Joined sources are checked against their own DocType's rights, but their
    /// rows are not filtered: AND [`PermissionFilter::for_read`] of every source in.
    /// System columns ([`SYSTEM_COLUMNS`]) are always allowed. The error is the same for a
    /// hidden, a masked and a missing field.
    ///
    /// # Errors
    ///
    /// [`PermQueryError::FieldNotFilterable`], or [`PermQueryError::QueryTooLarge`].
    pub fn check_query(
        engine: &PermissionEngine,
        principal: &Principal,
        doctype: &str,
        query: &Query,
    ) -> Result<(), PermQueryError> {
        let mut entities: BTreeMap<&Name, &str> = BTreeMap::new();
        entities.insert(&query.from.alias, doctype);
        for j in &query.joins {
            entities.insert(&j.source.alias, j.source.entity.as_str());
        }
        let mut guarded: Vec<&Expr> = Vec::new();
        guarded.extend(query.filter.iter());
        guarded.extend(query.having.iter());
        guarded.extend(query.group_by.iter());
        guarded.extend(query.order_by.iter().map(|o| &o.expr));
        guarded.extend(query.joins.iter().map(|j| &j.on));
        guarded.extend(
            query
                .select
                .iter()
                .map(|s| &s.expr)
                .filter(|e| !matches!(e, Expr::Field(_))),
        );
        let mut nodes = 0usize;
        for e in guarded {
            nodes += count_nodes(e);
            if nodes > MAX_WALK {
                return Err(PermQueryError::QueryTooLarge);
            }
            for r in field_refs(e) {
                if is_system(&r.field) {
                    continue;
                }
                // A field the DocType does not have is refused like a hidden one, so the
                // answer never tells whether a field exists.
                let ok = entities.get(&r.alias).is_some_and(|entity| {
                    engine
                        .snapshot()
                        .facts(entity)
                        .is_some_and(|f| f.field(r.field.as_str()).is_some())
                        && engine.filter_ok(principal, entity, r.field.as_str())
                });
                if !ok {
                    let entity = entities.get(&r.alias).copied().unwrap_or(r.alias.as_str());
                    return Err(PermQueryError::FieldNotFilterable {
                        path: format!("{entity}.{}", r.field),
                    });
                }
            }
        }
        Ok(())
    }
}
