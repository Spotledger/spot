//! The server loop end to end with a fake event source: a change is classified, routed to its
//! step and recorded in the status store (WP-DX-02.T1 deliverables 2 and 4).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use camino::Utf8PathBuf;
use spot_core::SystemClock;
use spot_devserver::DevServer;
use spot_devserver::classify::Classifier;
use spot_devserver::reload::{ReloadContext, Reloader, Step, StepName, StepOutcome};
use spot_devserver::status::StatusStore;
use spot_devserver::watch::{Op, RawEvent, VecEventSource, Watcher, WatcherConfig};

struct Echo(StepName);

impl Step for Echo {
    fn name(&self) -> StepName {
        self.0
    }

    fn reload(&self, ctx: &ReloadContext<'_>) -> StepOutcome {
        StepOutcome::ok(format!("{} change(s)", ctx.changes.len()))
    }
}

#[test]
fn a_change_flows_from_the_watcher_to_the_status_endpoint() {
    let root = Utf8PathBuf::from("/scratch/dev");
    let source = VecEventSource::new();
    let mut watcher = Watcher::new(
        source,
        WatcherConfig {
            debounce: Duration::ZERO,
            extra_ignored: Vec::new(),
        },
    );
    watcher
        .source_mut()
        .push(RawEvent::new("/scratch/dev/meta/widget.toml", Op::Modified));
    watcher
        .source_mut()
        .push(RawEvent::new("/scratch/dev/ts/main.ts", Op::Modified));

    let reloader = Arc::new(
        Reloader::new(8)
            .with_step(Arc::new(Echo(StepName::Metadata)))
            .with_step(Arc::new(Echo(StepName::Scripts))),
    );
    let status = Arc::new(StatusStore::new(8));
    let mut server = DevServer::new(root, Classifier::new(), watcher, reloader, status.clone());

    let records = server.poll_once(&SystemClock::new());
    assert_eq!(records.len(), 2, "one record per step");
    assert!(records.iter().all(|record| record.ok));

    let snapshot = status.snapshot();
    assert_eq!(snapshot.reloads, 2);
    assert_eq!(snapshot.failures, 0);
    let json = status.to_json();
    assert!(json.contains("\"metadata\""));
    assert!(json.contains("\"scripts\""));
    assert_eq!(server.poll_once(&SystemClock::new()).len(), 0);
}
