//! The loop runner and its seams.

use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};

/// How serious an open condition is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Information.
    Info,
    /// Needs attention.
    Amber,
    /// Needs a person now.
    Red,
}

/// A condition a step raises.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Raised {
    /// The catalogue id (`NODE_SILENT`, ...).
    pub condition: &'static str,
    /// The subject (a node id, an obligation id): an identity, never content.
    pub subject: String,
    /// The severity.
    pub severity: Severity,
}

/// Where conditions go (the conditions service's handle).
pub trait ConditionSink {
    /// Raises (or keeps open) a condition.
    fn raise(&mut self, raised: &Raised);
    /// Clears a condition that no longer holds.
    fn clear(&mut self, condition: &'static str, subject: &str);
}

/// A step failed; the others still run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepError {
    /// A stable reason (never content).
    pub reason: String,
}

/// What a step did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StepOutcome {
    /// Subjects examined.
    pub examined: usize,
    /// Conditions raised or kept open.
    pub raised: usize,
    /// Conditions cleared.
    pub cleared: usize,
    /// Whether the step stopped at its bound with work left.
    pub truncated: bool,
}

/// One periodic step. `Send`, so a runner can live in a port shared between threads.
pub trait Step: Send {
    /// The step's name.
    fn name(&self) -> &'static str;
    /// The least time between two runs, seconds.
    fn interval_secs(&self) -> u64;
    /// Runs the step at `now` (Unix seconds), examining at most `max_items` subjects.
    ///
    /// # Errors
    ///
    /// [`StepError`]; the runner records it and goes on.
    fn run(
        &mut self,
        now: i64,
        max_items: usize,
        sink: &mut dyn ConditionSink,
    ) -> Result<StepOutcome, StepError>;
}

/// What one tick did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TickReport {
    /// Steps that ran, with their outcome.
    pub ran: Vec<(&'static str, StepOutcome)>,
    /// Steps that failed (or panicked), with the reason.
    pub failed: Vec<(&'static str, String)>,
    /// Steps not yet due.
    pub skipped: usize,
}

/// The readiness summary: `calm` or `N amber, M red`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    /// Open amber conditions.
    pub amber: usize,
    /// Open red conditions.
    pub red: usize,
}

impl Summary {
    /// The line the readiness shows.
    pub fn line(&self) -> String {
        if self.amber == 0 && self.red == 0 {
            "sync_loops: calm".to_owned()
        } else {
            format!("sync_loops: {} amber, {} red", self.amber, self.red)
        }
    }
}

/// Counts what is open, wrapping a sink so the runner can summarise.
#[derive(Default)]
struct Tally {
    open: BTreeMap<(&'static str, String), Severity>,
}

struct Tee<'a> {
    inner: &'a mut dyn ConditionSink,
    tally: &'a mut Tally,
}

impl ConditionSink for Tee<'_> {
    fn raise(&mut self, raised: &Raised) {
        self.tally
            .open
            .insert((raised.condition, raised.subject.clone()), raised.severity);
        self.inner.raise(raised);
    }

    fn clear(&mut self, condition: &'static str, subject: &str) {
        self.tally.open.remove(&(condition, subject.to_owned()));
        self.inner.clear(condition, subject);
    }
}

/// The runner.
pub struct Loops {
    steps: Vec<(Box<dyn Step>, Option<i64>)>,
    max_items: usize,
    tally: Tally,
}

impl Loops {
    /// A runner; each step examines at most `max_items` subjects per run.
    pub fn new(max_items: usize) -> Self {
        Self {
            steps: Vec::new(),
            max_items: max_items.max(1),
            tally: Tally::default(),
        }
    }

    /// Adds a step.
    pub fn with_step(mut self, step: Box<dyn Step>) -> Self {
        self.steps.push((step, None));
        self
    }

    /// Runs every due step. A failing or panicking step is recorded and the rest still run.
    pub fn tick(&mut self, now: i64, sink: &mut dyn ConditionSink) -> TickReport {
        let mut report = TickReport::default();
        let max = self.max_items;
        for (step, last) in &mut self.steps {
            let due = last.is_none_or(|at| {
                now.saturating_sub(at) >= i64::try_from(step.interval_secs()).unwrap_or(i64::MAX)
            });
            if !due {
                report.skipped += 1;
                continue;
            }
            *last = Some(now);
            let mut tee = Tee {
                inner: sink,
                tally: &mut self.tally,
            };
            let result = catch_unwind(AssertUnwindSafe(|| step.run(now, max, &mut tee)));
            match result {
                Ok(Ok(outcome)) => report.ran.push((step.name(), outcome)),
                Ok(Err(error)) => report.failed.push((step.name(), error.reason)),
                Err(_) => report
                    .failed
                    .push((step.name(), "the step panicked".to_owned())),
            }
        }
        report
    }

    /// The open conditions summarised.
    pub fn summary(&self) -> Summary {
        let mut summary = Summary::default();
        for severity in self.tally.open.values() {
            match severity {
                Severity::Amber => summary.amber += 1,
                Severity::Red => summary.red += 1,
                Severity::Info => {}
            }
        }
        summary
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[derive(Default)]
    pub struct Recorder(pub Vec<String>);

    impl ConditionSink for Recorder {
        fn raise(&mut self, r: &Raised) {
            self.0.push(format!("raise {} {}", r.condition, r.subject));
        }
        fn clear(&mut self, c: &'static str, s: &str) {
            self.0.push(format!("clear {c} {s}"));
        }
    }

    struct Boom;
    impl Step for Boom {
        fn name(&self) -> &'static str {
            "boom"
        }
        fn interval_secs(&self) -> u64 {
            10
        }
        fn run(
            &mut self,
            _: i64,
            _: usize,
            _: &mut dyn ConditionSink,
        ) -> Result<StepOutcome, StepError> {
            Err(StepError {
                reason: "down".into(),
            })
        }
    }

    struct Panics;
    impl Step for Panics {
        fn name(&self) -> &'static str {
            "panics"
        }
        fn interval_secs(&self) -> u64 {
            10
        }
        fn run(
            &mut self,
            _: i64,
            _: usize,
            _: &mut dyn ConditionSink,
        ) -> Result<StepOutcome, StepError> {
            panic!("bug");
        }
    }

    struct Raises;
    impl Step for Raises {
        fn name(&self) -> &'static str {
            "raises"
        }
        fn interval_secs(&self) -> u64 {
            60
        }
        fn run(
            &mut self,
            _: i64,
            _: usize,
            sink: &mut dyn ConditionSink,
        ) -> Result<StepOutcome, StepError> {
            sink.raise(&Raised {
                condition: "NODE_SILENT",
                subject: "n1".into(),
                severity: Severity::Amber,
            });
            sink.raise(&Raised {
                condition: "OBLIGATION_RED",
                subject: "o1".into(),
                severity: Severity::Red,
            });
            Ok(StepOutcome {
                examined: 2,
                raised: 2,
                ..StepOutcome::default()
            })
        }
    }

    #[test]
    fn one_failing_step_never_stops_the_others_and_intervals_are_respected() {
        let mut loops = Loops::new(100)
            .with_step(Box::new(Boom))
            .with_step(Box::new(Panics))
            .with_step(Box::new(Raises));
        let mut sink = Recorder::default();
        let report = loops.tick(1_000, &mut sink);
        assert_eq!(report.failed.len(), 2);
        assert_eq!(report.ran.len(), 1);
        assert_eq!(loops.summary().line(), "sync_loops: 1 amber, 1 red");
        // 5 s later nothing is due; 11 s later the 10 s steps are, the 60 s one is not.
        assert_eq!(loops.tick(1_005, &mut sink).skipped, 3);
        let later = loops.tick(1_011, &mut sink);
        assert_eq!(later.failed.len(), 2);
        assert_eq!(later.skipped, 1);
    }

    #[test]
    fn an_empty_tally_is_calm() {
        assert_eq!(Loops::new(1).summary().line(), "sync_loops: calm");
    }
}
