//! The rate limit: a sliding one-minute window, global and per node.
//!
//! A repair started at `t` is remembered until `t + 60 s`. A new repair is allowed only while
//! fewer than the limit are remembered, so in **any** one-minute window no more than the limit
//! start. (A refilling token bucket with a burst allows up to twice its rate across a refill
//! boundary; the window does not, which is what the brief's property requires.)

use std::collections::{BTreeMap, VecDeque};

use spot_conditions::Millis;

use crate::settings::RATE_WINDOW;

#[derive(Debug, Default)]
pub(crate) struct RateWindow {
    global: VecDeque<Millis>,
    per_node: BTreeMap<String, VecDeque<Millis>>,
}

impl RateWindow {
    /// Forgets starts at least one window old.
    pub(crate) fn prune(&mut self, now: Millis) {
        while self
            .global
            .front()
            .is_some_and(|t| now.elapsed_at_least(*t, RATE_WINDOW))
        {
            self.global.pop_front();
        }
        for starts in self.per_node.values_mut() {
            while starts
                .front()
                .is_some_and(|t| now.elapsed_at_least(*t, RATE_WINDOW))
            {
                starts.pop_front();
            }
        }
        self.per_node.retain(|_, starts| !starts.is_empty());
    }

    pub(crate) fn global_full(&self, limit: u32) -> bool {
        self.global.len() >= limit as usize
    }

    pub(crate) fn node_full(&self, node: &str, limit: u32) -> bool {
        self.per_node
            .get(node)
            .is_some_and(|starts| starts.len() >= limit as usize)
    }

    pub(crate) fn record(&mut self, node: &str, now: Millis) {
        self.global.push_back(now);
        self.per_node
            .entry(node.to_owned())
            .or_default()
            .push_back(now);
    }
}
