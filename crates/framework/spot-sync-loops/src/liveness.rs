//! The liveness step (hub side): `NODE_SILENT` and `LAG_HIGH` from `last_seen` and queue age.

use crate::runner::{ConditionSink, Raised, Severity, Step, StepError, StepOutcome};

/// What the hub knows about one node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeFacts {
    /// The node id.
    pub node: String,
    /// When the hub last heard from it, Unix seconds.
    pub last_seen: i64,
    /// Age of the oldest change queued for it, seconds (0 when none).
    pub queue_age: i64,
}

/// Where node facts come from (the node registry and the queues).
pub trait NodeFactSource: Send {
    /// Facts for at most `max` nodes.
    ///
    /// # Errors
    ///
    /// A reason when the registry cannot be read.
    fn nodes(&mut self, max: usize) -> Result<Vec<NodeFacts>, String>;
}

/// Thresholds, seconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Thresholds {
    /// Silent after this long without contact.
    pub silent_after: i64,
    /// Lagging when the oldest queued change is older than this.
    pub lag_after: i64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            silent_after: 3_600,
            lag_after: 900,
        }
    }
}

/// The step.
pub struct LivenessStep<S: NodeFactSource> {
    source: S,
    thresholds: Thresholds,
    silent: std::collections::BTreeSet<String>,
    lagging: std::collections::BTreeSet<String>,
}

impl<S: NodeFactSource> LivenessStep<S> {
    /// A step over `source`.
    pub fn new(source: S, thresholds: Thresholds) -> Self {
        Self {
            source,
            thresholds,
            silent: Default::default(),
            lagging: Default::default(),
        }
    }
}

impl<S: NodeFactSource> Step for LivenessStep<S> {
    fn name(&self) -> &'static str {
        "liveness"
    }

    fn interval_secs(&self) -> u64 {
        30
    }

    fn run(
        &mut self,
        now: i64,
        max_items: usize,
        sink: &mut dyn ConditionSink,
    ) -> Result<StepOutcome, StepError> {
        let nodes = self
            .source
            .nodes(max_items.saturating_add(1))
            .map_err(|reason| StepError { reason })?;
        let mut outcome = StepOutcome {
            truncated: nodes.len() > max_items,
            ..StepOutcome::default()
        };
        for n in nodes.into_iter().take(max_items) {
            outcome.examined += 1;
            let silent = now.saturating_sub(n.last_seen) > self.thresholds.silent_after;
            let lagging = n.queue_age > self.thresholds.lag_after;
            for (flag, set, id) in [
                (silent, &mut self.silent, "NODE_SILENT"),
                (lagging, &mut self.lagging, "LAG_HIGH"),
            ] {
                if flag {
                    sink.raise(&Raised {
                        condition: id,
                        subject: n.node.clone(),
                        severity: Severity::Amber,
                    });
                    set.insert(n.node.clone());
                    outcome.raised += 1;
                } else if set.remove(&n.node) {
                    sink.clear(id, &n.node);
                    outcome.cleared += 1;
                }
            }
        }
        Ok(outcome)
    }
}

impl<T: NodeFactSource + ?Sized> NodeFactSource for Box<T> {
    fn nodes(&mut self, max: usize) -> Result<Vec<NodeFacts>, String> {
        (**self).nodes(max)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    struct Src(Vec<NodeFacts>);
    impl NodeFactSource for Src {
        fn nodes(&mut self, max: usize) -> Result<Vec<NodeFacts>, String> {
            Ok(self.0.iter().take(max).cloned().collect())
        }
    }

    #[derive(Default)]
    struct Sink(Vec<String>);
    impl ConditionSink for Sink {
        fn raise(&mut self, r: &Raised) {
            self.0.push(format!("raise {} {}", r.condition, r.subject));
        }
        fn clear(&mut self, c: &'static str, s: &str) {
            self.0.push(format!("clear {c} {s}"));
        }
    }

    #[test]
    fn silence_and_lag_are_raised_and_cleared_when_the_node_returns() {
        let mut step = LivenessStep::new(
            Src(vec![NodeFacts {
                node: "n1".into(),
                last_seen: 0,
                queue_age: 2_000,
            }]),
            Thresholds::default(),
        );
        let mut sink = Sink::default();
        step.run(10_000, 10, &mut sink).unwrap();
        assert_eq!(sink.0, ["raise NODE_SILENT n1", "raise LAG_HIGH n1"]);
        step.source.0[0] = NodeFacts {
            node: "n1".into(),
            last_seen: 9_990,
            queue_age: 0,
        };
        let mut sink = Sink::default();
        let outcome = step.run(10_000, 10, &mut sink).unwrap();
        assert_eq!(sink.0, ["clear NODE_SILENT n1", "clear LAG_HIGH n1"]);
        assert_eq!(outcome.cleared, 2);
    }
}
