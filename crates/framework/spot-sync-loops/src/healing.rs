//! The healing step (e): act on what the node can mend by itself, with an attempt budget, then
//! hand the rest to a person (WP-RTB-02.T11).
//!
//! A suspect (a scope the reconcile step found different, a record the closure could not complete)
//! gets up to [`Budget::attempts`] automatic mends. Each attempt is a [`Healer`] call and writes a
//! `sys_sync_decision`. A success clears the condition. When the budget is spent the step stops
//! trying, raises `HEAL_BUDGET_EXHAUSTED` **red** and keeps it open: what it cannot fix is a person's, and
//! it must never loop on it.

use std::collections::BTreeMap;

use crate::audit::{Decision, SharedDecisions};
use crate::runner::{ConditionSink, Raised, Severity, Step, StepError, StepOutcome};

/// Healing could not mend the subject within its budget: the catalogue's `HEAL_BUDGET_EXHAUSTED`.
pub const HEALING_FAILED: &str = "HEAL_BUDGET_EXHAUSTED";

/// Something to mend.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Suspect {
    /// The subject: a scope or a bundle's first position (an identity, never content).
    pub subject: String,
}

/// Where suspects come from.
pub trait HealingSource: Send {
    /// At most `max` suspects.
    ///
    /// # Errors
    /// A reason when they cannot be read.
    fn suspects(&mut self, max: usize) -> Result<Vec<Suspect>, String>;
}

/// The automatic mend: a scoped re-send request, a rebuild of one projection.
pub trait Healer: Send {
    /// Tries to mend `subject`.
    ///
    /// # Errors
    /// A reason without content when the mend did not work.
    fn heal(&mut self, subject: &str) -> Result<(), String>;
}

/// How hard to try.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Budget {
    /// Attempts per subject before it is left to a person.
    pub attempts: u32,
}

impl Default for Budget {
    fn default() -> Self {
        Self { attempts: 3 }
    }
}

/// The step.
pub struct HealingStep<S: HealingSource, H: Healer> {
    source: S,
    healer: H,
    budget: Budget,
    decisions: SharedDecisions,
    tried: BTreeMap<String, u32>,
}

impl<S: HealingSource, H: Healer> HealingStep<S, H> {
    /// A step over `source` and `healer`.
    pub fn new(source: S, healer: H, budget: Budget, decisions: SharedDecisions) -> Self {
        Self {
            source,
            healer,
            budget,
            decisions,
            tried: BTreeMap::new(),
        }
    }
}

impl<S: HealingSource, H: Healer> Step for HealingStep<S, H> {
    fn name(&self) -> &'static str {
        "healing"
    }

    fn interval_secs(&self) -> u64 {
        300
    }

    fn run(
        &mut self,
        now: i64,
        max_items: usize,
        sink: &mut dyn ConditionSink,
    ) -> Result<StepOutcome, StepError> {
        let suspects = self
            .source
            .suspects(max_items.saturating_add(1))
            .map_err(|reason| StepError { reason })?;
        let mut outcome = StepOutcome {
            truncated: suspects.len() > max_items,
            ..StepOutcome::default()
        };
        let current: Vec<String> = suspects
            .iter()
            .take(max_items)
            .map(|s| s.subject.clone())
            .collect();
        // A subject that is no longer a suspect is mended: forget it and clear.
        let gone: Vec<String> = self
            .tried
            .keys()
            .filter(|subject| !current.contains(subject))
            .cloned()
            .collect();
        for subject in gone {
            self.tried.remove(&subject);
            sink.clear(HEALING_FAILED, &subject);
            outcome.cleared += 1;
        }
        for subject in current {
            outcome.examined += 1;
            let tried = self.tried.entry(subject.clone()).or_insert(0);
            if *tried >= self.budget.attempts {
                sink.raise(&Raised {
                    condition: HEALING_FAILED,
                    subject,
                    severity: Severity::Red,
                });
                outcome.raised += 1;
                continue;
            }
            *tried += 1;
            // A mend that did not take leaves the subject a suspect next run: the budget counts it.
            let result = self.healer.heal(&subject);
            self.decisions.record(&Decision {
                step: "healing",
                action: "heal",
                subject: subject.clone(),
                at: now,
                ok: result.is_ok(),
            });
        }
        Ok(outcome)
    }
}

impl<T: HealingSource + ?Sized> HealingSource for Box<T> {
    fn suspects(&mut self, max: usize) -> Result<Vec<Suspect>, String> {
        (**self).suspects(max)
    }
}

impl<T: Healer + ?Sized> Healer for Box<T> {
    fn heal(&mut self, subject: &str) -> Result<(), String> {
        (**self).heal(subject)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::audit::MemoryDecisions;

    struct Always(Vec<String>);
    impl HealingSource for Always {
        fn suspects(&mut self, max: usize) -> Result<Vec<Suspect>, String> {
            Ok(self
                .0
                .iter()
                .take(max)
                .map(|s| Suspect { subject: s.clone() })
                .collect())
        }
    }

    struct Fails;
    impl Healer for Fails {
        fn heal(&mut self, _: &str) -> Result<(), String> {
            Err("no".to_owned())
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

    #[test]
    fn the_budget_is_spent_then_a_person_is_called_and_nothing_loops() {
        let memory = MemoryDecisions::new();
        let mut step = HealingStep::new(
            Always(vec!["scope_a".to_owned()]),
            Fails,
            Budget { attempts: 2 },
            SharedDecisions::new(Box::new(memory.clone())),
        );
        let mut sink = Conditions::default();
        for at in 1..=5 {
            step.run(at, 10, &mut sink).unwrap();
        }
        // Two audited attempts, then red every run and no more attempts.
        assert_eq!(memory.all().len(), 2);
        assert!(memory.all().iter().all(|d| !d.ok && d.action == "heal"));
        assert_eq!(
            sink.0
                .iter()
                .filter(|l| l.contains("HEAL_BUDGET_EXHAUSTED scope_a Red"))
                .count(),
            3
        );
    }

    #[test]
    fn a_subject_that_stops_being_a_suspect_is_cleared_and_forgotten() {
        let mut step = HealingStep::new(
            Always(vec!["scope_a".to_owned()]),
            Fails,
            Budget { attempts: 1 },
            SharedDecisions::new(Box::new(MemoryDecisions::new())),
        );
        let mut sink = Conditions::default();
        step.run(1, 10, &mut sink).unwrap();
        step.run(2, 10, &mut sink).unwrap();
        step.source.0.clear();
        let outcome = step.run(3, 10, &mut sink).unwrap();
        assert_eq!(outcome.cleared, 1);
        assert_eq!(
            sink.0.last().unwrap(),
            "clear HEAL_BUDGET_EXHAUSTED scope_a"
        );
        assert!(step.tried.is_empty());
    }
}
