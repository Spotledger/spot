//! The obligations step: due soon, overdue, red, and the ladder (`notify_contact` before
//! `suspend_node`, decision R1 of WP-SYN-10.T3).

use crate::runner::{ConditionSink, Raised, Severity, Step, StepError, StepOutcome};

/// One open obligation from the ledger.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Obligation {
    /// The obligation id (an identity, never content).
    pub id: String,
    /// The node that owes it.
    pub node: String,
    /// The deadline, Unix seconds.
    pub deadline: i64,
}

/// Where open obligations are read from (the ledger).
pub trait ObligationSource: Send {
    /// Open obligations, at most `max`, oldest deadline first.
    ///
    /// # Errors
    ///
    /// A reason when the ledger cannot be read.
    fn open(&mut self, max: usize) -> Result<Vec<Obligation>, String>;
}

/// The automatic responses the ladder names; each call writes a `sys_sync_decision`.
pub trait Responder: Send {
    /// Notifies the node's contact (always before a suspension).
    fn notify_contact(&mut self, node: &str, obligation: &str);
    /// Suspends the node.
    fn suspend_node(&mut self, node: &str, obligation: &str);
}

/// Windows around a deadline, seconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Windows {
    /// Due soon: within this long before the deadline.
    pub due_soon: i64,
    /// Red: overdue by more than this.
    pub red_after: i64,
}

impl Default for Windows {
    fn default() -> Self {
        Self {
            due_soon: 24 * 3_600,
            red_after: 72 * 3_600,
        }
    }
}

/// The step.
pub struct ObligationsStep<S: ObligationSource, R: Responder> {
    source: S,
    responder: R,
    windows: Windows,
    notified: std::collections::BTreeSet<String>,
}

impl<S: ObligationSource, R: Responder> ObligationsStep<S, R> {
    /// A step over `source`, answering through `responder`.
    pub fn new(source: S, responder: R, windows: Windows) -> Self {
        Self {
            source,
            responder,
            windows,
            notified: std::collections::BTreeSet::new(),
        }
    }

    /// The responder (for tests and the host).
    pub fn responder(&self) -> &R {
        &self.responder
    }
}

impl<S: ObligationSource, R: Responder> Step for ObligationsStep<S, R> {
    fn name(&self) -> &'static str {
        "obligations"
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
        let open = self
            .source
            .open(max_items.saturating_add(1))
            .map_err(|reason| StepError { reason })?;
        let truncated = open.len() > max_items;
        let mut outcome = StepOutcome {
            truncated,
            ..StepOutcome::default()
        };
        for o in open.into_iter().take(max_items) {
            outcome.examined += 1;
            let late = now.saturating_sub(o.deadline);
            let (condition, severity) = if late > self.windows.red_after {
                ("OBLIGATION_RED", Severity::Red)
            } else if late > 0 {
                ("OBLIGATION_OVERDUE", Severity::Amber)
            } else if -late <= self.windows.due_soon {
                ("OBLIGATION_DUE_SOON", Severity::Info)
            } else {
                continue;
            };
            sink.raise(&Raised {
                condition,
                subject: o.id.clone(),
                severity,
            });
            outcome.raised += 1;
            // The ladder: contact first (once), suspension only when red and already notified.
            if severity >= Severity::Amber && self.notified.insert(o.id.clone()) {
                self.responder.notify_contact(&o.node, &o.id);
            } else if severity == Severity::Red && self.notified.contains(&o.id) {
                self.responder.suspend_node(&o.node, &o.id);
            }
        }
        Ok(outcome)
    }
}

impl<T: ObligationSource + ?Sized> ObligationSource for Box<T> {
    fn open(&mut self, max: usize) -> Result<Vec<Obligation>, String> {
        (**self).open(max)
    }
}

impl<T: Responder + ?Sized> Responder for Box<T> {
    fn notify_contact(&mut self, node: &str, obligation: &str) {
        (**self).notify_contact(node, obligation);
    }

    fn suspend_node(&mut self, node: &str, obligation: &str) {
        (**self).suspend_node(node, obligation);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    struct Src(Vec<Obligation>);
    impl ObligationSource for Src {
        fn open(&mut self, max: usize) -> Result<Vec<Obligation>, String> {
            Ok(self.0.iter().take(max).cloned().collect())
        }
    }

    #[derive(Default)]
    struct Log(Vec<String>);
    impl Responder for Log {
        fn notify_contact(&mut self, node: &str, o: &str) {
            self.0.push(format!("notify {node} {o}"));
        }
        fn suspend_node(&mut self, node: &str, o: &str) {
            self.0.push(format!("suspend {node} {o}"));
        }
    }

    #[derive(Default)]
    struct Sink(Vec<(&'static str, String)>);
    impl ConditionSink for Sink {
        fn raise(&mut self, r: &Raised) {
            self.0.push((r.condition, r.subject.clone()));
        }
        fn clear(&mut self, _: &'static str, _: &str) {}
    }

    fn ob(id: &str, deadline: i64) -> Obligation {
        Obligation {
            id: id.into(),
            node: "n1".into(),
            deadline,
        }
    }

    #[test]
    fn the_ladder_notifies_before_it_suspends_and_each_class_is_raised() {
        let now = 1_000_000;
        let src = Src(vec![
            ob("soon", now + 3_600),
            ob("far", now + 10 * 86_400),
            ob("late", now - 3_600),
            ob("red", now - 4 * 86_400),
        ]);
        let mut step = ObligationsStep::new(src, Log::default(), Windows::default());
        let mut sink = Sink::default();
        let outcome = step.run(now, 100, &mut sink).unwrap();
        assert_eq!(outcome.examined, 4);
        let kinds: Vec<_> = sink.0.iter().map(|(c, s)| format!("{c}:{s}")).collect();
        assert_eq!(
            kinds,
            [
                "OBLIGATION_DUE_SOON:soon",
                "OBLIGATION_OVERDUE:late",
                "OBLIGATION_RED:red"
            ]
        );
        // First run: contact notified for the overdue and the red one, nobody suspended yet.
        assert_eq!(step.responder().0, ["notify n1 late", "notify n1 red"]);
        // Second run: the red one was already notified, so now the node is suspended.
        step.run(now + 60, 100, &mut sink).unwrap();
        assert!(step.responder().0.contains(&"suspend n1 red".to_owned()));
        assert!(!step.responder().0.iter().any(|l| l == "suspend n1 late"));
    }

    #[test]
    fn the_bound_truncates_and_says_so() {
        let src = Src((0..5).map(|i| ob(&format!("o{i}"), 0)).collect());
        let mut step = ObligationsStep::new(src, Log::default(), Windows::default());
        let outcome = step.run(10, 2, &mut Sink::default()).unwrap();
        assert!(outcome.truncated);
        assert_eq!(outcome.examined, 2);
    }
}
