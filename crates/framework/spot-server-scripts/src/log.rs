//! Observability: what every script run did, in a bounded in-memory ring and as telemetry.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};

use spot_app_api::prelude::OffsetDateTime;

use crate::record::ScriptEvent;

/// A level a script may log at with `host.log`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LogLevel {
    /// Detailed, for debugging a script.
    Debug,
    /// Ordinary progress.
    Info,
    /// Something a script author should look at.
    Warn,
    /// A failure the script handled itself.
    Error,
}

impl LogLevel {
    /// Reads a level from its name; unknown names become [`LogLevel::Info`].
    pub fn parse(text: &str) -> LogLevel {
        match text {
            "debug" => LogLevel::Debug,
            "warn" | "warning" => LogLevel::Warn,
            "error" => LogLevel::Error,
            _ => LogLevel::Info,
        }
    }

    /// The level's name.
    pub fn as_str(self) -> &'static str {
        match self {
            LogLevel::Debug => "debug",
            LogLevel::Info => "info",
            LogLevel::Warn => "warn",
            LogLevel::Error => "error",
        }
    }
}

/// One script run: the script, the event, how long it took, how many host calls it made, and
/// how it ended. The result code is `ok` for a run that finished, otherwise the stable error
/// code (a `script.*` engine code or a `serverscript.*` code).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScriptRun {
    /// The script's name.
    pub script: String,
    /// The DocType the script listens to.
    pub doctype: String,
    /// The event it ran for.
    pub event: ScriptEvent,
    /// The result code: `ok`, or the error's code.
    pub code: String,
    /// Whether the run finished without an error.
    pub ok: bool,
    /// How long the run took, in milliseconds.
    pub duration_ms: u64,
    /// How many host calls the run made.
    pub host_calls: u32,
    /// When the run finished.
    pub at: OffsetDateTime,
    /// A message a script logged (`host.log`), when it logged one.
    pub message: Option<String>,
}

impl ScriptRun {
    /// Whether the run succeeded.
    pub fn is_ok(&self) -> bool {
        self.ok
    }
}

/// A sink that receives every run as telemetry. A host wires this to its metrics and tracing.
pub trait ScriptTelemetry: Send + Sync {
    /// One finished run.
    fn run(&self, run: &ScriptRun);
}

/// The bounded, thread-safe ring of recent runs, plus the optional telemetry sink.
pub struct ScriptLog {
    capacity: usize,
    runs: Mutex<VecDeque<ScriptRun>>,
    telemetry: Option<Arc<dyn ScriptTelemetry>>,
}

impl ScriptLog {
    /// A log keeping the last `capacity` runs (at least one).
    pub fn new(capacity: usize) -> ScriptLog {
        ScriptLog {
            capacity: capacity.max(1),
            runs: Mutex::new(VecDeque::new()),
            telemetry: None,
        }
    }

    /// The same log, also reporting every run to `telemetry`.
    #[must_use]
    pub fn with_telemetry(mut self, telemetry: Arc<dyn ScriptTelemetry>) -> ScriptLog {
        self.telemetry = Some(telemetry);
        self
    }

    /// The log's capacity.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Records a run: it goes to the ring (evicting the oldest) and to telemetry.
    pub fn record(&self, run: ScriptRun) {
        if let Some(telemetry) = &self.telemetry {
            telemetry.run(&run);
        }
        let mut runs = self.lock();
        runs.push_back(run);
        while runs.len() > self.capacity {
            runs.pop_front();
        }
    }

    /// A snapshot of the runs, oldest first.
    pub fn runs(&self) -> Vec<ScriptRun> {
        self.lock().iter().cloned().collect()
    }

    /// The most recent `n` runs, oldest first.
    pub fn recent(&self, n: usize) -> Vec<ScriptRun> {
        let runs = self.lock();
        let skip = runs.len().saturating_sub(n);
        runs.iter().skip(skip).cloned().collect()
    }

    /// How many runs are in the ring now.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Whether the ring holds no runs.
    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// Empties the ring.
    pub fn clear(&self) {
        self.lock().clear();
    }

    fn lock(&self) -> MutexGuard<'_, VecDeque<ScriptRun>> {
        self.runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn run(name: &str) -> ScriptRun {
        ScriptRun {
            script: name.into(),
            doctype: "note".into(),
            event: ScriptEvent::Validate,
            code: "ok".into(),
            ok: true,
            duration_ms: 1,
            host_calls: 0,
            at: OffsetDateTime::UNIX_EPOCH,
            message: None,
        }
    }

    #[test]
    fn the_ring_keeps_the_most_recent_runs() {
        let log = ScriptLog::new(2);
        log.record(run("a"));
        log.record(run("b"));
        log.record(run("c"));
        assert_eq!(log.len(), 2);
        let names: Vec<String> = log.runs().into_iter().map(|r| r.script).collect();
        assert_eq!(names, vec!["b".to_string(), "c".to_string()]);
        assert_eq!(log.recent(1)[0].script, "c");
    }
}
