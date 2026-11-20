//! The log artefact of the secret canary sweep (WP-SEC-03.T15): a `tracing` subscriber that
//! keeps every event and span of the test process, at trace level, as text.
//!
//! The in-process host (`spot_cli::start_with`) installs no subscriber of its own (only the
//! binary does), so the sweep installs this one as the process default before anything runs.
//! Every field value is kept as the host wrote it (a string is kept raw, not quoted), because a
//! leak must be found whatever the log format would have done to it.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};

/// The shared buffer every captured line goes to.
#[derive(Clone, Default)]
pub struct Captured(Arc<Mutex<Vec<u8>>>);

impl Captured {
    /// A copy of everything captured so far.
    pub fn snapshot(&self) -> Vec<u8> {
        match self.0.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    fn push(&self, line: &str) {
        let mut guard = match self.0.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.extend_from_slice(line.as_bytes());
        guard.push(b'\n');
    }
}

/// Writes `name=value` pairs.
struct Fields<'a>(&'a mut String);

impl Visit for Fields<'_> {
    fn record_str(&mut self, field: &Field, value: &str) {
        let _ = write!(self.0, " {}={}", field.name(), value);
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let _ = write!(self.0, " {}={:?}", field.name(), value);
    }
}

struct Capture {
    out: Captured,
    next: AtomicU64,
}

impl Subscriber for Capture {
    fn enabled(&self, _: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, attributes: &Attributes<'_>) -> Id {
        // Span ids start at 1: `Id::from_u64(0)` panics.
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let mut line = format!(
            "span {} {}",
            attributes.metadata().target(),
            attributes.metadata().name()
        );
        attributes.record(&mut Fields(&mut line));
        self.out.push(&line);
        Id::from_u64(id)
    }

    fn record(&self, _: &Id, values: &Record<'_>) {
        let mut line = String::from("span-record");
        values.record(&mut Fields(&mut line));
        self.out.push(&line);
    }

    fn record_follows_from(&self, _: &Id, _: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let metadata = event.metadata();
        let mut line = format!(
            "{} {} {}",
            metadata.level(),
            metadata.target(),
            metadata.name()
        );
        event.record(&mut Fields(&mut line));
        self.out.push(&line);
    }

    fn enter(&self, _: &Id) {}

    fn exit(&self, _: &Id) {}
}

static INSTALLED: OnceLock<Captured> = OnceLock::new();

/// Installs the capturing subscriber as the process default and returns its buffer; later calls
/// return the same buffer.
///
/// # Panics
/// When another global subscriber is already installed: the log artefact would be empty, and a
/// sweep that cannot see the logs must fail, not pass.
pub fn install() -> Captured {
    INSTALLED
        .get_or_init(|| {
            let captured = Captured::default();
            let subscriber = Capture {
                out: captured.clone(),
                next: AtomicU64::new(1),
            };
            tracing::subscriber::set_global_default(subscriber).unwrap_or_else(|_| {
                panic!("another tracing subscriber is installed: the sweep cannot capture the logs")
            });
            captured
        })
        .clone()
}
