//! The query engine: evaluates the typed query IR over a read view (WP-CON-01.T3).
//!
//! The semantics are the module docs of `spot_storage_contract::query`. The engine is a plain
//! pipeline: join (nested loops), filter, lock the `from` rows, then either sort and page the
//! rows or group, filter the groups, sort and page them.

mod eval;
#[cfg(test)]
mod tests;

use std::cmp::Ordering;
use std::collections::HashMap;

use spot_storage_contract::query::{JoinKind, Nulls, Order, OrderDir};
use spot_storage_contract::{Catalog, Query, RecordId, Rows, StorageResult, Value};

use self::eval::{Binding, Env, Evaluator, Scope, collator_for, group_key};
use crate::locks::RowLocker;
use crate::store::{Record, View};

/// Validates `query` against `catalog` and runs it over `view`: committed data merged with the
/// running transaction's own writes (or committed data only, outside a transaction).
///
/// With a lock other than `RowLock::None`, call `locker.lock_row` for every `from` row that
/// passes the filter, before paging: `Ok(false)` leaves the row out (`SKIP LOCKED`), an error
/// ends the query. After a blocked `Wait::Block` lock the caller waits for the lock and runs the
/// query again, so a run must have no effects other than the locks it takes.
pub(crate) fn execute(
    catalog: &Catalog,
    view: &View<'_>,
    query: &Query,
    locker: &mut dyn RowLocker,
) -> StorageResult<Rows> {
    query.validate(catalog)?;
    crate::capabilities().check_query(query)?;
    let sources = std::iter::once(&query.from).chain(query.joins.iter().map(|j| &j.source));
    let aliases = sources.map(|s| &s.alias).collect();
    let ev = Evaluator::new(aliases, collator_for(&catalog.collation)?);

    let joined = joined_rows(query, view, &ev, locker)?;
    let columns = query.select.iter().map(|s| s.alias.clone()).collect();
    let rows = if query.is_aggregate() {
        grouped_rows(query, &ev, &joined)?
    } else {
        plain_rows(query, &ev, joined)?
    };
    Ok(Rows { columns, rows })
}

/// A row that passed the join conditions and the filter.
struct Candidate<'a> {
    /// The `from` record's id, the last sort key.
    from_id: RecordId,
    env: Env<'a>,
}

/// Joined rows in `from` id order, filtered, with the `from` rows locked.
fn joined_rows<'a>(
    query: &Query,
    view: &View<'a>,
    ev: &Evaluator<'_>,
    locker: &mut dyn RowLocker,
) -> StorageResult<Vec<Candidate<'a>>> {
    let right_tables: Vec<Vec<(RecordId, &'a Record)>> = query
        .joins
        .iter()
        .map(|j| view.rows(&j.source.entity).collect())
        .collect();
    let mut out = Vec::new();
    for (from_id, record) in view.rows(&query.from.entity) {
        let mut envs: Vec<Env<'a>> = vec![vec![Some(Binding {
            id: from_id,
            record,
        })]];
        for (join, table) in query.joins.iter().zip(&right_tables) {
            let mut next = Vec::new();
            for env in &envs {
                let before = next.len();
                for (id, rec) in table {
                    let mut candidate = env.clone();
                    candidate.push(Some(Binding {
                        id: *id,
                        record: rec,
                    }));
                    if ev.is_true(&join.on, Scope::Row(&candidate))? {
                        next.push(candidate);
                    }
                }
                if next.len() == before && join.kind == JoinKind::Left {
                    let mut candidate = env.clone();
                    candidate.push(None);
                    next.push(candidate);
                }
            }
            envs = next;
        }
        let mut kept = Vec::new();
        for env in envs {
            let keep = match &query.filter {
                Some(f) => ev.is_true(f, Scope::Row(&env))?,
                None => true,
            };
            if keep {
                kept.push(env);
            }
        }
        if kept.is_empty() || !locker.lock_row(&query.from.entity, from_id)? {
            continue;
        }
        out.extend(kept.into_iter().map(|env| Candidate { from_id, env }));
    }
    Ok(out)
}

/// A non-aggregate query: sort by the keys and then the `from` id, page, project.
fn plain_rows(
    query: &Query,
    ev: &Evaluator<'_>,
    joined: Vec<Candidate<'_>>,
) -> StorageResult<Vec<Vec<Value>>> {
    let mut keyed = Vec::with_capacity(joined.len());
    for c in joined {
        let keys = query
            .order_by
            .iter()
            .map(|o| ev.eval(&o.expr, Scope::Row(&c.env)))
            .collect::<StorageResult<Vec<_>>>()?;
        keyed.push((keys, c));
    }
    keyed.sort_by(|(ka, a), (kb, b)| {
        compare_keys(ev, &query.order_by, ka, kb).then_with(|| a.from_id.cmp(&b.from_id))
    });
    let mut out = Vec::new();
    for (_, c) in page(query, keyed) {
        out.push(project(query, ev, Scope::Row(&c.env))?);
    }
    Ok(out)
}

/// An aggregate query: group, filter the groups, sort them by the keys, page, project.
fn grouped_rows(
    query: &Query,
    ev: &Evaluator<'_>,
    joined: &[Candidate<'_>],
) -> StorageResult<Vec<Vec<Value>>> {
    let mut groups: Vec<Vec<&Env<'_>>> = Vec::new();
    if query.group_by.is_empty() {
        // Aggregating without grouping keys always gives one group, even over no rows.
        groups.push(joined.iter().map(|c| &c.env).collect());
    } else {
        let mut index: HashMap<String, usize> = HashMap::new();
        for c in joined {
            let mut key = String::new();
            for g in &query.group_by {
                let part = group_key(&ev.eval(g, Scope::Row(&c.env))?);
                key.push_str(&format!("{}:{part}", part.len()));
            }
            let at = *index.entry(key).or_insert_with(|| {
                groups.push(Vec::new());
                groups.len() - 1
            });
            groups[at].push(&c.env);
        }
    }
    let mut keyed = Vec::new();
    for rows in groups {
        let scope = Scope::Group(&rows);
        if let Some(h) = &query.having
            && !ev.is_true(h, scope)?
        {
            continue;
        }
        let keys = query
            .order_by
            .iter()
            .map(|o| ev.eval(&o.expr, scope))
            .collect::<StorageResult<Vec<_>>>()?;
        keyed.push((keys, project(query, ev, scope)?));
    }
    keyed.sort_by(|(ka, _), (kb, _)| compare_keys(ev, &query.order_by, ka, kb));
    Ok(page(query, keyed).map(|(_, row)| row).collect())
}

fn page<T>(query: &Query, rows: Vec<T>) -> impl Iterator<Item = T> {
    let skip = usize::try_from(query.offset).unwrap_or(usize::MAX);
    let take = query.limit.map_or(usize::MAX, |l| l as usize);
    rows.into_iter().skip(skip).take(take)
}

fn project(query: &Query, ev: &Evaluator<'_>, scope: Scope<'_, '_>) -> StorageResult<Vec<Value>> {
    query
        .select
        .iter()
        .map(|s| ev.eval(&s.expr, scope))
        .collect()
}

/// Compares two key vectors: each key by direction, nulls placed as asked whatever the
/// direction.
fn compare_keys(ev: &Evaluator<'_>, order: &[Order], a: &[Value], b: &[Value]) -> Ordering {
    for ((o, x), y) in order.iter().zip(a).zip(b) {
        let ord = match (x.is_null(), y.is_null()) {
            (true, true) => Ordering::Equal,
            (true, false) => null_side(o.nulls),
            (false, true) => null_side(o.nulls).reverse(),
            (false, false) => match o.dir {
                OrderDir::Asc => ev.sort_cmp(x, y),
                OrderDir::Desc => ev.sort_cmp(x, y).reverse(),
            },
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

/// How a null sorts against a value.
fn null_side(nulls: Nulls) -> Ordering {
    match nulls {
        Nulls::First => Ordering::Less,
        Nulls::Last => Ordering::Greater,
    }
}
