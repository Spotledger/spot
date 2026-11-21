//! The closure repair step (f): bundles that left parking unapplied wait on the repair list; this
//! step asks the hub for the records they miss, a bounded number per run, and calls a person for the
//! ones that stay stuck (ADR-064 section 6; WP-RTB-02.T11).
//!
//! The repair pull itself is `spot-sync-repair-pull`'s; the step drives it through [`RepairPull`].
//! Each pull is an automatic response and writes a `sys_sync_decision`. An item older than
//! [`Windows::stuck_after`] raises `MISSING_DEP` amber, and red past
//! [`Windows::red_after`].

use crate::audit::{Decision, SharedDecisions};
use crate::runner::{ConditionSink, Raised, Severity, Step, StepError, StepOutcome};

/// A repair item has waited too long: the catalogue's `MISSING_DEP` (ADR-067).
pub const CLOSURE_REPAIR_STUCK: &str = "MISSING_DEP";

/// One repair-list entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Waiting {
    /// The subject: the bundle's first position (an identity).
    pub subject: String,
    /// Seconds on the list.
    pub age_secs: i64,
}

/// Where the repair list is read.
pub trait RepairSource: Send {
    /// At most `max` entries, oldest first.
    ///
    /// # Errors
    /// A reason when the list cannot be read.
    fn waiting(&mut self, max: usize) -> Result<Vec<Waiting>, String>;
}

/// The repair pull: asks the hub for what the bundle misses.
pub trait RepairPull: Send {
    /// Pulls for `subject`.
    ///
    /// # Errors
    /// A reason without content.
    fn pull(&mut self, subject: &str) -> Result<(), String>;
}

/// Windows, seconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Windows {
    /// Amber after this long on the list.
    pub stuck_after: i64,
    /// Red after this long.
    pub red_after: i64,
}

impl Default for Windows {
    fn default() -> Self {
        Self {
            stuck_after: 3_600,
            red_after: 24 * 3_600,
        }
    }
}

/// The step.
pub struct ClosureRepairStep<S: RepairSource, P: RepairPull> {
    source: S,
    pull: P,
    windows: Windows,
    decisions: SharedDecisions,
}

impl<S: RepairSource, P: RepairPull> ClosureRepairStep<S, P> {
    /// A step over `source` and `pull`.
    pub fn new(source: S, pull: P, windows: Windows, decisions: SharedDecisions) -> Self {
        Self {
            source,
            pull,
            windows,
            decisions,
        }
    }
}

impl<S: RepairSource, P: RepairPull> Step for ClosureRepairStep<S, P> {
    fn name(&self) -> &'static str {
        "closure_repair"
    }

    fn interval_secs(&self) -> u64 {
        60
    }

    fn run(
        &mut self,
        now: i64,
        max_items: usize,
        sink: &mut dyn ConditionSink,
    ) -> Result<StepOutcome, StepError> {
        let items = self
            .source
            .waiting(max_items.saturating_add(1))
            .map_err(|reason| StepError { reason })?;
        let mut outcome = StepOutcome {
            truncated: items.len() > max_items,
            ..StepOutcome::default()
        };
        for item in items.into_iter().take(max_items) {
            outcome.examined += 1;
            let result = self.pull.pull(&item.subject);
            self.decisions.record(&Decision {
                step: "closure_repair",
                action: "repair_pull",
                subject: item.subject.clone(),
                at: now,
                ok: result.is_ok(),
            });
            if item.age_secs > self.windows.stuck_after {
                let severity = if item.age_secs > self.windows.red_after {
                    Severity::Red
                } else {
                    Severity::Amber
                };
                sink.raise(&Raised {
                    condition: CLOSURE_REPAIR_STUCK,
                    subject: item.subject,
                    severity,
                });
                outcome.raised += 1;
            } else {
                sink.clear(CLOSURE_REPAIR_STUCK, &item.subject);
                outcome.cleared += 1;
            }
        }
        Ok(outcome)
    }
}

impl<T: RepairSource + ?Sized> RepairSource for Box<T> {
    fn waiting(&mut self, max: usize) -> Result<Vec<Waiting>, String> {
        (**self).waiting(max)
    }
}

impl<T: RepairPull + ?Sized> RepairPull for Box<T> {
    fn pull(&mut self, subject: &str) -> Result<(), String> {
        (**self).pull(subject)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::audit::MemoryDecisions;

    struct List(Vec<Waiting>);
    impl RepairSource for List {
        fn waiting(&mut self, max: usize) -> Result<Vec<Waiting>, String> {
            Ok(self.0.iter().take(max).cloned().collect())
        }
    }

    #[derive(Clone, Default)]
    struct Pulls(Arc<Mutex<Vec<String>>>);
    impl RepairPull for Pulls {
        fn pull(&mut self, subject: &str) -> Result<(), String> {
            self.0.lock().unwrap().push(subject.to_owned());
            Ok(())
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

    fn waiting(subject: &str, age: i64) -> Waiting {
        Waiting {
            subject: subject.to_owned(),
            age_secs: age,
        }
    }

    #[test]
    fn every_pull_is_audited_and_age_decides_the_condition() {
        let memory = MemoryDecisions::new();
        let pulls = Pulls::default();
        let mut step = ClosureRepairStep::new(
            List(vec![
                waiting("p10", 60),
                waiting("p20", 7_200),
                waiting("p30", 100_000),
            ]),
            pulls.clone(),
            Windows::default(),
            SharedDecisions::new(Box::new(memory.clone())),
        );
        let mut sink = Conditions::default();
        step.run(5, 10, &mut sink).unwrap();
        assert_eq!(*pulls.0.lock().unwrap(), ["p10", "p20", "p30"]);
        assert_eq!(memory.all().len(), 3);
        assert_eq!(
            sink.0,
            [
                "clear MISSING_DEP p10",
                "raise MISSING_DEP p20 Amber",
                "raise MISSING_DEP p30 Red"
            ]
        );
    }

    #[test]
    fn the_bound_limits_the_pulls_of_one_run() {
        let pulls = Pulls::default();
        let mut step = ClosureRepairStep::new(
            List(vec![waiting("a", 1), waiting("b", 1), waiting("c", 1)]),
            pulls.clone(),
            Windows::default(),
            SharedDecisions::new(Box::new(MemoryDecisions::new())),
        );
        let outcome = step.run(1, 2, &mut Conditions::default()).unwrap();
        assert!(outcome.truncated);
        assert_eq!(pulls.0.lock().unwrap().len(), 2);
    }
}
