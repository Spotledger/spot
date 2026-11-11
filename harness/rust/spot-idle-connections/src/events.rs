//! Counting the server's accept errors. `spot-http` does not expose them: its accept loop logs
//! `accept failed; backing off` at debug level and sleeps. The harness installs one global
//! tracing layer that counts exactly those events.

use std::fmt;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::{Context, SubscriberExt};

static ACCEPT_ERRORS: AtomicU64 = AtomicU64::new(0);
static INSTALLED: OnceLock<bool> = OnceLock::new();

struct Message(String);

impl Visit for Message {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            use std::fmt::Write;
            let _ = write!(self.0, "{value:?}");
        }
    }
}

struct CountAccepts;

impl<S: Subscriber> Layer<S> for CountAccepts {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        if *meta.level() != Level::DEBUG || meta.target() != "spot_http::server" {
            return;
        }
        let mut message = Message(String::new());
        event.record(&mut message);
        if message.0.starts_with("accept failed") {
            ACCEPT_ERRORS.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Installs the counting layer as the global subscriber (once). Returns whether the counter is
/// live: `false` when another global subscriber was already installed, in which case the report
/// says accept errors were not counted.
pub fn install() -> bool {
    *INSTALLED.get_or_init(|| {
        let subscriber = tracing_subscriber::registry().with(CountAccepts);
        tracing::subscriber::set_global_default(subscriber).is_ok()
    })
}

/// Accept errors counted since the process started.
#[must_use]
pub fn accept_errors() -> u64 {
    ACCEPT_ERRORS.load(Ordering::Relaxed)
}
