//! A small directed graph with strongly-connected components and shortest-path search.
//!
//! Written by hand (the brief allows no new third-party crate) and kept in one place so the
//! anomaly detection and its self-tests share one implementation. Tarjan's algorithm is
//! iterative: a 100,000-transaction history must not overflow the stack.

use std::collections::VecDeque;

/// A directed graph over nodes `0..n`.
#[derive(Clone, Debug, Default)]
pub struct DiGraph {
    adj: Vec<Vec<usize>>,
}

impl DiGraph {
    /// An empty graph with `n` nodes.
    pub fn new(n: usize) -> DiGraph {
        DiGraph {
            adj: vec![Vec::new(); n],
        }
    }

    /// Number of nodes.
    pub fn len(&self) -> usize {
        self.adj.len()
    }

    /// Whether the graph has no nodes.
    pub fn is_empty(&self) -> bool {
        self.adj.is_empty()
    }

    /// Adds an edge `u -> v` (duplicates are kept; the algorithms are duplicate-safe).
    pub fn add_edge(&mut self, u: usize, v: usize) {
        if u < self.adj.len() && v < self.adj.len() {
            self.adj[u].push(v);
        }
    }

    /// The successors of `u`.
    pub fn successors(&self, u: usize) -> &[usize] {
        self.adj.get(u).map_or(&[], Vec::as_slice)
    }

    /// The strongly-connected components, by Tarjan's algorithm (iterative).
    pub fn sccs(&self) -> Scc {
        let n = self.adj.len();
        let mut index = vec![usize::MAX; n];
        let mut low = vec![0usize; n];
        let mut on_stack = vec![false; n];
        let mut stack: Vec<usize> = Vec::new();
        let mut comp = vec![usize::MAX; n];
        let mut members: Vec<Vec<usize>> = Vec::new();
        let mut next_index = 0usize;
        // (node, next successor position)
        let mut call: Vec<(usize, usize)> = Vec::new();

        for root in 0..n {
            if index[root] != usize::MAX {
                continue;
            }
            call.push((root, 0));
            index[root] = next_index;
            low[root] = next_index;
            next_index += 1;
            stack.push(root);
            on_stack[root] = true;
            while let Some(&mut (u, ref mut pos)) = call.last_mut() {
                if *pos < self.adj[u].len() {
                    let v = self.adj[u][*pos];
                    *pos += 1;
                    if index[v] == usize::MAX {
                        index[v] = next_index;
                        low[v] = next_index;
                        next_index += 1;
                        stack.push(v);
                        on_stack[v] = true;
                        call.push((v, 0));
                    } else if on_stack[v] {
                        low[u] = low[u].min(index[v]);
                    }
                } else {
                    call.pop();
                    if low[u] == index[u] {
                        let c = members.len();
                        let mut group = Vec::new();
                        while let Some(w) = stack.pop() {
                            on_stack[w] = false;
                            comp[w] = c;
                            group.push(w);
                            if w == u {
                                break;
                            }
                        }
                        members.push(group);
                    }
                    if let Some(&mut (parent, _)) = call.last_mut() {
                        low[parent] = low[parent].min(low[u]);
                    }
                }
            }
        }
        Scc { comp, members }
    }

    /// A shortest path (fewest nodes) from `from` to `to`, or `None` if `to` is unreachable.
    /// `from == to` yields the one-node path.
    pub fn shortest_path(&self, from: usize, to: usize) -> Option<Vec<usize>> {
        if from == to {
            return Some(vec![from]);
        }
        let n = self.adj.len();
        if from >= n || to >= n {
            return None;
        }
        let mut prev = vec![usize::MAX; n];
        let mut seen = vec![false; n];
        let mut queue = VecDeque::new();
        seen[from] = true;
        queue.push_back(from);
        while let Some(u) = queue.pop_front() {
            for &v in &self.adj[u] {
                if !seen[v] {
                    seen[v] = true;
                    prev[v] = u;
                    if v == to {
                        let mut path = vec![to];
                        let mut cur = to;
                        while prev[cur] != usize::MAX {
                            cur = prev[cur];
                            path.push(cur);
                        }
                        path.reverse();
                        return Some(path);
                    }
                    queue.push_back(v);
                }
            }
        }
        None
    }
}

/// The result of [`DiGraph::sccs`].
#[derive(Clone, Debug)]
pub struct Scc {
    comp: Vec<usize>,
    members: Vec<Vec<usize>>,
}

impl Scc {
    /// The component index of `v`.
    pub fn component_of(&self, v: usize) -> usize {
        self.comp[v]
    }

    /// The nodes of component `c`.
    pub fn members(&self, c: usize) -> &[usize] {
        &self.members[c]
    }

    /// Number of components.
    pub fn count(&self) -> usize {
        self.members.len()
    }

    /// The components with more than one node.
    pub fn nontrivial(&self) -> impl Iterator<Item = (usize, &[usize])> {
        self.members
            .iter()
            .enumerate()
            .filter(|(_, m)| m.len() > 1)
            .map(|(c, m)| (c, m.as_slice()))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn sccs_split_a_diamond_and_a_cycle() {
        // 0 -> 1 -> 3, 0 -> 2 -> 3; 3 <-> 4
        let mut g = DiGraph::new(5);
        g.add_edge(0, 1);
        g.add_edge(0, 2);
        g.add_edge(1, 3);
        g.add_edge(2, 3);
        g.add_edge(3, 4);
        g.add_edge(4, 3);
        let scc = g.sccs();
        assert_eq!(scc.component_of(3), scc.component_of(4));
        assert_eq!(scc.members(scc.component_of(3)).len(), 2);
        assert_ne!(scc.component_of(0), scc.component_of(3));
        let big: Vec<usize> = scc.nontrivial().map(|(_, m)| m.len()).collect();
        assert_eq!(big, vec![2]);
    }

    #[test]
    fn a_long_chain_does_not_overflow_the_stack() {
        let n = 200_000;
        let mut g = DiGraph::new(n);
        for i in 0..n - 1 {
            g.add_edge(i, i + 1);
        }
        let scc = g.sccs();
        assert_eq!(scc.count(), n);
        assert!(g.shortest_path(0, n - 1).is_some());
        assert!(g.shortest_path(n - 1, 0).is_none());
    }

    #[test]
    fn shortest_path_is_shortest() {
        let mut g = DiGraph::new(4);
        g.add_edge(0, 1);
        g.add_edge(0, 2);
        g.add_edge(1, 3);
        g.add_edge(2, 3);
        g.add_edge(3, 0);
        assert_eq!(g.shortest_path(0, 3).unwrap().len(), 3);
        assert_eq!(g.shortest_path(2, 2).unwrap(), vec![2]);
    }
}
