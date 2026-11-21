//! The series step (i): samples the numbers an operator watches over time (queue depth, oldest
//! queued age, parked bundles, repair list length) into the health view's series (WP-RTB-02.T11).
//!
//! It raises nothing and responds to nothing: a series is history, and the conditions of the other
//! steps are the alarms. A sample is a name from a fixed list, a time and a number: never a
//! subject's content.

use crate::runner::{ConditionSink, Step, StepError, StepOutcome};

/// One sample.
#[derive(Clone, Debug, PartialEq)]
pub struct Sample {
    /// The series name (`queue_depth`, `queue_age_secs`, `parked`, `repair`): a fixed word.
    pub name: &'static str,
    /// The value.
    pub value: f64,
}

/// Where the numbers come from.
pub trait SampleSource: Send {
    /// The current samples, at most `max`.
    ///
    /// # Errors
    /// A reason when a store cannot be read.
    fn samples(&mut self, max: usize) -> Result<Vec<Sample>, String>;
}

/// Where samples go (the health view).
pub trait SeriesSink: Send {
    /// Records `sample` at `at` (Unix seconds).
    fn record(&mut self, at: i64, sample: &Sample);
}

/// The step.
pub struct SeriesStep<S: SampleSource, K: SeriesSink> {
    source: S,
    sink: K,
}

impl<S: SampleSource, K: SeriesSink> SeriesStep<S, K> {
    /// A step over `source` and `sink`.
    pub fn new(source: S, sink: K) -> Self {
        Self { source, sink }
    }
}

impl<S: SampleSource, K: SeriesSink> Step for SeriesStep<S, K> {
    fn name(&self) -> &'static str {
        "series"
    }

    fn interval_secs(&self) -> u64 {
        60
    }

    fn run(
        &mut self,
        now: i64,
        max_items: usize,
        _sink: &mut dyn ConditionSink,
    ) -> Result<StepOutcome, StepError> {
        let samples = self
            .source
            .samples(max_items.saturating_add(1))
            .map_err(|reason| StepError { reason })?;
        let mut outcome = StepOutcome {
            truncated: samples.len() > max_items,
            ..StepOutcome::default()
        };
        for sample in samples.into_iter().take(max_items) {
            self.sink.record(now, &sample);
            outcome.examined += 1;
        }
        Ok(outcome)
    }
}

impl<T: SampleSource + ?Sized> SampleSource for Box<T> {
    fn samples(&mut self, max: usize) -> Result<Vec<Sample>, String> {
        (**self).samples(max)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::runner::Raised;

    struct Fixed(Vec<Sample>);
    impl SampleSource for Fixed {
        fn samples(&mut self, max: usize) -> Result<Vec<Sample>, String> {
            Ok(self.0.iter().take(max).cloned().collect())
        }
    }

    #[derive(Clone, Default)]
    struct Keep(Arc<Mutex<Vec<(i64, String, f64)>>>);
    impl SeriesSink for Keep {
        fn record(&mut self, at: i64, sample: &Sample) {
            self.0
                .lock()
                .unwrap()
                .push((at, sample.name.to_owned(), sample.value));
        }
    }

    struct Quiet;
    impl ConditionSink for Quiet {
        fn raise(&mut self, _: &Raised) {
            panic!("a series raises nothing");
        }
        fn clear(&mut self, _: &'static str, _: &str) {
            panic!("a series clears nothing");
        }
    }

    #[test]
    fn samples_are_recorded_at_the_tick_time_and_nothing_is_raised() {
        let keep = Keep::default();
        let mut step = SeriesStep::new(
            Fixed(vec![
                Sample {
                    name: "queue_depth",
                    value: 4.0,
                },
                Sample {
                    name: "parked",
                    value: 1.0,
                },
                Sample {
                    name: "repair",
                    value: 0.0,
                },
            ]),
            keep.clone(),
        );
        let outcome = step.run(500, 2, &mut Quiet).unwrap();
        assert!(outcome.truncated);
        assert_eq!(
            *keep.0.lock().unwrap(),
            [
                (500, "queue_depth".to_owned(), 4.0),
                (500, "parked".to_owned(), 1.0)
            ]
        );
    }
}
