//! The dev server: the watcher and the reload pipeline, driven once per poll.
//!
//! [`DevServer::poll_once`] is the whole loop body: drain the watcher at the current monotonic
//! time, classify every change, run the matching steps, and record the outcome. The binary's
//! loop and the tests both call it, so there is one path and no hidden timer.

use std::sync::Arc;

use camino::Utf8PathBuf;
use spot_core::Clock;

use crate::classify::Classifier;
use crate::reload::{ReloadRecord, Reloader};
use crate::status::StatusStore;
use crate::watch::{EventSource, Watcher};

/// The running development server, generic over the event source.
pub struct DevServer<S: EventSource> {
    root: Utf8PathBuf,
    classifier: Classifier,
    watcher: Watcher<S>,
    reloader: Arc<Reloader>,
    status: Arc<StatusStore>,
}

impl<S: EventSource> DevServer<S> {
    /// Builds a server watching `root`, with `watcher`, `reloader` and `status`.
    #[must_use]
    pub fn new(
        root: impl Into<Utf8PathBuf>,
        classifier: Classifier,
        watcher: Watcher<S>,
        reloader: Arc<Reloader>,
        status: Arc<StatusStore>,
    ) -> Self {
        Self {
            root: root.into(),
            classifier,
            watcher,
            reloader,
            status,
        }
    }

    /// Starts watching the root.
    ///
    /// # Errors
    ///
    /// [`crate::DevError::Watch`] from the event source.
    pub fn watch(&mut self) -> crate::DevResult<()> {
        self.watcher.watch(std::slice::from_ref(&self.root))
    }

    /// Drains the watcher, runs the pipeline and records the outcome. Returns the records the
    /// batch produced (empty when nothing changed).
    pub fn poll_once(&mut self, clock: &dyn Clock) -> Vec<ReloadRecord> {
        let events = self.watcher.poll(clock.monotonic());
        if events.is_empty() {
            return Vec::new();
        }
        let changes: Vec<_> = events
            .iter()
            .map(|event| self.classifier.classify(event, &self.root))
            .collect();
        let summary = self.reloader.reload(&changes, clock);
        for record in &summary.records {
            self.status.record(record.clone());
        }
        summary.records
    }

    /// The watched root.
    #[must_use]
    pub fn root(&self) -> &camino::Utf8Path {
        &self.root
    }

    /// The status store the endpoint serves.
    #[must_use]
    pub fn status(&self) -> &Arc<StatusStore> {
        &self.status
    }

    /// The reloader, for the CLI's one-shot check.
    #[must_use]
    pub fn reloader(&self) -> &Arc<Reloader> {
        &self.reloader
    }
}
