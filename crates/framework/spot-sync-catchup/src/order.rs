//! Dependency ordering of coalesced images (ADR-064 section 8, ADR-066 section 2).
//!
//! Coalescing reorders a record relative to the records it references: the latest image of a
//! record can be newer than an image that links to it. Applying the bulk in position order would
//! then place a record before a record it references, and the deferred foreign key would fail at
//! commit (ADR-053). So the planner orders the images by **dependency**, never by position:
//! targets before referrers, a strongly connected group (a cycle) kept together in one group, and
//! position only as the deterministic tie-break inside and between independent groups.

use crate::error::CatchUpError;
use crate::event::{Image, RecordId};
use std::collections::{BTreeMap, BTreeSet};

/// The catalog's view of a record's references (ADR-064 section 2).
pub trait References {
    /// The records `record`'s image references: its declared links, parent and child rows.
    ///
    /// A target the image set does not hold is ignored by the orderer: it is either already at the
    /// node or out of the node's scope, and this crate has no way to tell (and must not guess).
    ///
    /// # Errors
    /// `ReferenceUnreadable` when a link's target cannot be determined from the catalog.
    fn references(
        &self,
        record: &RecordId,
        image: &crate::event::Payload,
    ) -> Result<Vec<RecordId>, CatchUpError>;
}

/// Order images so that every record comes after the records it references.
///
/// The result is a list of groups. Every group is applied as one bundle in one destination
/// transaction (ADR-064 section 1), so records inside a group may be in any order; between groups
/// the order is the dependency order. Records that do not depend on each other and share a group
/// are ordered by position, and so are the groups themselves, which makes the output deterministic
/// for the same input.
///
/// A cycle cannot be ordered, so its members stay in one group: the whole cycle is applied
/// together, which is what makes applying it possible at all.
///
/// # Errors
/// `DuplicateImage` when the same record appears twice (coalesce first), and whatever
/// [`References`] returns.
pub fn order_by_dependency(
    images: &[Image],
    refs: &impl References,
) -> Result<Vec<Vec<RecordId>>, CatchUpError> {
    let mut index: BTreeMap<&RecordId, usize> = BTreeMap::new();
    for (position, image) in images.iter().enumerate() {
        if index.insert(&image.record, position).is_some() {
            return Err(CatchUpError::DuplicateImage {
                entity: image.record.entity.clone(),
                id: image.record.id.clone(),
            });
        }
    }

    // `dependencies[i]` is every in-set record image `i` references, without repeats and without
    // itself: a self-reference is not a dependency and must not close a cycle of one.
    let mut dependencies: Vec<Vec<usize>> = vec![Vec::new(); images.len()];
    for (position, image) in images.iter().enumerate() {
        let mut seen = BTreeSet::new();
        for target in refs.references(&image.record, &image.payload)? {
            if let Some(&target_position) = index.get(&target)
                && target_position != position
                && seen.insert(target_position)
            {
                dependencies[position].push(target_position);
            }
        }
    }

    let components = strongly_connected(&dependencies);
    let mut component_of = vec![0usize; images.len()];
    for (component, members) in components.iter().enumerate() {
        for &member in members {
            component_of[member] = component;
        }
    }

    // The condensation: an edge from the component holding a target to the component holding the
    // record that references it. It is acyclic by construction, so a level per component exists.
    let mut predecessors: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); components.len()];
    for (position, targets) in dependencies.iter().enumerate() {
        for &target in targets {
            let (target_component, referrer_component) =
                (component_of[target], component_of[position]);
            if target_component != referrer_component {
                predecessors[referrer_component].insert(target_component);
            }
        }
    }
    let mut successors: Vec<Vec<usize>> = vec![Vec::new(); components.len()];
    let mut waiting: Vec<usize> = vec![0; components.len()];
    for (component, preds) in predecessors.iter().enumerate() {
        waiting[component] = preds.len();
        for &pred in preds {
            successors[pred].push(component);
        }
    }

    let mut level = vec![0usize; components.len()];
    let mut ready: BTreeSet<usize> = waiting
        .iter()
        .enumerate()
        .filter(|(_, count)| **count == 0)
        .map(|(component, _)| component)
        .collect();
    while let Some(component) = ready.pop_first() {
        let next = level[component].saturating_add(1);
        for &successor in &successors[component] {
            if level[successor] < next {
                level[successor] = next;
            }
            waiting[successor] -= 1;
            if waiting[successor] == 0 {
                ready.insert(successor);
            }
        }
    }

    // One group per level: everything whose dependencies are all in earlier groups. Position is
    // the deterministic tie-break inside a group, and the record identity breaks a position tie so
    // two runs over the same input always give the same plan.
    let mut by_level: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (component, members) in components.iter().enumerate() {
        by_level
            .entry(level[component])
            .or_default()
            .extend(members.iter().copied());
    }
    let mut groups = Vec::with_capacity(by_level.len());
    for (_, mut members) in by_level {
        members.sort_by(|a, b| {
            images[*a]
                .position
                .cmp(&images[*b].position)
                .then_with(|| images[*a].record.cmp(&images[*b].record))
        });
        groups.push(
            members
                .into_iter()
                .map(|member| images[member].record.clone())
                .collect(),
        );
    }
    Ok(groups)
}

/// Tarjan's strongly connected components, iteratively (a deep reference chain must not overflow the
/// stack), each component's members ascending.
fn strongly_connected(edges: &[Vec<usize>]) -> Vec<Vec<usize>> {
    let count = edges.len();
    let mut index = vec![usize::MAX; count];
    let mut low = vec![0usize; count];
    let mut on_stack = vec![false; count];
    let mut stack: Vec<usize> = Vec::new();
    let mut components: Vec<Vec<usize>> = Vec::new();
    let mut next = 0usize;

    for root in 0..count {
        if index[root] != usize::MAX {
            continue;
        }
        index[root] = next;
        low[root] = next;
        next += 1;
        stack.push(root);
        on_stack[root] = true;
        let mut frames: Vec<(usize, usize)> = vec![(root, 0)];

        while let Some(&(node, child)) = frames.last() {
            if child < edges[node].len() {
                if let Some(frame) = frames.last_mut() {
                    frame.1 += 1;
                }
                let target = edges[node][child];
                if index[target] == usize::MAX {
                    index[target] = next;
                    low[target] = next;
                    next += 1;
                    stack.push(target);
                    on_stack[target] = true;
                    frames.push((target, 0));
                } else if on_stack[target] {
                    low[node] = low[node].min(index[target]);
                }
                continue;
            }
            frames.pop();
            if low[node] == index[node] {
                let mut members = Vec::new();
                while let Some(member) = stack.pop() {
                    on_stack[member] = false;
                    members.push(member);
                    if member == node {
                        break;
                    }
                }
                members.sort_unstable();
                components.push(members);
            }
            if let Some(&(parent, _)) = frames.last() {
                low[parent] = low[parent].min(low[node]);
            }
        }
    }
    components
}
