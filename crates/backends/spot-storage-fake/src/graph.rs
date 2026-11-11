//! Breadth-first traversal over links, on a transaction's read view.

use std::collections::{BTreeMap, BTreeSet};

use spot_storage_contract::schema::system;
use spot_storage_contract::{
    Catalog, Direction, EdgeSpec, EntityKind, ErrorKind, FieldType, LinkTarget, Name, NodeRef,
    Reached, RecordId, StorageError, StorageResult, Traversal, TraversalResult, Value,
};

use crate::store::{Record, View};

/// Deepest level a traversal may ask for.
const MAX_DEPTH: u16 = 64;

/// Where an edge type points.
enum Target {
    /// Always this entity.
    Fixed(Name),
    /// The entity named by this text field of the linking record.
    Dynamic(Name),
}

struct Edge<'t> {
    spec: &'t EdgeSpec,
    target: Target,
    /// Incoming index: target node → linking record ids. Built on first use.
    incoming: Option<BTreeMap<NodeRef, Vec<RecordId>>>,
}

fn invalid(msg: String) -> StorageError {
    StorageError::new(ErrorKind::InvalidQuery, msg)
}

fn resolve<'t>(catalog: &Catalog, spec: &'t EdgeSpec) -> StorageResult<Edge<'t>> {
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
        spec,
        target,
        incoming: None,
    })
}

impl Edge<'_> {
    /// The node a record of the edge's entity links to, if any.
    fn points_at(&self, catalog: &Catalog, id: RecordId, record: &Record) -> Option<NodeRef> {
        let Some(Value::Id(to)) = record.value(id, &self.spec.field) else {
            return None;
        };
        let entity = match &self.target {
            Target::Fixed(t) => t.clone(),
            Target::Dynamic(field) => match record.value(id, field) {
                Some(Value::Text(name)) => {
                    let name = Name::new(&name).ok()?;
                    catalog.entity(&name).ok()?;
                    name
                }
                _ => return None,
            },
        };
        Some(NodeRef { entity, id: to })
    }

    fn outgoing(&self, catalog: &Catalog, view: &View<'_>, node: &NodeRef) -> Option<NodeRef> {
        if node.entity != self.spec.entity {
            return None;
        }
        let record = view.get(&node.entity, node.id)?;
        let to = self.points_at(catalog, node.id, record)?;
        view.get(&to.entity, to.id).map(|_| to)
    }

    fn incoming(&mut self, catalog: &Catalog, view: &View<'_>, node: &NodeRef) -> Vec<NodeRef> {
        if self.incoming.is_none() {
            let mut index: BTreeMap<NodeRef, Vec<RecordId>> = BTreeMap::new();
            for (id, record) in view.rows(&self.spec.entity) {
                if let Some(to) = self.points_at(catalog, id, record) {
                    index.entry(to).or_default().push(id);
                }
            }
            self.incoming = Some(index);
        }
        let ids = self.incoming.as_ref().and_then(|index| index.get(node));
        ids.map_or_else(Vec::new, |ids| {
            ids.iter()
                .map(|id| NodeRef {
                    entity: self.spec.entity.clone(),
                    id: *id,
                })
                .collect()
        })
    }
}

fn too_large(max_nodes: u32) -> StorageError {
    StorageError::new(
        ErrorKind::TraversalTooLarge,
        format!("the traversal reaches more than {max_nodes} nodes; narrow it or raise max_nodes"),
    )
}

/// Runs `t` over `view`.
pub(crate) fn traverse(
    catalog: &Catalog,
    view: &View<'_>,
    t: &Traversal,
) -> StorageResult<TraversalResult> {
    if t.edges.is_empty() {
        return Err(invalid("a traversal needs at least one edge type".into()));
    }
    if !(1..=MAX_DEPTH).contains(&t.max_depth) {
        return Err(invalid(format!(
            "max_depth must be 1 to {MAX_DEPTH}, not {}",
            t.max_depth
        )));
    }
    let mut edges = t
        .edges
        .iter()
        .map(|spec| resolve(catalog, spec))
        .collect::<StorageResult<Vec<_>>>()?;
    let max_nodes = usize::try_from(t.max_nodes).unwrap_or(usize::MAX);
    let (outgoing, incoming) = match t.direction {
        Direction::Outgoing => (true, false),
        Direction::Incoming => (false, true),
        Direction::Both => (true, true),
    };

    let mut reached: BTreeMap<NodeRef, Reached> = BTreeMap::new();
    for s in &t.start {
        catalog.entity(&s.entity)?;
        if view.get(&s.entity, s.id).is_some() {
            reached.entry(s.clone()).or_insert_with(|| Reached {
                node: s.clone(),
                depth: 0,
                via: None,
            });
        }
    }
    if reached.len() > max_nodes {
        return Err(too_large(t.max_nodes));
    }
    let mut frontier: BTreeSet<NodeRef> = reached.keys().cloned().collect();
    for depth in 1..=t.max_depth {
        let mut next: BTreeMap<NodeRef, (NodeRef, EdgeSpec)> = BTreeMap::new();
        for node in &frontier {
            for edge in &mut edges {
                let mut found = Vec::new();
                if outgoing {
                    found.extend(edge.outgoing(catalog, view, node));
                }
                if incoming {
                    found.extend(edge.incoming(catalog, view, node));
                }
                for to in found {
                    if reached.contains_key(&to) {
                        continue;
                    }
                    let via = (node.clone(), edge.spec.clone());
                    next.entry(to)
                        .and_modify(|best| {
                            if via < *best {
                                *best = via.clone();
                            }
                        })
                        .or_insert(via);
                }
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next.keys().cloned().collect();
        for (node, via) in next {
            reached.insert(
                node.clone(),
                Reached {
                    node,
                    depth,
                    via: Some(via),
                },
            );
        }
        if reached.len() > max_nodes {
            return Err(too_large(t.max_nodes));
        }
    }
    let mut nodes: Vec<Reached> = reached.into_values().collect();
    nodes.sort_by(|a, b| (a.depth, &a.node).cmp(&(b.depth, &b.node)));
    Ok(TraversalResult { nodes })
}
