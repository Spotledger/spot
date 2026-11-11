//! Strongly connected components, targets first.

/// The strongly connected components of a graph where `edges[i]` lists the nodes `i` references.
///
/// The result is in dependency order: a component comes after every component it references, so
/// applying it in order never places a record before one it links to. A cycle is one component.
/// Members of a component are ascending, and the order of components follows the node order, so
/// the output is deterministic. Iterative (Tarjan), so a long chain cannot overflow the stack.
pub(crate) fn components(edges: &[Vec<usize>]) -> Vec<Vec<usize>> {
    const NONE: usize = usize::MAX;
    let n = edges.len();
    let mut index = vec![NONE; n];
    let mut low = vec![0_usize; n];
    let mut on_stack = vec![false; n];
    let mut stack: Vec<usize> = Vec::new();
    let mut out: Vec<Vec<usize>> = Vec::new();
    let mut next_index = 0_usize;
    // (node, next edge to look at)
    let mut work: Vec<(usize, usize)> = Vec::new();

    for root in 0..n {
        if index[root] != NONE {
            continue;
        }
        work.push((root, 0));
        while let Some(&(v, edge)) = work.last() {
            if edge == 0 && index[v] == NONE {
                index[v] = next_index;
                low[v] = next_index;
                next_index += 1;
                stack.push(v);
                on_stack[v] = true;
            }
            if let Some(&w) = edges[v].get(edge) {
                if let Some(top) = work.last_mut() {
                    top.1 += 1;
                }
                if index[w] == NONE {
                    work.push((w, 0));
                } else if on_stack[w] {
                    low[v] = low[v].min(index[w]);
                }
                continue;
            }
            work.pop();
            if let Some(&(parent, _)) = work.last() {
                low[parent] = low[parent].min(low[v]);
            }
            if low[v] == index[v] {
                let mut component = Vec::new();
                while let Some(w) = stack.pop() {
                    on_stack[w] = false;
                    component.push(w);
                    if w == v {
                        break;
                    }
                }
                component.sort_unstable();
                out.push(component);
            }
        }
    }
    out
}
