//! The freshness and stamps step (c): how long since the hub last confirmed each freshness class,
//! `FRESHNESS_STALE` amber then red, and the stamp that marks documents posted while a class is
//! stale (ADR-065 section 4; WP-RTB-02.T11).
//!
//! The tracker itself (`spot-sync-freshness`) keeps the age; this step reads it through
//! [`FreshnessSource`], raises the condition and tells the [`StampSink`] when a class turns stale
//! or fresh. The stamp flip is the step's one automatic response and is audited.

use std::collections::BTreeSet;

use crate::audit::{Decision, SharedDecisions};
use crate::runner::{ConditionSink, Raised, Severity, Step, StepError, StepOutcome};

/// The condition id.
pub const FRESHNESS_STALE: &str = "FRESHNESS_STALE";

/// What the tracker knows about one class.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClassFacts {
    /// The class name (an identity).
    pub class: String,
    /// Seconds since the hub last confirmed it covers the node's prefix for this class.
    pub age_secs: i64,
}

/// Where class ages come from.
pub trait FreshnessSource: Send {
    /// Facts for at most `max` classes.
    ///
    /// # Errors
    /// A reason when the tracker cannot be read.
    fn classes(&mut self, max: usize) -> Result<Vec<ClassFacts>, String>;
}

/// Where the stale stamp is set: documents posted while a class is stale carry it.
pub trait StampSink: Send {
    /// Marks `class` stale (`true`) or fresh (`false`).
    fn set_stale(&mut self, class: &str, stale: bool);
}

/// Windows, seconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Windows {
    /// Amber, and the stamp, after this long without confirmation.
    pub amber_after: i64,
    /// Red after this long.
    pub red_after: i64,
}

impl Default for Windows {
    fn default() -> Self {
        Self {
            amber_after: 900,
            red_after: 24 * 3_600,
        }
    }
}

/// The step.
pub struct FreshnessStep<S: FreshnessSource, T: StampSink> {
    source: S,
    stamps: T,
    windows: Windows,
    decisions: SharedDecisions,
    stale: BTreeSet<String>,
}

impl<S: FreshnessSource, T: StampSink> FreshnessStep<S, T> {
    /// A step over `source` and `stamps`.
    pub fn new(source: S, stamps: T, windows: Windows, decisions: SharedDecisions) -> Self {
        Self {
            source,
            stamps,
            windows,
            decisions,
            stale: BTreeSet::new(),
        }
    }
}

impl<S: FreshnessSource, T: StampSink> Step for FreshnessStep<S, T> {
    fn name(&self) -> &'static str {
        "freshness"
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
        let classes = self
            .source
            .classes(max_items.saturating_add(1))
            .map_err(|reason| StepError { reason })?;
        let mut outcome = StepOutcome {
            truncated: classes.len() > max_items,
            ..StepOutcome::default()
        };
        for facts in classes.into_iter().take(max_items) {
            outcome.examined += 1;
            let stale = facts.age_secs > self.windows.amber_after;
            if stale {
                let severity = if facts.age_secs > self.windows.red_after {
                    Severity::Red
                } else {
                    Severity::Amber
                };
                sink.raise(&Raised {
                    condition: FRESHNESS_STALE,
                    subject: facts.class.clone(),
                    severity,
                });
                outcome.raised += 1;
            } else {
                sink.clear(FRESHNESS_STALE, &facts.class);
                outcome.cleared += 1;
            }
            let was = self.stale.contains(&facts.class);
            if stale != was {
                self.stamps.set_stale(&facts.class, stale);
                self.decisions.record(&Decision {
                    step: "freshness",
                    action: if stale { "stamp_stale" } else { "stamp_fresh" },
                    subject: facts.class.clone(),
                    at: now,
                    ok: true,
                });
                if stale {
                    self.stale.insert(facts.class);
                } else {
                    self.stale.remove(&facts.class);
                }
            }
        }
        Ok(outcome)
    }
}

impl<T: FreshnessSource + ?Sized> FreshnessSource for Box<T> {
    fn classes(&mut self, max: usize) -> Result<Vec<ClassFacts>, String> {
        (**self).classes(max)
    }
}

impl<T: StampSink + ?Sized> StampSink for Box<T> {
    fn set_stale(&mut self, class: &str, stale: bool) {
        (**self).set_stale(class, stale);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::audit::MemoryDecisions;

    struct Ages(Vec<ClassFacts>);
    impl FreshnessSource for Ages {
        fn classes(&mut self, max: usize) -> Result<Vec<ClassFacts>, String> {
            Ok(self.0.iter().take(max).cloned().collect())
        }
    }

    #[derive(Clone, Default)]
    struct Stamps(Arc<Mutex<Vec<(String, bool)>>>);
    impl StampSink for Stamps {
        fn set_stale(&mut self, class: &str, stale: bool) {
            self.0.lock().unwrap().push((class.to_owned(), stale));
        }
    }

    #[derive(Default)]
    struct Conditions(Vec<String>);
    impl ConditionSink for Conditions {
        fn raise(&mut self, r: &Raised) {
            self.0.push(format!(
                "raise {} {} {:?}",
                r.condition, r.subject, r.severity
            ));
        }
        fn clear(&mut self, c: &'static str, s: &str) {
            self.0.push(format!("clear {c} {s}"));
        }
    }

    fn facts(class: &str, age: i64) -> ClassFacts {
        ClassFacts {
            class: class.to_owned(),
            age_secs: age,
        }
    }

    #[test]
    fn a_class_turns_amber_then_red_and_is_stamped_once_each_way() {
        let memory = MemoryDecisions::new();
        let stamps = Stamps::default();
        let mut step = FreshnessStep::new(
            Ages(vec![facts("prices", 1_000)]),
            stamps.clone(),
            Windows::default(),
            SharedDecisions::new(Box::new(memory.clone())),
        );
        let mut sink = Conditions::default();
        step.run(10, 10, &mut sink).unwrap();
        assert_eq!(sink.0, ["raise FRESHNESS_STALE prices Amber"]);
        // Red later: the stamp is not flipped again.
        step.source.0 = vec![facts("prices", 90_000)];
        step.run(20, 10, &mut sink).unwrap();
        assert_eq!(sink.0[1], "raise FRESHNESS_STALE prices Red");
        // Fresh again: cleared and the stamp comes off.
        step.source.0 = vec![facts("prices", 5)];
        step.run(30, 10, &mut sink).unwrap();
        assert_eq!(sink.0[2], "clear FRESHNESS_STALE prices");
        assert_eq!(
            *stamps.0.lock().unwrap(),
            [("prices".to_owned(), true), ("prices".to_owned(), false)]
        );
        let actions: Vec<&str> = memory.all().iter().map(|d| d.action).collect();
        assert_eq!(actions, ["stamp_stale", "stamp_fresh"]);
    }

    #[test]
    fn the_bound_truncates_and_says_so() {
        let mut step = FreshnessStep::new(
            Ages(vec![facts("a", 1), facts("b", 1), facts("c", 1)]),
            Stamps::default(),
            Windows::default(),
            SharedDecisions::new(Box::new(MemoryDecisions::new())),
        );
        let outcome = step.run(1, 2, &mut Conditions::default()).unwrap();
        assert_eq!(outcome.examined, 2);
        assert!(outcome.truncated);
    }
}
