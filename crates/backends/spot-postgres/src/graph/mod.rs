//! `GraphTraversal` for [`PgTransaction`]: breadth-first walks as one recursive query per call
//! (WP-STO-07.T1, ADR-50, ADR-053 "Graph"; SPK-14: recursive queries, no graph extension).
//!
//! # Shape of the query
//!
//! `WITH RECURSIVE reach(...)` has **one row per level**, not one per path. A row carries the
//! level's depth, the nodes visited so far (two parallel arrays, entity and id) and the nodes
//! first reached at this level with, for each, its predecessor and the edge type used. The
//! non-recursive term is level 0: the start records that exist. The recursive term reads the
//! previous level's new nodes, follows every edge type in the requested direction, drops what
//! the visited arrays already hold, keeps one row per newly reached node (`DISTINCT ON`) and
//! appends them. Two things follow:
//!
//! - **Termination and cost.** A node is visited once, so cycles end and the work is bounded by
//!   the edges of the reached subgraph times the depth, never by the number of paths. (A
//!   per-path `CYCLE` clause would enumerate every path: exponential on a lattice.) Recursion
//!   also stops at `max_depth`.
//! - **`max_nodes` inside the query.** The recursion stops as soon as the visited arrays hold
//!   more than `max_nodes` nodes, and each level is cut with `LIMIT max_nodes + 1 - visited`, so
//!   no more than `max_nodes + 1` nodes are ever collected or fetched. The caller sees
//!   `TraversalTooLarge` when the final count exceeds `max_nodes`; nothing is truncated.
//!
//! Aggregates may not appear in a recursive term itself, so the per-level grouping sits in a
//! `LATERAL` subquery. One statement means one snapshot: the walk is consistent even at `READ
//! COMMITTED`, and it sees the transaction's own writes.
//!
//! # Rules (the contract's, as the in-memory fake implements them)
//!
//! - Each node appears once, at its smallest depth.
//! - Its predecessor is the smallest `(entity, id)` at `depth - 1` that has an edge to it; if
//!   several edge types connect that pair, the smallest edge type `(entity, field)`. All
//!   comparisons are bytewise (`COLLATE "C"`; uuids compare bytewise).
//! - Outgoing: from a record of the edge's entity to the record its link names, if that record
//!   exists. Incoming: from a record to the records of the edge's entity whose link names it.
//!   A dynamic link follows only when its entity field names an entity of the catalog.
//! - Output is ordered by depth, then entity, then id.
//!
//! Identifiers come from the validated catalog and are quoted; every value (entity names, ids,
//! depth, limits) is a bound parameter. Incoming edges need an index on the link column to be
//! cheap; the catalog's declared indexes decide that.

use std::collections::{BTreeMap, BTreeSet};

use spot_storage_contract::schema::system;
use spot_storage_contract::{
    Catalog, Direction, EdgeSpec, EntityKind, ErrorKind, FieldType, LinkTarget, Name, NodeRef,
    Reached, RecordId, StorageError, StorageResult, Traversal, TraversalResult,
};
use tokio_postgres::types::ToSql;
use uuid::Uuid;

use crate::PgTransaction;
use crate::schema::names::quote;

/// Deepest level a traversal may ask for.
const MAX_DEPTH: u16 = 64;

/// Where an edge type points.
enum Target {
    /// Always this entity.
    Fixed(Name),
    /// The entity named by this text field of the linking record.
    Dynamic(Name),
}

struct Edge {
    spec: EdgeSpec,
    target: Target,
}

fn invalid(msg: String) -> StorageError {
    StorageError::new(ErrorKind::InvalidQuery, msg)
}

fn resolve(catalog: &Catalog, spec: &EdgeSpec) -> StorageResult<Edge> {
    let def = catalog.entity(&spec.entity).map_err(|_| {
        invalid(format!(
            "edge entity `{}` is not in the catalog",
            spec.entity
        ))
    })?;
    let target = match (&def.kind, spec.field.as_str()) {
        (EntityKind::Child { parent }, system::PARENT) => Target::Fixed(parent.clone()),
        _ => match def.field(&spec.field).map(|f| &f.ty) {
            Ok(FieldType::Link {
                target: LinkTarget::Entity(t),
                ..
            }) => Target::Fixed(t.clone()),
            Ok(FieldType::Link {
                target: LinkTarget::Dynamic { entity_field },
                ..
            }) => Target::Dynamic(entity_field.clone()),
            _ => {
                return Err(invalid(format!(
                    "`{}.{}` is not a link field and cannot be an edge",
                    spec.entity, spec.field
                )));
            }
        },
    };
    Ok(Edge {
        spec: spec.clone(),
        target,
    })
}

/// Bound parameters, numbered in order of first use.
#[derive(Default)]
struct Params {
    values: Vec<Box<dyn ToSql + Sync + Send>>,
    texts: BTreeMap<String, usize>,
}

impl Params {
    /// `$n::text`, reusing the parameter for equal text.
    fn text(&mut self, text: &str) -> String {
        if let Some(n) = self.texts.get(text) {
            return format!("${n}::text");
        }
        self.values.push(Box::new(text.to_owned()));
        let n = self.values.len();
        self.texts.insert(text.to_owned(), n);
        format!("${n}::text")
    }

    fn uuids(&mut self, ids: Vec<Uuid>) -> String {
        self.values.push(Box::new(ids));
        format!("${}::uuid[]", self.values.len())
    }

    fn bigint(&mut self, value: i64) -> String {
        self.values.push(Box::new(value));
        format!("${}::bigint", self.values.len())
    }
}

/// The frontier: the nodes first reached at the previous level.
const FRONTIER: &str = "unnest(r.nw_e, r.nw_i) AS f(e, i)";

/// The `SELECT`s that follow `edge` (number `index` among the sorted edge types) from the
/// frontier. Each yields `(e, i, pe, pi, x)`: the reached node, its predecessor and the edge.
fn branches(
    catalog: &Catalog,
    edge: &Edge,
    index: usize,
    direction: Direction,
    params: &mut Params,
) -> Vec<String> {
    let table = quote(edge.spec.entity.as_str());
    let column = quote(edge.spec.field.as_str());
    let this = params.text(edge.spec.entity.as_str());
    let x = format!("{index}::int");
    let mut out = Vec::new();
    let (outgoing, incoming) = match direction {
        Direction::Outgoing => (true, false),
        Direction::Incoming => (false, true),
        Direction::Both => (true, true),
    };
    match &edge.target {
        Target::Fixed(target) => {
            let target_text = params.text(target.as_str());
            if outgoing {
                out.push(format!(
                    "SELECT {target_text} AS e, t.\"id\" AS i, f.e AS pe, f.i AS pi, {x} AS x \
                     FROM {FRONTIER} JOIN {table} s ON s.\"id\" = f.i \
                     JOIN {tt} t ON t.\"id\" = s.{column} WHERE f.e = {this}",
                    tt = quote(target.as_str()),
                ));
            }
            if incoming {
                out.push(format!(
                    "SELECT {this} AS e, s.\"id\" AS i, f.e AS pe, f.i AS pi, {x} AS x \
                     FROM {FRONTIER} JOIN {table} s ON s.{column} = f.i WHERE f.e = {target_text}"
                ));
            }
        }
        Target::Dynamic(entity_field) => {
            let names = quote(entity_field.as_str());
            if outgoing {
                // The target's table depends on the row's entity field: one branch per entity.
                for target in catalog.entities() {
                    let target_text = params.text(target.name.as_str());
                    out.push(format!(
                        "SELECT {target_text} AS e, t.\"id\" AS i, f.e AS pe, f.i AS pi, {x} AS x \
                         FROM {FRONTIER} JOIN {table} s ON s.\"id\" = f.i \
                         JOIN {tt} t ON t.\"id\" = s.{column} \
                         WHERE f.e = {this} AND s.{names} = {target_text}",
                        tt = quote(target.name.as_str()),
                    ));
                }
            }
            if incoming {
                out.push(format!(
                    "SELECT {this} AS e, s.\"id\" AS i, f.e AS pe, f.i AS pi, {x} AS x \
                     FROM {FRONTIER} JOIN {table} s ON s.{column} = f.i AND s.{names} = f.e"
                ));
            }
        }
    }
    out
}

/// Walks links from the start nodes.
pub(crate) async fn traverse(
    tx: &mut PgTransaction,
    traversal: &Traversal,
) -> StorageResult<TraversalResult> {
    let catalog = std::sync::Arc::clone(tx.catalog_arc());
    let plan = plan(&catalog, traversal).map_err(|e| tx.fail_storage(e))?;
    let Some((sql, params, edges)) = plan else {
        return Ok(TraversalResult::default());
    };
    let args: Vec<&(dyn ToSql + Sync)> = params.values.iter().map(|b| &**b as _).collect();
    let client = tx.client()?;
    let result = client.query(&sql, &args).await;
    let rows = result.map_err(|e| tx.fail(&e, None))?;
    let mut nodes = Vec::with_capacity(rows.len());
    for row in &rows {
        if row.get::<_, i64>(1) > i64::from(traversal.max_nodes) {
            return Err(tx.fail_storage(too_large(traversal.max_nodes)));
        }
        nodes.push(reached(row, &edges).map_err(|e| tx.fail_storage(e))?);
    }
    Ok(TraversalResult { nodes })
}

fn too_large(max_nodes: u32) -> StorageError {
    StorageError::new(
        ErrorKind::TraversalTooLarge,
        format!("the traversal reaches more than {max_nodes} nodes; narrow it or raise max_nodes"),
    )
}

fn node(entity: &str, id: Uuid) -> StorageResult<NodeRef> {
    Ok(NodeRef {
        entity: Name::new(entity)?,
        id: RecordId::from_bytes(*id.as_bytes())?,
    })
}

fn reached(row: &tokio_postgres::Row, edges: &[Edge]) -> StorageResult<Reached> {
    let corrupt = || {
        StorageError::new(
            ErrorKind::Internal,
            "a traversal row could not be read in the shape the query builds",
        )
    };
    let depth =
        u16::try_from(row.try_get::<_, i32>(0).map_err(|_| corrupt())?).map_err(|_| corrupt())?;
    let entity: String = row.try_get(2).map_err(|_| corrupt())?;
    let id: Uuid = row.try_get(3).map_err(|_| corrupt())?;
    let via = if depth == 0 {
        None
    } else {
        let pe: String = row.try_get(4).map_err(|_| corrupt())?;
        let pi: Uuid = row.try_get(5).map_err(|_| corrupt())?;
        let x = usize::try_from(row.try_get::<_, i32>(6).map_err(|_| corrupt())?)
            .map_err(|_| corrupt())?;
        let spec = edges.get(x).ok_or_else(corrupt)?.spec.clone();
        Some((node(&pe, pi)?, spec))
    };
    Ok(Reached {
        node: node(&entity, id)?,
        depth,
        via,
    })
}

/// Checks the traversal like every backend does and builds the statement. `None` when there is
/// nothing to walk from.
fn plan(catalog: &Catalog, t: &Traversal) -> StorageResult<Option<(String, Params, Vec<Edge>)>> {
    if t.edges.is_empty() {
        return Err(invalid("a traversal needs at least one edge type".into()));
    }
    if !(1..=MAX_DEPTH).contains(&t.max_depth) {
        return Err(invalid(format!(
            "max_depth must be 1 to {MAX_DEPTH}, not {}",
            t.max_depth
        )));
    }
    // Sorted and without repeats, so an edge's number orders like its `EdgeSpec`.
    let specs: BTreeSet<&EdgeSpec> = t.edges.iter().collect();
    let edges = specs
        .into_iter()
        .map(|spec| resolve(catalog, spec))
        .collect::<StorageResult<Vec<_>>>()?;
    let mut starts: BTreeMap<&Name, Vec<Uuid>> = BTreeMap::new();
    for s in &t.start {
        catalog.entity(&s.entity)?;
        starts
            .entry(&s.entity)
            .or_default()
            .push(Uuid::from_bytes(*s.id.as_bytes()));
    }
    if starts.is_empty() {
        return Ok(None);
    }

    let mut params = Params::default();
    let seeds: Vec<String> = starts
        .into_iter()
        .map(|(entity, ids)| {
            let (name, ids) = (params.text(entity.as_str()), params.uuids(ids));
            format!(
                "SELECT {name} AS e, \"id\" AS i FROM {} WHERE \"id\" = ANY({ids})",
                quote(entity.as_str())
            )
        })
        .collect();
    let mut found = Vec::new();
    for (index, edge) in edges.iter().enumerate() {
        found.extend(branches(catalog, edge, index, t.direction, &mut params));
    }
    let max_nodes = params.bigint(i64::from(t.max_nodes));
    let max_depth = params.bigint(i64::from(t.max_depth));
    let seeds = seeds.join(" UNION ALL ");
    let found = found.join(" UNION ALL ");
    let order = "ORDER BY s.e COLLATE \"C\", s.i";
    let sql = format!(
        "WITH RECURSIVE reach(depth, vis_e, vis_i, nw_e, nw_i, nw_pe, nw_pi, nw_x) AS (\
           SELECT 0, array_agg(s.e {order}), array_agg(s.i {order}), \
                  array_agg(s.e {order}), array_agg(s.i {order}), \
                  array_agg(NULL::text {order}), array_agg(NULL::uuid {order}), \
                  array_agg(NULL::int {order}) \
           FROM ({seeds}) s HAVING count(*) > 0 \
         UNION ALL \
           SELECT r.depth + 1, r.vis_e || n.e, r.vis_i || n.i, n.e, n.i, n.pe, n.pi, n.x \
           FROM reach r CROSS JOIN LATERAL (\
             SELECT array_agg(d.e ORDER BY d.e COLLATE \"C\", d.i) AS e, \
                    array_agg(d.i ORDER BY d.e COLLATE \"C\", d.i) AS i, \
                    array_agg(d.pe ORDER BY d.e COLLATE \"C\", d.i) AS pe, \
                    array_agg(d.pi ORDER BY d.e COLLATE \"C\", d.i) AS pi, \
                    array_agg(d.x ORDER BY d.e COLLATE \"C\", d.i) AS x \
             FROM (\
               SELECT DISTINCT ON (c.e COLLATE \"C\", c.i) c.e, c.i, c.pe, c.pi, c.x \
               FROM ({found} OFFSET 0) c \
               WHERE (c.e, c.i) NOT IN (SELECT v.e, v.i FROM unnest(r.vis_e, r.vis_i) v(e, i)) \
               ORDER BY c.e COLLATE \"C\", c.i, c.pe COLLATE \"C\", c.pi, c.x \
               LIMIT {max_nodes} + 1 - cardinality(r.vis_i)\
             ) d\
           ) n \
           WHERE r.depth < {max_depth} AND cardinality(r.vis_i) <= {max_nodes} AND n.i IS NOT NULL\
         ) \
         SELECT r.depth, cardinality(r.vis_i)::bigint AS total, u.e, u.i, u.pe, u.pi, u.x \
         FROM reach r, unnest(r.nw_e, r.nw_i, r.nw_pe, r.nw_pi, r.nw_x) AS u(e, i, pe, pi, x) \
         ORDER BY r.depth, u.e COLLATE \"C\", u.i"
    );
    Ok(Some((sql, params, edges)))
}
