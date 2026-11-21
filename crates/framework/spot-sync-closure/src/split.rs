//! Dependency order and packing of extras into prerequisite bundles (ADR-064 section 4).

use crate::closure::ExtraNode;
use crate::model::RecordKey;
use std::collections::BTreeMap;

/// The strongly connected components of the extras graph, dependencies first: a component comes
/// after every component it references. A cycle is one component and stays together.
///
/// Iterative Tarjan, so a long reference chain cannot overflow the stack.
pub(crate) fn components(extras: &BTreeMap<RecordKey, ExtraNode>) -> Vec<Vec<RecordKey>> {
    let keys: Vec<&RecordKey> = extras.keys().collect();
    let index_of: BTreeMap<&RecordKey, usize> =
        keys.iter().enumerate().map(|(i, k)| (*k, i)).collect();
    let edges: Vec<Vec<usize>> = keys
        .iter()
        .map(|k| {
            extras[*k]
                .deps
                .iter()
                .filter_map(|d| index_of.get(d).copied())
                .collect()
        })
        .collect();
    let n = keys.len();
    let (mut index, mut low) = (vec![usize::MAX; n], vec![0usize; n]);
    let mut on_stack = vec![false; n];
    let (mut stack, mut next, mut result) = (Vec::new(), 0usize, Vec::new());
    for root in 0..n {
        if index[root] != usize::MAX {
            continue;
        }
        let mut calls: Vec<(usize, usize)> = vec![(root, 0)];
        index[root] = next;
        low[root] = next;
        next += 1;
        stack.push(root);
        on_stack[root] = true;
        while let Some(&mut (v, ref mut cursor)) = calls.last_mut() {
            if let Some(&w) = edges[v].get(*cursor) {
                *cursor += 1;
                if index[w] == usize::MAX {
                    index[w] = next;
                    low[w] = next;
                    next += 1;
                    stack.push(w);
                    on_stack[w] = true;
                    calls.push((w, 0));
                } else if on_stack[w] {
                    low[v] = low[v].min(index[w]);
                }
            } else {
                calls.pop();
                if let Some(&(parent, _)) = calls.last() {
                    low[parent] = low[parent].min(low[v]);
                }
                if low[v] == index[v] {
                    let mut comp = Vec::new();
                    while let Some(w) = stack.pop() {
                        on_stack[w] = false;
                        comp.push(keys[w].clone());
                        if w == v {
                            break;
                        }
                    }
                    comp.sort();
                    result.push(comp);
                }
            }
        }
    }
    result
}

/// Pack components, in order, into groups that stay under both caps. A component larger than a
/// cap on its own is one group (a cycle is never cut).
pub(crate) fn pack(
    comps: Vec<Vec<RecordKey>>,
    size_of: &dyn Fn(&RecordKey) -> usize,
    max_records: usize,
    max_bytes: usize,
) -> Vec<Vec<RecordKey>> {
    let mut groups: Vec<Vec<RecordKey>> = Vec::new();
    let (mut cur, mut cur_bytes): (Vec<RecordKey>, usize) = (Vec::new(), 0);
    for comp in comps {
        let bytes: usize = comp.iter().map(size_of).sum();
        if !cur.is_empty()
            && (cur.len() + comp.len() > max_records || cur_bytes + bytes > max_bytes)
        {
            groups.push(std::mem::take(&mut cur));
            cur_bytes = 0;
        }
        cur_bytes += bytes;
        cur.extend(comp);
    }
    if !cur.is_empty() {
        groups.push(cur);
    }
    groups
}
