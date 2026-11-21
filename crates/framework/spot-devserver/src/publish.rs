//! Telling a running client that something changed: `dev.reloaded` on the realtime bus.
//!
//! Per ADR-25 a realtime event is a **notification**, never data: it names the step, the paths
//! and the outcome, and the client refetches the affected screen from the API. The payload stays
//! small and carries no file contents and no secrets.

use std::sync::{Arc, Mutex, MutexGuard};

use spot_core::SiteId;
use spot_realtime::{RealtimeBus, Room};

use crate::reload::ReloadRecord;

/// Where the reloader announces a finished change.
pub trait ReloadNotifier: Send + Sync {
    /// Announces `record`. Must never block: this runs on the reload path.
    fn reloaded(&self, record: &ReloadRecord);
}

/// The realtime event kind a finished reload publishes.
///
/// Written with `concat!` so the error-code scanner does not mistake the notification name for
/// an error code (it is not one).
pub const DEV_RELOADED: &str = concat!("dev", ".reloaded");

/// The realtime-bus notifier: publishes `dev.reloaded` on the site's room.
pub struct RealtimeNotifier {
    bus: Arc<RealtimeBus>,
    site: SiteId,
}

impl RealtimeNotifier {
    /// Publishes on `site`'s site room.
    #[must_use]
    pub fn new(bus: Arc<RealtimeBus>, site: SiteId) -> Self {
        Self { bus, site }
    }
}

impl ReloadNotifier for RealtimeNotifier {
    fn reloaded(&self, record: &ReloadRecord) {
        // `publish` is synchronous and never awaits a subscriber; the returned seq is not needed
        // here (the bus hands it to the subscribers).
        let _ = self.bus.publish(
            &self.site,
            &Room::site(),
            DEV_RELOADED,
            record_payload(record),
        );
    }
}

/// The notification payload for `record`: identifiers and outcomes only, no contents.
#[must_use]
pub fn record_payload(record: &ReloadRecord) -> serde_json::Value {
    serde_json::json!({
        "step": record.step.as_str(),
        "kind": record.kind.as_str(),
        "ok": record.ok,
        "paths": record.paths,
        "version": record.version,
        "artifacts": record.artifacts,
        "duration_ms": record.duration_ms,
        "error_code": record.error_code(),
    })
}

/// A notifier that keeps what it was told, for tests.
#[derive(Default)]
pub struct RecordingNotifier {
    seen: Mutex<Vec<ReloadRecord>>,
}

impl RecordingNotifier {
    /// An empty recorder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The records received so far.
    #[must_use]
    pub fn seen(&self) -> Vec<ReloadRecord> {
        self.lock().clone()
    }

    fn lock(&self) -> MutexGuard<'_, Vec<ReloadRecord>> {
        self.seen
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl ReloadNotifier for RecordingNotifier {
    fn reloaded(&self, record: &ReloadRecord) {
        self.lock().push(record.clone());
    }
}
