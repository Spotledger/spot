//! The watcher: debouncing, ignore rules and event normalisation (WP-DX-02.T1 deliverable 1).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use camino::Utf8PathBuf;
use spot_devserver::watch::{Op, RawEvent, VecEventSource, Watcher, WatcherConfig};

fn watcher() -> Watcher<VecEventSource> {
    Watcher::new(
        VecEventSource::new(),
        WatcherConfig {
            debounce: Duration::from_millis(100),
            extra_ignored: Vec::new(),
        },
    )
}

#[test]
fn a_change_is_emitted_only_after_the_debounce_window() {
    let mut watcher = watcher();
    watcher
        .source_mut()
        .push(RawEvent::new("/root/ts/main.ts", Op::Modified));
    assert!(watcher.poll(Duration::ZERO).is_empty());
    assert_eq!(watcher.pending_len(), 1);
    assert!(watcher.poll(Duration::from_millis(50)).is_empty());
    let events = watcher.poll(Duration::from_millis(100));
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].path, camino::Utf8Path::new("/root/ts/main.ts"));
    assert_eq!(watcher.pending_len(), 0);
}

#[test]
fn repeated_edits_to_one_file_coalesce_into_one_change() {
    let mut watcher = watcher();
    watcher
        .source_mut()
        .push(RawEvent::new("/root/Views/Form.toml", Op::Modified));
    watcher
        .source_mut()
        .push(RawEvent::new("/root/views/form.toml", Op::Modified));
    watcher
        .source_mut()
        .push(RawEvent::new("/root/views\\form.toml", Op::Modified));
    assert!(watcher.poll(Duration::ZERO).is_empty(), "ingest");
    let events = watcher.poll(Duration::from_millis(200));
    assert_eq!(events.len(), 1, "case and separator variants are one file");
}

#[test]
fn ignored_directories_and_editor_temp_files_never_reach_the_pipeline() {
    let mut watcher = watcher();
    for (path, op) in [
        ("/root/target/debug/app", Op::Modified),
        ("/root/node_modules/pkg/index.ts", Op::Created),
        ("/root/.git/HEAD", Op::Modified),
        ("/root/ts/.main.ts.swp", Op::Created),
        ("/root/ts/main.ts~", Op::Modified),
        ("/root/ts/4913", Op::Created),
    ] {
        watcher.source_mut().push(RawEvent::new(path, op));
    }
    assert!(watcher.poll(Duration::from_millis(500)).is_empty());
    assert_eq!(watcher.pending_len(), 0);
}

#[test]
fn an_editors_atomic_save_is_a_write_of_the_real_file() {
    let mut watcher = watcher();
    // vim writes `4913`, then the temp, then renames the temp onto the real file.
    watcher
        .source_mut()
        .push(RawEvent::new("/root/views/4913", Op::Created));
    watcher
        .source_mut()
        .push(RawEvent::new("/root/views/.form.toml.swp", Op::Created));
    watcher
        .source_mut()
        .push(RawEvent::new("/root/views/form.toml.tmp-4482", Op::Created));
    watcher
        .source_mut()
        .push(RawEvent::renamed("/root/views/form.toml"));
    assert!(watcher.poll(Duration::ZERO).is_empty(), "ingest");
    let events = watcher.poll(Duration::from_millis(200));
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0].path,
        camino::Utf8Path::new("/root/views/form.toml")
    );
    assert!(events[0].is_rename);
    assert_eq!(events[0].op, Op::Renamed);
}

#[test]
fn watch_records_the_roots_it_was_asked_for() {
    let mut watcher = watcher();
    let roots = vec![Utf8PathBuf::from("/root/app")];
    watcher.watch(&roots).unwrap();
    assert_eq!(watcher.source().watched(), roots.as_slice());
}
