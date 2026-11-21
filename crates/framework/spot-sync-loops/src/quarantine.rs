//! The quarantine step (g): items the apply path refused sit in the quarantine queue until a
//! person (or the queue's own retry) closes them; this step raises the age of what is open
//! (`QUARANTINED`, amber then red). It retries nothing and closes nothing: the queue
//! (`spot-quarantine`) owns its retry schedule and its audited closures, and a second actor here would
//! race it (WP-RTB-02.T11).

use crate::runner::{ConditionSink, Raised, Severity, Step, StepError, StepOutcome};

/// An open quarantine item is old: the catalogue's `QUARANTINED` (ADR-067).
pub const QUARANTINE_AGED: &str = "QUARANTINED";

/// One open item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenItem {
    /// The item id (an identity).
    pub subject: String,
    /// Seconds since it was first seen.
    pub age_secs: i64,
}

/// Where open items are read.
pub trait QuarantineSource: Send {
    /// At most `max` open items, oldest first.
    ///
    /// # Errors
    /// A reason when the queue cannot be read.
    fn open(&mut self, max: usize) -> Result<Vec<OpenItem>, String>;
}

/// Windows, seconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Windows {
    /// Amber after this long open.
    pub amber_after: i64,
    /// Red after this long open.
    pub red_after: i64,
}

impl Default for Windows {
    fn default() -> Self {
        Self {
            amber_after: 24 * 3_600,
            red_after: 7 * 24 * 3_600,
        }
    }
}

/// The step.
pub struct QuarantineStep<S: QuarantineSource> {
    source: S,
    windows: Windows,
}

impl<S: QuarantineSource> QuarantineStep<S> {
    /// A step over `source`.
    pub fn new(source: S, windows: Windows) -> Self {
        Self { source, windows }
    }
}

impl<S: QuarantineSource> Step for QuarantineStep<S> {
    fn name(&self) -> &'static str {
        "quarantine"
    }

    fn interval_secs(&self) -> u64 {
        600
    }

    fn run(
        &mut self,
        _now: i64,
        max_items: usize,
        sink: &mut dyn ConditionSink,
    ) -> Result<StepOutcome, StepError> {
        let items = self
            .source
            .open(max_items.saturating_add(1))
            .map_err(|reason| StepError { reason })?;
        let mut outcome = StepOutcome {
            truncated: items.len() > max_items,
            ..StepOutcome::default()
        };
        for item in items.into_iter().take(max_items) {
            outcome.examined += 1;
            if item.age_secs > self.windows.amber_after {
                let severity = if item.age_secs > self.windows.red_after {
                    Severity::Red
                } else {
                    Severity::Amber
                };
                sink.raise(&Raised {
                    condition: QUARANTINE_AGED,
                    subject: item.subject,
                    severity,
                });
                outcome.raised += 1;
            } else {
                sink.clear(QUARANTINE_AGED, &item.subject);
                outcome.cleared += 1;
            }
        }
        Ok(outcome)
    }
}

impl<T: QuarantineSource + ?Sized> QuarantineSource for Box<T> {
    fn open(&mut self, max: usize) -> Result<Vec<OpenItem>, String> {
        (**self).open(max)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    struct Items(Vec<OpenItem>);
    impl QuarantineSource for Items {
        fn open(&mut self, max: usize) -> Result<Vec<OpenItem>, String> {
            Ok(self.0.iter().take(max).cloned().collect())
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
    fn age_decides_the_condition_and_nothing_is_closed() {
        let item = |subject: &str, age| OpenItem {
            subject: subject.to_owned(),
            age_secs: age,
        };
        let mut step = QuarantineStep::new(
            Items(vec![
                item("1", 60),
                item("2", 2 * 24 * 3_600),
                item("3", 30 * 24 * 3_600),
            ]),
            Windows::default(),
        );
        let mut sink = Conditions::default();
        step.run(1, 10, &mut sink).unwrap();
        assert_eq!(
            sink.0,
            [
                "clear QUARANTINED 1",
                "raise QUARANTINED 2 Amber",
                "raise QUARANTINED 3 Red"
            ]
        );
    }
}
