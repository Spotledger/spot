//! The storage executor: runs queries inside one transaction, over its traversal and its document
//! reads.

use std::collections::BTreeMap;
use std::sync::Arc;

use spot_graph_model::GraphModel;
use spot_storage_contract::{
    Direction as StorageDirection, EdgeSpec, Name, NodeRef, RowLock, Transaction, Traversal, Value,
};

use crate::aggregate::merge_lines;
use crate::compile::{EdgeFieldAccess, RelationStep, Step, compile_with};
use crate::error::GraphQueryError;
use crate::exec::{
    EdgeProps, GraphExecutor, Invocation, NodeData, NodeSource, caller_record, walk,
};
use crate::query::GraphQuery;
use crate::result::GraphResult;

/// The fields that hold a record's lifecycle state and creator, when the entity has them. The
/// storage contract does not reserve these names, so the caller says which fields carry them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FactFields {
    /// The text field holding the lifecycle state, such as `draft`, `posted` or `cancelled`.
    pub state: Option<Name>,
    /// The text field holding the creator's user name.
    pub created_by: Option<Name>,
}

/// Runs graph queries in one transaction. Give the transaction back with
/// [`StorageExecutor::into_transaction`] to commit or roll it back.
pub struct StorageExecutor<T> {
    tx: T,
    model: Arc<GraphModel>,
    facts: FactFields,
    edge_access: Arc<dyn EdgeFieldAccess>,
}

impl<T: Transaction> StorageExecutor<T> {
    /// An executor over `tx`, following the edge types of `model`, under the caller's
    /// field-level access to edge properties. The policy is required: there is no allow-all
    /// default. Bind it to the caller's principal and rules.
    pub fn new(
        tx: T,
        model: Arc<GraphModel>,
        facts: FactFields,
        edge_access: Arc<dyn EdgeFieldAccess>,
    ) -> Self {
        Self {
            tx,
            model,
            facts,
            edge_access,
        }
    }

    /// The transaction back, for commit or rollback.
    pub fn into_transaction(self) -> T {
        self.tx
    }
}

/// The storage side of the walk.
struct StorageSource<'a, T> {
    tx: &'a mut T,
    facts: &'a FactFields,
    who: Invocation<'a>,
    /// The properties of the relation edges the walk has crossed, by (edge type, `from` end,
    /// `to` end), read together with the records that make the edge.
    relation_props: BTreeMap<(String, NodeRef, NodeRef), EdgeProps>,
}

fn text_of(data: &std::collections::BTreeMap<Name, Value>, field: &Option<Name>) -> Option<String> {
    match field.as_ref().and_then(|f| data.get(f)) {
        Some(Value::Text(text)) => Some(text.clone()),
        _ => None,
    }
}

impl<T: Transaction> NodeSource for StorageSource<'_, T> {
    async fn load(&mut self, node: &NodeRef) -> Result<Option<NodeData>, GraphQueryError> {
        let Some(document) = self.tx.load(&node.entity, node.id, RowLock::None).await? else {
            return Ok(None);
        };
        let values = document.data.fields;
        Ok(Some(NodeData {
            state: text_of(&values, &self.facts.state),
            created_by: text_of(&values, &self.facts.created_by),
            values,
        }))
    }

    async fn step(
        &mut self,
        node: &NodeRef,
        step: &Step,
        max_nodes: u32,
    ) -> Result<Vec<NodeRef>, GraphQueryError> {
        if let Some(relation) = &step.relation {
            return self.relation_step(node, step, relation, max_nodes).await;
        }
        let result = self
            .tx
            .traverse(&Traversal {
                start: vec![node.clone()],
                edges: vec![step.spec.clone()],
                direction: step.direction,
                max_depth: 1,
                max_nodes,
            })
            .await?;
        Ok(result
            .nodes
            .into_iter()
            .filter(|reached| reached.depth == 1)
            .map(|reached| reached.node)
            .collect())
    }

    async fn edge_properties(
        &mut self,
        from: &NodeRef,
        to: &NodeRef,
        step: &Step,
    ) -> Result<Option<EdgeProps>, GraphQueryError> {
        if step.relation.is_none() {
            return Ok(None);
        }
        // The edge runs from the `from` end to the `to` end whatever way the walk crossed it.
        let key = |a: &NodeRef, b: &NodeRef| (step.name.clone(), a.clone(), b.clone());
        let found = match step.direction {
            StorageDirection::Outgoing => self.relation_props.get(&key(from, to)),
            StorageDirection::Incoming => self.relation_props.get(&key(to, from)),
            StorageDirection::Both => self
                .relation_props
                .get(&key(from, to))
                .or_else(|| self.relation_props.get(&key(to, from))),
        };
        Ok(found.cloned())
    }
}

impl<T: Transaction> StorageSource<'_, T> {
    /// One step along a relation edge: the relation records that name `node` at one end, read
    /// through the traversal (bounded by `max_nodes`, so too many is `TraversalTooLarge`) and
    /// the caller's read rights on the relation DocType. A record the caller may not read, or
    /// whose link fields the caller may not read, is no edge.
    async fn relation_step(
        &mut self,
        node: &NodeRef,
        step: &Step,
        relation: &RelationStep,
        max_nodes: u32,
    ) -> Result<Vec<NodeRef>, GraphQueryError> {
        let mut reached: Vec<NodeRef> = Vec::new();
        let outgoing = matches!(
            step.direction,
            StorageDirection::Outgoing | StorageDirection::Both
        );
        let incoming = matches!(
            step.direction,
            StorageDirection::Incoming | StorageDirection::Both
        );
        // `near` is the field holding `node`; `far` the field holding the record reached.
        if outgoing && node.entity == relation.from_entity {
            self.relation_side(
                node,
                step,
                relation,
                (&relation.from_field, &relation.to_field),
                &relation.to_entity,
                true,
                max_nodes,
                &mut reached,
            )
            .await?;
        }
        if incoming && node.entity == relation.to_entity {
            self.relation_side(
                node,
                step,
                relation,
                (&relation.to_field, &relation.from_field),
                &relation.from_entity,
                false,
                max_nodes,
                &mut reached,
            )
            .await?;
        }
        reached.sort();
        reached.dedup();
        reached.retain(|n| n != node);
        Ok(reached)
    }

    #[allow(clippy::too_many_arguments)]
    async fn relation_side(
        &mut self,
        node: &NodeRef,
        step: &Step,
        relation: &RelationStep,
        (near, far): (&Name, &Name),
        far_entity: &Name,
        node_is_from: bool,
        max_nodes: u32,
        reached: &mut Vec<NodeRef>,
    ) -> Result<(), GraphQueryError> {
        let records = self
            .tx
            .traverse(&Traversal {
                start: vec![node.clone()],
                edges: vec![EdgeSpec {
                    entity: relation.doctype.clone(),
                    field: near.clone(),
                }],
                direction: StorageDirection::Incoming,
                max_depth: 1,
                max_nodes,
            })
            .await?;
        // The readable lines of each pair on this side; merged below (or refused as ambiguous).
        let mut lines: BTreeMap<NodeRef, Vec<EdgeProps>> = BTreeMap::new();
        for record in records.nodes.into_iter().filter(|r| r.depth == 1) {
            let record = record.node;
            let Some(document) = self
                .tx
                .load(&record.entity, record.id, RowLock::None)
                .await?
            else {
                continue;
            };
            let data = NodeData {
                state: text_of(&document.data.fields, &self.facts.state),
                created_by: text_of(&document.data.fields, &self.facts.created_by),
                values: document.data.fields,
            };
            let Some(visible) = caller_record(&record, &data, &self.who) else {
                continue;
            };
            let (Some(Value::Id(near_id)), Some(Value::Id(far_id))) =
                (visible.get(near), visible.get(far))
            else {
                continue;
            };
            if *near_id != node.id {
                continue;
            }
            let far_node = NodeRef {
                entity: far_entity.clone(),
                id: *far_id,
            };
            let props: EdgeProps = step
                .properties
                .iter()
                .filter_map(|name| visible.get(name).map(|v| (name.clone(), v.clone())))
                .collect();
            lines.entry(far_node).or_default().push(props);
        }
        for (far_node, pair_lines) in lines {
            let props = merge_lines(&step.name, relation.aggregate_sum.as_ref(), &pair_lines)?;
            let key = if node_is_from {
                (step.name.clone(), node.clone(), far_node.clone())
            } else {
                (step.name.clone(), far_node.clone(), node.clone())
            };
            self.relation_props.insert(key, props);
            reached.push(far_node);
        }
        Ok(())
    }
}

impl<T: Transaction> GraphExecutor for StorageExecutor<T> {
    async fn execute(
        &mut self,
        query: &GraphQuery,
        who: &Invocation<'_>,
    ) -> Result<GraphResult, GraphQueryError> {
        let steps = compile_with(&self.model, query, self.edge_access.as_ref())?;
        let mut source = StorageSource {
            tx: &mut self.tx,
            facts: &self.facts,
            who: *who,
            relation_props: BTreeMap::new(),
        };
        walk(&mut source, &steps, query, who).await
    }
}
