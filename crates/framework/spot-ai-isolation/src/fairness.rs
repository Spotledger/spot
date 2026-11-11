//! Weighted fair queueing across agents (WP-AI-05.T1): one agent's burst cannot starve the
//! others.
//!
//! Each agent has a weight; an item of `cost` units advances that agent's virtual finish time
//! by `cost / weight`, and the queue is served in virtual-finish order. A heavier agent
//! advances more slowly per unit of work, so it is granted a larger share, while a light agent
//! that enqueues one item still finishes it after at most the items already at the same finish
//! time — never behind a whole burst. Ties break on arrival order.

use std::collections::{BTreeSet, HashMap};
use std::hash::Hash;

/// A weighted fair queue keyed by agent.
#[derive(Clone, Debug)]
pub struct WeightedFairQueue<K> {
    weights: HashMap<K, u64>,
    virtual_time: HashMap<K, u64>,
    queue: BTreeSet<(u64, u64, K)>,
    seq: u64,
}

impl<K: Ord + Clone + Hash + Eq> Default for WeightedFairQueue<K> {
    fn default() -> Self {
        Self {
            weights: HashMap::new(),
            virtual_time: HashMap::new(),
            queue: BTreeSet::new(),
            seq: 0,
        }
    }
}

impl<K: Ord + Clone + Hash + Eq> WeightedFairQueue<K> {
    /// An empty queue; every agent's weight defaults to one.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets an agent's weight; a weight of zero is treated as one.
    pub fn set_weight(&mut self, key: &K, weight: u64) {
        self.weights.insert(key.clone(), weight.max(1));
    }

    /// Adds one item of `cost` units for `key`.
    pub fn enqueue(&mut self, key: &K, cost: u64) {
        let weight = self.weights.get(key).copied().unwrap_or(1).max(1);
        let finish = self.virtual_time.get(key).copied().unwrap_or(0) + cost.div_ceil(weight);
        self.virtual_time.insert(key.clone(), finish);
        self.seq = self.seq.wrapping_add(1);
        self.queue.insert((finish, self.seq, key.clone()));
    }

    /// Removes and returns the agent whose next item has the smallest virtual finish time.
    pub fn dequeue(&mut self) -> Option<K> {
        self.queue.pop_first().map(|(_, _, key)| key)
    }

    /// How many items are queued.
    #[must_use]
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// Whether the queue is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn a_burst_cannot_starve_a_light_agent() {
        let mut queue: WeightedFairQueue<&str> = WeightedFairQueue::new();
        for _ in 0..1000 {
            queue.enqueue(&"burst", 1);
        }
        queue.enqueue(&"light", 1);
        assert_eq!(queue.len(), 1001);
        // The light agent's single item is served after at most the burst's first item.
        let mut served = Vec::new();
        for _ in 0..2 {
            served.push(queue.dequeue().expect("two items are queued"));
        }
        assert!(served.contains(&"light"), "served order was {served:?}");
    }

    #[test]
    fn a_heavier_agent_gets_a_larger_share() {
        let mut queue: WeightedFairQueue<&str> = WeightedFairQueue::new();
        queue.set_weight(&"heavy", 4);
        queue.set_weight(&"light", 1);
        queue.enqueue(&"light", 4);
        queue.enqueue(&"heavy", 4);
        // The same 4 units advance the heavy agent by one and the light agent by four, so the
        // heavy agent's item is served first.
        assert_eq!(queue.dequeue(), Some("heavy"));
        assert_eq!(queue.dequeue(), Some("light"));
    }

    #[test]
    fn ties_break_on_arrival_order() {
        let mut queue: WeightedFairQueue<&str> = WeightedFairQueue::new();
        queue.enqueue(&"first", 1);
        queue.enqueue(&"second", 1);
        assert_eq!(queue.dequeue(), Some("first"));
        assert_eq!(queue.dequeue(), Some("second"));
        assert!(queue.is_empty());
    }
}
