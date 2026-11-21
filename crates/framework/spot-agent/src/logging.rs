//! Structured logs: JSON lines, every message passed through the `spot-redact` rules,
//! plus the panic hook (writes the log, exits non-zero so the service manager restarts).

use std::io::Write as _;
use std::sync::{Arc, Mutex};

use spot_redact::{RedactionPolicy, Redactor};

use crate::config::LogLevel;

/// A shared JSON-lines logger.
#[derive(Clone)]
pub struct Logger {
    level: LogLevel,
    redactor: Arc<Redactor>,
    sink: Arc<Mutex<Box<dyn std::io::Write + Send>>>,
}

impl Logger {
    /// A logger writing to `sink`.
    #[must_use]
    pub fn new(level: LogLevel, sink: Box<dyn std::io::Write + Send>) -> Self {
        Self::with_redactor(level, sink, build_redactor(&[]))
    }

    /// A logger with a prepared redactor (known secrets registered).
    #[must_use]
    pub fn with_redactor(
        level: LogLevel,
        sink: Box<dyn std::io::Write + Send>,
        redactor: Option<Redactor>,
    ) -> Self {
        Self {
            level,
            redactor: Arc::new(redactor.unwrap_or_else(fallback_redactor)),
            sink: Arc::new(Mutex::new(sink)),
        }
    }

    /// Writes one line: `{"level","event","message"}`. The message is redacted; an event
    /// name is a fixed identifier chosen by the code, never data.
    pub fn log(&self, level: LogLevel, event: &str, message: &str) {
        if level > self.level {
            return;
        }
        let redacted = self.redactor.redact_str(message);
        let line = serde_json::json!({
            "level": level.as_str(),
            "event": event,
            "message": redacted.as_ref(),
        });
        if let Ok(mut sink) = self.sink.lock() {
            let _ = writeln!(sink, "{line}");
            let _ = sink.flush();
        }
    }

    /// Info line.
    pub fn info(&self, event: &str, message: &str) {
        self.log(LogLevel::Info, event, message);
    }

    /// Warning line.
    pub fn warn(&self, event: &str, message: &str) {
        self.log(LogLevel::Warn, event, message);
    }

    /// Error line.
    pub fn error(&self, event: &str, message: &str) {
        self.log(LogLevel::Error, event, message);
    }
}

/// Builds the default redactor and registers each known secret (a secret shorter than the
/// redactor's minimum is skipped; the pattern rules still apply).
#[must_use]
pub fn build_redactor(secrets: &[(&str, &str)]) -> Option<Redactor> {
    let mut redactor = Redactor::from_policy(&RedactionPolicy::default()).ok()?;
    for (label, value) in secrets {
        let _ = redactor.register_secret(label, &spot_secrets::SecretString::new(*value));
    }
    Some(redactor)
}

fn fallback_redactor() -> Redactor {
    // `from_policy` only fails on a pattern compile bug; the default policy is a constant.
    match Redactor::from_policy(&RedactionPolicy::default()) {
        Ok(r) => r,
        Err(_) => std::process::exit(70),
    }
}

/// Installs the panic hook: logs the panic location (never the payload, which could hold
/// data) and exits with status 101 so the service manager restarts the agent.
pub fn install_panic_hook(logger: Logger) {
    std::panic::set_hook(Box::new(move |info| {
        let location = info.location().map_or_else(
            || "unknown".to_owned(),
            |l| format!("{}:{}", l.file(), l.line()),
        );
        logger.error("panic", &format!("the agent panicked at {location}"));
        std::process::exit(101);
    }));
}
