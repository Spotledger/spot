//! The walk: a permission- and filter-aware breadth-first search over a [`NodeSource`], and the
//! [`GraphExecutor`] seam the storage executor and the in-memory index both implement.
//!
//! The walk is a BFS over the **visible subgraph**: nodes the caller may read and that pass the
//! node filter, joined by edges that pass the edge filter. Depths, the smallest predecessor and
//! the edges are those of that subgraph, so a path through a hidden node never shows up, not even
//! as a longer depth. The walk asks the source for one step at a time, which is how a hidden
//! node's own edges stay unseen: they are never requested for it.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;

use spot_permissions::{
    Action, DocFacts, FieldFacts, FieldLevel, PermissionEngine, Principal, mask,
};
use spot_storage_contract::{ErrorKind, Name, NodeRef, StorageError, Value};

use crate::compile::Step;
use crate::error::GraphQueryError;
use crate::filter;
use crate::query::{GraphQuery, Order};
use crate::result::{GraphEdge, GraphNode, GraphResult, PathHop};

/// Who asks, and the rules that answer.
#[derive(Clone, Copy)]
pub struct Invocation<'a> {
    /// The acting principal.
    pub principal: &'a Principal,
    /// The permission engine over the current snapshot.
    pub engine: &'a PermissionEngine,
}

/// The values of one record, as a source reports them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NodeData {
    /// The stored values by field name.
    pub values: BTreeMap<Name, Value>,
    /// The lifecycle state, when the record has one.
    pub state: Option<String>,
    /// Who created the record, by user name.
    pub created_by: Option<String>,
}

/// Where the walk reads from: a transaction's storage, or an index.
pub trait NodeSource: Send {
    /// The record's values, or `None` when it does not exist.
    fn load(
        &mut self,
        node: &NodeRef,
    ) -> impl Future<Output = Result<Option<NodeData>, GraphQueryError>> + Send;

    /// The records one step away from `node`, excluding `node` itself. Storage's own
    /// `TraversalTooLarge` passes through.
    fn step(
        &mut self,
        node: &NodeRef,
        step: &Step,
        max_nodes: u32,
    ) -> impl Future<Output = Result<Vec<NodeRef>, GraphQueryError>> + Send;

    /// The properties of the edge the traversal crossed from `from` to `to` along `step`, or
    /// `None` when the source holds no properties for it. The source resolves the edge's
    /// orientation from `step.direction`. The default holds none: storage's link edges have no
    /// properties; an index that stores them overrides this.
    fn edge_properties(
        &mut self,
        _from: &NodeRef,
        _to: &NodeRef,
        _step: &Step,
    ) -> impl Future<Output = Result<Option<EdgeProps>, GraphQueryError>> + Send {
        async { Ok(None) }
    }
}

/// The properties of one edge, by name.
pub type EdgeProps = BTreeMap<Name, Value>;

/// The seam: runs a validated query for one invocation. The storage executor implements it over
/// the transaction's traversal; the in-memory index implements the same contract later.
pub trait GraphExecutor: Send {
    /// Answers `query` for `who`.
    fn execute(
        &mut self,
        query: &GraphQuery,
        who: &Invocation<'_>,
    ) -> impl Future<Output = Result<GraphResult, GraphQueryError>> + Send;
}

/// The record as the caller may read it: `None` when the caller may not read the record at all,
/// otherwise its fields cut to what the caller's field rules allow (hidden fields absent, masked
/// ones masked). Used for relation records, which are the storage of a relation edge.
pub(crate) fn caller_record(
    node: &NodeRef,
    data: &NodeData,
    who: &Invocation<'_>,
) -> Option<BTreeMap<Name, Value>> {
    let facts = facts_of(node, data);
    if !who.engine.can(who.principal, Action::Read, &facts).allowed {
        return None;
    }
    Some(CallerView::new(node, data, *who).values())
}

/// The verdict on one record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
    Visible,
    Denied,
    Filtered,
}

/// Runs the walk over `source` with the compiled `steps`.
///
/// # Errors
///
/// Whatever the source fails with, and [`ErrorKind::TraversalTooLarge`] when the nodes the source
/// returned exceed `max_nodes`.
pub async fn walk<S: NodeSource>(
    source: &mut S,
    steps: &[Step],
    query: &GraphQuery,
    who: &Invocation<'_>,
) -> Result<GraphResult, GraphQueryError> {
    let mut seen = Seen::default();
    let needs = Needs::of(query);
    let mut returned: BTreeSet<NodeRef> = BTreeSet::new();
    let mut truncated = false;

    // Start nodes: a missing or hidden one contributes nothing and leaves no trace.
    let mut starts: Vec<NodeRef> = query.from.clone();
    starts.sort();
    starts.dedup();
    let mut level: Vec<NodeRef> = Vec::new();
    for start in starts {
        returned.insert(start.clone());
        if let Some((data, Verdict::Visible)) = seen.judge(source, &start, query, who).await? {
            seen.accept(start.clone(), 0, None, data);
            level.push(start);
        }
    }
    ensure_budget(&returned, query.max_nodes)?;

    for depth in 1..=query.max_depth {
        if level.is_empty() {
            break;
        }
        // Candidates in node order; each keeps every (predecessor, step) pair that reached it.
        let mut candidates: BTreeMap<NodeRef, Vec<(NodeRef, usize)>> = BTreeMap::new();
        for predecessor in &level {
            for (index, step) in steps.iter().enumerate() {
                for reached in source.step(predecessor, step, query.max_nodes).await? {
                    returned.insert(reached.clone());
                    ensure_budget(&returned, query.max_nodes)?;
                    if reached != *predecessor && !seen.depth.contains_key(&reached) {
                        candidates
                            .entry(reached)
                            .or_default()
                            .push((predecessor.clone(), index));
                    }
                }
            }
        }

        let mut next: Vec<NodeRef> = Vec::new();
        for (node, pairs) in candidates {
            // The permission check comes first: a denied node sets the flag whatever the edge
            // filter says, and the edge filter never sees a record the caller may not read.
            let Some((node_data, verdict)) = seen.judge(source, &node, query, who).await? else {
                continue;
            };
            match verdict {
                Verdict::Denied => {
                    truncated = true;
                    continue;
                }
                Verdict::Filtered => continue,
                Verdict::Visible => {}
            }
            // The first pair whose edge passes the edge filter is the smallest predecessor. Every
            // predecessor is visible, and so is the node, so both views are the caller's own.
            let mut via = None;
            for (predecessor, index) in &pairs {
                let Some(pred_data) = seen.load(source, predecessor).await? else {
                    continue;
                };
                let step = &steps[*index];
                let props = if needs.edge_properties {
                    edge_props_of(source, predecessor, &node, step, &needs).await?
                } else {
                    None
                };
                if edge_passes(
                    query,
                    who,
                    step,
                    (predecessor, &pred_data),
                    (&node, &node_data),
                    props.as_ref(),
                ) {
                    let rolled = if query.rollup_sum {
                        None
                    } else {
                        rollup_of(query, step, seen.rolled.get(predecessor), &props)?
                    };
                    via = Some((predecessor.clone(), *index, props, rolled));
                    break;
                }
            }
            if let Some((predecessor, index, props, rolled)) = via {
                seen.edge_props
                    .insert(node.clone(), props.unwrap_or_default());
                if let Some(rolled) = rolled {
                    seen.rolled.insert(node.clone(), rolled);
                }
                seen.accept(node.clone(), depth, Some((predecessor, index)), node_data);
                next.push(node);
            }
        }
        level = next;
    }

    if query.rollup_sum {
        crate::paths::sum_over_paths(source, steps, query, who, &mut seen, &needs).await?;
    }

    Ok(assemble(seen, steps, query, who, truncated))
}

fn ensure_budget(returned: &BTreeSet<NodeRef>, max_nodes: u32) -> Result<(), GraphQueryError> {
    if returned.len() > max_nodes as usize {
        return Err(GraphQueryError::Storage(StorageError::new(
            ErrorKind::TraversalTooLarge,
            format!(
                "the traversal reaches more than {max_nodes} nodes; narrow it or raise max_nodes"
            ),
        )));
    }
    Ok(())
}

/// Whether the edge passes the edge filter, evaluated on the record holding the link, over the
/// caller's view of it. Both records are visible when this runs.
pub(crate) fn edge_passes(
    query: &GraphQuery,
    who: &Invocation<'_>,
    step: &Step,
    (predecessor, pred_data): (&NodeRef, &NodeData),
    (node, node_data): (&NodeRef, &NodeData),
    props: Option<&EdgeProps>,
) -> bool {
    let Some(expr) = &query.edge_filter else {
        return true;
    };
    let pred_view = CallerView::new(predecessor, pred_data, *who);
    let node_view = CallerView::new(node, node_data, *who);
    // The record holding the link: the predecessor for an outgoing step, the reached node for an
    // incoming one. A `both` step holds the link on the predecessor when the caller can see that
    // it names the node.
    let predecessor_holds = match step.direction {
        spot_storage_contract::Direction::Outgoing => true,
        spot_storage_contract::Direction::Incoming => false,
        spot_storage_contract::Direction::Both => {
            predecessor.entity == step.spec.entity
                && pred_view.value(&step.spec.field) == Some(Value::Id(node.id))
        }
    };
    let holder = if predecessor_holds {
        pred_view
    } else {
        node_view
    };
    match (&query.edge_var, props) {
        (Some(var), Some(props)) => filter::keeps_on_edge(expr, &holder.values(), var, props),
        (Some(var), None) => filter::keeps_on_edge(expr, &holder.values(), var, &EdgeProps::new()),
        (None, _) => filter::keeps(expr, &holder.values()),
    }
}

/// What the walk must fetch from the source for each crossed edge.
pub(crate) struct Needs {
    /// Whether any edge properties are read at all.
    pub(crate) edge_properties: bool,
    /// The properties the filter and the rollup name; each must exist on an edge whose type
    /// declares none.
    named: Vec<Name>,
}

impl Needs {
    fn of(query: &GraphQuery) -> Self {
        let mut named: Vec<Name> = match (&query.edge_var, &query.edge_filter) {
            (Some(var), Some(expr)) => filter::edge_refs(expr, var),
            _ => Vec::new(),
        };
        if let Some(rollup) = &query.rollup
            && !named.contains(rollup)
        {
            named.push(rollup.clone());
        }
        Self {
            edge_properties: query.include.edge_properties || !named.is_empty(),
            named,
        }
    }
}

/// The properties of the edge `from` to `to`, checked: every value is of a supported type, and,
/// when the edge type declares no properties, every property the query names exists.
pub(crate) async fn edge_props_of<S: NodeSource>(
    source: &mut S,
    from: &NodeRef,
    to: &NodeRef,
    step: &Step,
    needs: &Needs,
) -> Result<Option<EdgeProps>, GraphQueryError> {
    let mut props = source.edge_properties(from, to, step).await?;
    // A hidden property is dropped before anything looks at it, even its value type, so it can
    // neither be filtered on, rolled up, returned nor make the query fail.
    if let Some(props) = props.as_mut() {
        props.retain(|name, _| !step.hidden.contains(name));
    }
    if let Some(props) = &props {
        for (name, value) in props {
            if !supported(value) {
                return Err(GraphQueryError::UnsupportedEdgeValue {
                    edge: step.name.clone(),
                    property: name.as_str().to_owned(),
                });
            }
        }
    }
    if step.properties.is_empty() && step.hidden.is_empty() {
        for name in &needs.named {
            if !props.as_ref().is_some_and(|p| p.contains_key(name)) {
                return Err(GraphQueryError::UnknownEdgeProperty {
                    edge: step.name.clone(),
                    property: name.as_str().to_owned(),
                });
            }
        }
    }
    Ok(props)
}

/// The value types an edge property may have in a query: no floats exist among them.
fn supported(value: &Value) -> bool {
    matches!(
        value,
        Value::Null
            | Value::Bool(_)
            | Value::Int(_)
            | Value::Decimal(_)
            | Value::Text(_)
            | Value::Date(_)
            | Value::Id(_)
    )
}

/// The rolled-up product after crossing `step`: the predecessor's product times this edge's
/// property. `None` when the query asks for no rollup.
fn rollup_of(
    query: &GraphQuery,
    step: &Step,
    before: Option<&Value>,
    props: &Option<EdgeProps>,
) -> Result<Option<Value>, GraphQueryError> {
    let Some(property) = &query.rollup else {
        return Ok(None);
    };
    let factor = props
        .as_ref()
        .and_then(|p| p.get(property))
        .unwrap_or(&Value::Null);
    rollup_step(property, step, before, factor).map(Some)
}

/// `before` (the running product or sum, `None` at the start) times `factor`, exactly.
pub(crate) fn rollup_step(
    property: &Name,
    step: &Step,
    before: Option<&Value>,
    factor: &Value,
) -> Result<Value, GraphQueryError> {
    let not_numeric = || GraphQueryError::RollupNotNumeric {
        edge: step.name.clone(),
        property: property.as_str().to_owned(),
    };
    let overflow = || GraphQueryError::RollupOverflow {
        edge: step.name.clone(),
        property: property.as_str().to_owned(),
    };
    match (before, factor) {
        (None, Value::Int(_) | Value::Decimal(_)) => Ok(factor.clone()),
        (Some(Value::Int(a)), Value::Int(b)) => {
            Ok(Value::Int(a.checked_mul(*b).ok_or_else(overflow)?))
        }
        (Some(before), factor @ (Value::Int(_) | Value::Decimal(_)))
            if matches!(before, Value::Int(_) | Value::Decimal(_)) =>
        {
            exact_product(before, factor).ok_or_else(overflow)
        }
        _ => Err(not_numeric()),
    }
}

/// The exact sum of two numbers: integers with checked addition; a decimal sum keeps the larger
/// scale and must fit the decimal's 96 bits. `None` when it does not fit or would be rounded.
pub(crate) fn exact_sum(a: &Value, b: &Value) -> Option<Value> {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x.checked_add(*y).map(Value::Int),
        (Value::Decimal(x), Value::Decimal(y)) => {
            let scale = x.scale().max(y.scale());
            let sum = x.checked_add(*y)?;
            (sum.scale() == scale).then_some(Value::Decimal(sum))
        }
        (Value::Decimal(x), Value::Int(i)) | (Value::Int(i), Value::Decimal(x)) => {
            let sum = x.checked_add(From::from(*i))?;
            (sum.scale() == x.scale()).then_some(Value::Decimal(sum))
        }
        _ => None,
    }
}

/// The exact product of two numbers of which at least one is a decimal. The scale of the product
/// is the sum of the scales (an integer has scale 0) and must be at most 28; the product must fit
/// the decimal's 96 bits. `Decimal`'s own multiplication would round a product that does not fit;
/// this returns `None` for it instead.
fn exact_product(a: &Value, b: &Value) -> Option<Value> {
    match (a, b) {
        (Value::Decimal(x), Value::Decimal(y)) => {
            let scale = x.scale().checked_add(y.scale())?;
            if scale > 28 {
                return None;
            }
            let product = x.checked_mul(*y)?;
            (product.scale() == scale).then_some(Value::Decimal(product))
        }
        (Value::Decimal(x), Value::Int(i)) | (Value::Int(i), Value::Decimal(x)) => {
            let product = x.checked_mul(From::from(*i))?;
            (product.scale() == x.scale()).then_some(Value::Decimal(product))
        }
        _ => None,
    }
}

/// One record as its caller may read it: hidden fields are absent, masked fields carry their
/// masked text, and readable fields carry their stored value. Filters are evaluated over this
/// view only, so a filter never reads a value the caller cannot see.
struct CallerView<'a> {
    node: &'a NodeRef,
    data: &'a NodeData,
    who: Invocation<'a>,
    facts: DocFacts,
}

impl<'a> CallerView<'a> {
    fn new(node: &'a NodeRef, data: &'a NodeData, who: Invocation<'a>) -> Self {
        Self {
            node,
            data,
            who,
            facts: facts_of(node, data),
        }
    }

    /// The field's value as the caller may read it: `None` when hidden, the masked text when
    /// masked, otherwise the stored value (null when the record has none).
    fn value(&self, name: &Name) -> Option<Value> {
        let doctype = self.node.entity.as_str();
        let snapshot = self.who.engine.snapshot();
        let field_facts = snapshot
            .facts(doctype)
            .as_deref()
            .and_then(|f| f.field(name.as_str()))
            .cloned()
            .unwrap_or_else(|| FieldFacts::plain(name.as_str()));
        let access =
            self.who
                .engine
                .field_access(self.who.principal, doctype, &field_facts, &self.facts);
        let stored = self.data.values.get(name).cloned().unwrap_or(Value::Null);
        match access.level {
            FieldLevel::Hidden => None,
            FieldLevel::Masked(format) => Some(match stored {
                Value::Text(text) => Value::Text(mask(&text, &format)),
                _ => Value::Null,
            }),
            FieldLevel::Read | FieldLevel::ReadReveal => Some(stored),
        }
    }

    /// Every stored field the caller may read, in its view.
    fn values(&self) -> BTreeMap<Name, Value> {
        self.data
            .values
            .keys()
            .filter_map(|name| self.value(name).map(|value| (name.clone(), value)))
            .collect()
    }
}

/// The walk's record of what it has accepted, and a cache of what it has loaded and judged.
#[derive(Default)]
pub(crate) struct Seen {
    pub(crate) depth: BTreeMap<NodeRef, u16>,
    via: BTreeMap<NodeRef, Option<(NodeRef, usize)>>,
    pub(crate) data: BTreeMap<NodeRef, NodeData>,
    loaded: BTreeMap<NodeRef, Option<NodeData>>,
    judged: BTreeMap<NodeRef, Verdict>,
    /// The properties of the edge each accepted node was reached by.
    edge_props: BTreeMap<NodeRef, EdgeProps>,
    /// The rolled-up product at each accepted node (absent at the start nodes).
    pub(crate) rolled: BTreeMap<NodeRef, Value>,
    /// With a summed rollup: how many simple paths contributed at each accepted node.
    pub(crate) counts: BTreeMap<NodeRef, u128>,
}

impl Seen {
    fn accept(&mut self, node: NodeRef, depth: u16, via: Option<(NodeRef, usize)>, data: NodeData) {
        self.depth.insert(node.clone(), depth);
        self.via.insert(node.clone(), via);
        self.data.insert(node, data);
    }

    async fn load<S: NodeSource>(
        &mut self,
        source: &mut S,
        node: &NodeRef,
    ) -> Result<Option<NodeData>, GraphQueryError> {
        if let Some(cached) = self.loaded.get(node) {
            return Ok(cached.clone());
        }
        let loaded = source.load(node).await?;
        self.loaded.insert(node.clone(), loaded.clone());
        Ok(loaded)
    }

    /// The node's data and verdict, or `None` when the record is missing.
    async fn judge<S: NodeSource>(
        &mut self,
        source: &mut S,
        node: &NodeRef,
        query: &GraphQuery,
        who: &Invocation<'_>,
    ) -> Result<Option<(NodeData, Verdict)>, GraphQueryError> {
        let Some(data) = self.load(source, node).await? else {
            return Ok(None);
        };
        let verdict = match self.judged.get(node) {
            Some(verdict) => *verdict,
            None => {
                let verdict = verdict_of(node, &data, query, who);
                self.judged.insert(node.clone(), verdict);
                verdict
            }
        };
        Ok(Some((data, verdict)))
    }
}

fn facts_of(node: &NodeRef, data: &NodeData) -> DocFacts {
    DocFacts {
        doctype: node.entity.as_str().to_owned(),
        id: Some(node.id.to_string()),
        created_by: data.created_by.clone(),
        state: data.state.clone(),
        values: data
            .values
            .iter()
            .map(|(name, value)| (name.as_str().to_owned(), value.clone()))
            .collect(),
    }
}

fn verdict_of(
    node: &NodeRef,
    data: &NodeData,
    query: &GraphQuery,
    who: &Invocation<'_>,
) -> Verdict {
    let facts = facts_of(node, data);
    if !who.engine.can(who.principal, Action::Read, &facts).allowed {
        return Verdict::Denied;
    }
    match &query.node_filter {
        Some(expr) if !filter::keeps(expr, &CallerView::new(node, data, *who).values()) => {
            Verdict::Filtered
        }
        _ => Verdict::Visible,
    }
}

/// Builds the result from the accepted nodes: fields cut to what the caller may read, the
/// requested order, and the edges that reached each node.
fn assemble(
    seen: Seen,
    steps: &[Step],
    query: &GraphQuery,
    who: &Invocation<'_>,
    truncated: bool,
) -> GraphResult {
    let mut nodes: Vec<GraphNode> = seen
        .depth
        .iter()
        .map(|(node, depth)| {
            let data = seen.data.get(node).cloned().unwrap_or_default();
            GraphNode {
                node: node.clone(),
                depth: *depth,
                via: seen
                    .via
                    .get(node)
                    .cloned()
                    .flatten()
                    .map(|(pred, index)| (pred, steps[index].name.clone())),
                fields: project(node, &data, query, who),
                path: path_of(node, &seen, steps, query),
                rolled_up: seen.rolled.get(node).cloned(),
                path_count: seen
                    .counts
                    .get(node)
                    .map_or(0, |count| u32::try_from(*count).unwrap_or(u32::MAX)),
                path_count_saturated: seen
                    .counts
                    .get(node)
                    .is_some_and(|count| *count > u128::from(u32::MAX)),
            }
        })
        .collect();
    match query.order {
        Order::Breadth => nodes.sort_by(|a, b| (a.depth, &a.node).cmp(&(b.depth, &b.node))),
        Order::ByEntityThenId => nodes.sort_by(|a, b| a.node.cmp(&b.node)),
    }
    let edges: Vec<GraphEdge> = nodes
        .iter()
        .filter_map(|n| {
            n.via.as_ref().map(|(from, edge)| GraphEdge {
                edge: edge.clone(),
                from: from.clone(),
                to: n.node.clone(),
            })
        })
        .collect();
    GraphResult {
        nodes,
        edges,
        truncated_by_permission: truncated,
        max_paths_clamped: query.max_paths_clamped,
    }
}

/// The requested fields the caller may read. A masked field carries its masked text; a masked
/// non-text value carries null.
fn project(
    node: &NodeRef,
    data: &NodeData,
    query: &GraphQuery,
    who: &Invocation<'_>,
) -> BTreeMap<Name, Value> {
    let view = CallerView::new(node, data, *who);
    query
        .include
        .fields
        .iter()
        .filter_map(|field| view.value(field).map(|value| (field.clone(), value)))
        .collect()
}

/// The hops from a start node to `node`, shallowest first, when the query asked for edge
/// properties or a rollup. Every node on the way is visible, as is every edge.
fn path_of(node: &NodeRef, seen: &Seen, steps: &[Step], query: &GraphQuery) -> Vec<PathHop> {
    if !query.include.edge_properties && query.rollup.is_none() {
        return Vec::new();
    }
    let mut hops: Vec<PathHop> = Vec::new();
    let mut at = node.clone();
    while let Some(Some((pred, index))) = seen.via.get(&at) {
        hops.push(PathHop {
            node: at.clone(),
            edge_type: steps[*index].name.clone(),
            properties: if query.include.edge_properties {
                seen.edge_props.get(&at).cloned().unwrap_or_default()
            } else {
                EdgeProps::new()
            },
        });
        at = pred.clone();
    }
    hops.reverse();
    hops
}
