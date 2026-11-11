//! A real-filesystem test of the platform watcher: a file written after the watch starts is
//! seen, on Linux and Windows, without the test sleeping (WP-DX-02.T1 deliverable 5).
//!
//! The test polls the watcher's own channel in a tight loop against the injected monotonic
//! clock, so it is time-bounded but never `sleep`s.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use camino::Utf8PathBuf;
use spot_core::{Clock, SystemClock};
use spot_devserver::watch::{NotifyEventSource, Watcher, WatcherConfig};

#[test]
fn a_file_written_after_the_watch_starts_is_observed() {
    let temp = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();

    let mut watcher = Watcher::new(
        NotifyEventSource::new().unwrap(),
        WatcherConfig {
            debounce: Duration::from_millis(50),
            extra_ignored: Vec::new(),
        },
    );
    watcher.watch(std::slice::from_ref(&root)).unwrap();

    let target = root.join("main.ts");
    std::fs::write(&target, "export const value: number = 1;\n").unwrap();

    let clock = SystemClock::new();
    let deadline = Duration::from_secs(10);
    let mut seen = Vec::new();
    while clock.monotonic() < deadline {
        seen.extend(watcher.poll(clock.monotonic()));
        if !seen.is_empty() {
            break;
        }
        std::thread::yield_now();
    }

    assert!(
        !seen.is_empty(),
        "the watcher did not report {target} within the deadline"
    );
    assert!(
        seen.iter().any(|event| event.path == target),
        "no event named {target}: {seen:?}"
    );
}
