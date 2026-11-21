//! The vendor-side intake: signatures, grouping with a count, rate limits and the sinks.
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::cell::RefCell;
use std::rc::Rc;

use spot_unattended_support::error::SupportError;
use spot_unattended_support::intake::{
    CrashIntake, IntakeConfig, IntakeOutcome, JsonFileSink, Sink, SymbolLookup, WorkItem, signature,
};

#[derive(Clone, Default)]
struct Memory(Rc<RefCell<Vec<WorkItem>>>);

impl Sink for Memory {
    fn upsert(&mut self, item: &WorkItem) -> Result<(), SupportError> {
        self.0.borrow_mut().push(item.clone());
        Ok(())
    }
}

fn crash(location: &str) -> spot_unattended_support::CrashReport {
    common::report(
        location,
        "1.4.2",
        &["app::run", "app::step", "core::panicking::panic"],
    )
}

#[test]
fn duplicate_crashes_group_into_one_item_with_a_count() {
    let sink = Memory::default();
    let mut intake = CrashIntake::new(sink.clone(), IntakeConfig::default());
    let first = intake.ingest(&crash("src/a.rs:10"), 1_000).expect("ok");
    let IntakeOutcome::Filed { signature: sig } = first else {
        panic!("first report is filed, got {first:?}");
    };
    let mut counts = Vec::new();
    for i in 1..=5 {
        match intake.ingest(&crash("src/a.rs:10"), 1_000 + i).expect("ok") {
            IntakeOutcome::Grouped { signature, count } => {
                assert_eq!(signature, sig);
                counts.push(count);
            }
            other => panic!("expected a group, got {other:?}"),
        }
    }
    assert_eq!(counts, [2, 3, 4, 5, 6]);
    let items = intake.items();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].count, 6);
    assert_eq!((items[0].first_seen, items[0].last_seen), (1_000, 1_005));
    // The sink saw one filing, not six: updates are paced by min_update_secs.
    assert_eq!(sink.0.borrow().len(), 1);
    // A flush pushes the new count.
    assert_eq!(intake.flush(1_010).expect("flushed"), 1);
    assert_eq!(sink.0.borrow().last().expect("pushed").count, 6);
    assert_eq!(intake.flush(1_011).expect("nothing dirty"), 0);
}

#[test]
fn different_locations_and_versions_are_different_items() {
    let mut intake = CrashIntake::new(Memory::default(), IntakeConfig::default());
    intake.ingest(&crash("src/a.rs:10"), 1).expect("ok");
    intake.ingest(&crash("src/b.rs:10"), 2).expect("ok");
    let mut newer = crash("src/a.rs:10");
    newer.version = "1.5.0".to_owned();
    intake.ingest(&newer, 3).expect("ok");
    assert_eq!(intake.items().len(), 3);
}

#[test]
fn addresses_and_symbol_hashes_do_not_split_a_group() {
    let a = common::report(
        "src/a.rs:1",
        "1.0.0",
        &["0x7f00aa app::run::h0123456789abcdef", "core::panic"],
    );
    let b = common::report(
        "src/a.rs:1",
        "1.0.0",
        &["0x7f11bb app::run::hfedcba9876543210", "core::panic"],
    );
    assert_eq!(signature(&a), signature(&b));
    let c = common::report("src/a.rs:1", "1.0.0", &["app::other", "core::panic"]);
    assert_ne!(signature(&a), signature(&c));
    // Frames below the top five do not matter.
    let deep_a = common::report("l:1", "1", &["1", "2", "3", "4", "5", "tail-a"]);
    let deep_b = common::report("l:1", "1", &["1", "2", "3", "4", "5", "tail-b"]);
    assert_eq!(signature(&deep_a), signature(&deep_b));
}

#[test]
fn a_crash_storm_is_rate_limited_and_filed_later() {
    let sink = Memory::default();
    let config = IntakeConfig {
        window_secs: 100,
        max_new_per_window: 2,
        min_update_secs: 10,
        ..IntakeConfig::default()
    };
    let mut intake = CrashIntake::new(sink.clone(), config);
    let mut outcomes = Vec::new();
    for i in 0..4 {
        outcomes.push(
            intake
                .ingest(&crash(&format!("src/s.rs:{i}")), 10)
                .expect("ok"),
        );
    }
    let filed = outcomes
        .iter()
        .filter(|o| matches!(o, IntakeOutcome::Filed { .. }))
        .count();
    let limited = outcomes
        .iter()
        .filter(|o| matches!(o, IntakeOutcome::RateLimited { .. }))
        .count();
    assert_eq!((filed, limited), (2, 2));
    assert_eq!(sink.0.borrow().len(), 2);
    // A limited group is counted, not lost.
    let again = intake.ingest(&crash("src/s.rs:3"), 11).expect("ok");
    assert!(matches!(again, IntakeOutcome::RateLimited { .. }));
    assert_eq!(intake.items().iter().map(|i| i.count).sum::<u64>(), 5);
    // The next window files them.
    assert_eq!(intake.flush(200).expect("flushed"), 2);
    assert_eq!(sink.0.borrow().len(), 4);
}

#[test]
fn updates_are_paced_per_item() {
    let sink = Memory::default();
    let config = IntakeConfig {
        window_secs: 3_600,
        max_new_per_window: 10,
        min_update_secs: 60,
        ..IntakeConfig::default()
    };
    let mut intake = CrashIntake::new(sink.clone(), config);
    intake.ingest(&crash("src/a.rs:1"), 0).expect("ok");
    intake.ingest(&crash("src/a.rs:1"), 30).expect("ok");
    assert_eq!(sink.0.borrow().len(), 1);
    intake.ingest(&crash("src/a.rs:1"), 61).expect("ok");
    assert_eq!(sink.0.borrow().len(), 2);
    assert_eq!(sink.0.borrow()[1].count, 3);
}

#[test]
fn the_symbol_hook_and_versions_are_recorded() {
    struct Symbols;
    impl SymbolLookup for Symbols {
        fn lookup(&self, build_id: &str) -> Option<String> {
            (build_id == "abc123").then(|| "symbols/abc123".to_owned())
        }
    }
    let mut intake = CrashIntake::with_symbols(Memory::default(), Symbols, IntakeConfig::default());
    intake.ingest(&crash("src/a.rs:1"), 1).expect("ok");
    let mut other = crash("src/a.rs:1");
    other.version = "1.4.3".to_owned();
    // A different version is a different signature, so it has its own item and symbols.
    intake.ingest(&other, 2).expect("ok");
    let items = intake.items();
    assert!(
        items
            .iter()
            .all(|i| i.symbols.as_deref() == Some("symbols/abc123"))
    );
    assert!(items.iter().any(|i| i.versions == ["1.4.2"]));
}

#[test]
fn the_default_sink_writes_one_json_file_per_signature() {
    let dir = common::temp_dir("sink");
    let mut intake = CrashIntake::new(JsonFileSink::new(dir.join("out")), IntakeConfig::default());
    let IntakeOutcome::Filed { signature } = intake.ingest(&crash("src/a.rs:10"), 1).expect("ok")
    else {
        panic!("filed");
    };
    intake.ingest(&crash("src/a.rs:10"), 2).expect("ok");
    intake.flush(3).expect("flushed");
    let text =
        std::fs::read_to_string(dir.join("out").join(format!("{signature}.json"))).expect("file");
    let item: WorkItem = serde_json::from_str(&text).expect("json");
    assert_eq!(item.count, 2);
    assert_eq!(std::fs::read_dir(dir.join("out")).expect("dir").count(), 1);
}

#[test]
fn a_failing_sink_keeps_the_group_for_the_next_flush() {
    struct Flaky(Rc<RefCell<u32>>);
    impl Sink for Flaky {
        fn upsert(&mut self, _: &WorkItem) -> Result<(), SupportError> {
            let mut n = self.0.borrow_mut();
            *n += 1;
            if *n == 1 {
                Err(SupportError::new("unattended_support.sink_failed", "down"))
            } else {
                Ok(())
            }
        }
    }
    let calls = Rc::new(RefCell::new(0));
    let mut intake = CrashIntake::new(Flaky(calls.clone()), IntakeConfig::default());
    assert!(intake.ingest(&crash("src/a.rs:1"), 1).is_err());
    assert_eq!(intake.items().len(), 1, "the group is kept");
    assert_eq!(intake.flush(2).expect("retried"), 1);
}

fn config(max_groups: usize) -> IntakeConfig {
    IntakeConfig {
        max_groups,
        ..IntakeConfig::default()
    }
}

#[test]
fn invalid_reports_are_refused_before_anything_is_stored() {
    let mut intake = CrashIntake::new(Memory::default(), IntakeConfig::default());
    let mut cases: Vec<(&str, spot_unattended_support::CrashReport)> = Vec::new();
    let mut r = crash("src/a.rs:1");
    r.panic_location = "x".repeat(10_000);
    cases.push(("long line", r));
    let mut r = crash("src/a.rs:1");
    r.backtrace = (0..65).map(|i| format!("f{i}")).collect();
    cases.push(("many frames", r));
    let mut r = crash("src/a.rs:1");
    r.breadcrumbs = (0..51).map(|i| format!("b{i}")).collect();
    cases.push(("many breadcrumbs", r));
    let mut r = crash("src/a.rs:1");
    r.config_summary = (0..65).map(|i| (format!("k{i}"), "v".to_owned())).collect();
    cases.push(("many config entries", r));
    let mut r = crash("src/a.rs:1");
    r.format = 99;
    cases.push(("format", r));
    // Within every per-field cap, but over the byte cap as a whole.
    let mut r = crash("src/a.rs:1");
    r.backtrace = (0..64).map(|_| "y".repeat(500)).collect();
    r.breadcrumbs = (0..50).map(|_| "z".repeat(500)).collect();
    r.config_summary = (0..64)
        .map(|i| (format!("k{i}"), "v".repeat(500)))
        .collect();
    cases.push(("total size", r));
    for (label, report) in &cases {
        let err = intake.ingest(report, 1).expect_err(label);
        assert_eq!(err.code(), "unattended_support.report_invalid", "{label}");
    }
    assert!(intake.items().is_empty());
    assert_eq!(intake.stats().invalid, cases.len() as u64);
    // A report as the collector builds it is valid.
    let built = spot_unattended_support::CrashCollector::new(common::context(), common::scrubber())
        .build("src/a.rs:1", &["f".repeat(5_000)], 1);
    assert!(built.validate().is_ok());
    assert!(intake.ingest(&built, 2).is_ok());
}

#[test]
fn the_group_map_is_bounded_and_every_drop_is_counted() {
    let sink = Memory::default();
    let storm = IntakeConfig {
        max_new_per_window: 1,
        ..config(3)
    };
    let mut intake = CrashIntake::new(sink, storm);
    // One group is filed; the next distinct ones wait behind the rate limit.
    for i in 0..3 {
        intake
            .ingest(&crash(&format!("src/s.rs:{i}")), 10 + i)
            .expect("ok");
    }
    assert_eq!(intake.items().len(), 3);
    // At the limit a new signature evicts the oldest group that was never filed.
    let out = intake.ingest(&crash("src/s.rs:99"), 20).expect("ok");
    assert!(matches!(out, IntakeOutcome::RateLimited { .. }), "{out:?}");
    assert_eq!(intake.items().len(), 3);
    assert_eq!(intake.stats().evicted, 1);
    assert!(
        intake
            .items()
            .iter()
            .all(|i| i.sample.panic_location != "src/s.rs:1")
    );
    assert!(
        intake
            .items()
            .iter()
            .any(|i| i.sample.panic_location == "src/s.rs:0"),
        "the filed group stays"
    );
    // When every held group is filed, a new signature is refused and counted.
    let sink = Memory::default();
    let mut intake = CrashIntake::new(sink, config(2));
    intake.ingest(&crash("src/f.rs:1"), 1).expect("ok");
    intake.ingest(&crash("src/f.rs:2"), 2).expect("ok");
    let out = intake.ingest(&crash("src/f.rs:3"), 3).expect("ok");
    assert!(matches!(out, IntakeOutcome::Refused { .. }), "{out:?}");
    assert_eq!(intake.items().len(), 2);
    assert_eq!(intake.stats().refused, 1);
    // Known signatures still group at the limit.
    let out = intake.ingest(&crash("src/f.rs:1"), 4).expect("ok");
    assert!(matches!(out, IntakeOutcome::Grouped { count: 2, .. }));
}

#[cfg(feature = "github")]
mod github_hostile {
    use super::*;
    use spot_unattended_support::intake::github::{GithubSink, HttpPost, fence_for, safe_title};

    #[derive(Default)]
    struct Capture(Rc<RefCell<Vec<(String, String)>>>);

    impl HttpPost for Capture {
        fn post(&mut self, path: &str, body: &str) -> Result<(u16, String), String> {
            self.0.borrow_mut().push((path.to_owned(), body.to_owned()));
            Ok((201, "{\"number\": 1}".to_owned()))
        }
    }

    #[test]
    fn a_hostile_location_and_frame_cannot_inject_markdown() {
        let hostile = "x`\n```\n![img](http://evil.example/a.png) [link](http://evil.example) @everyone #1\n```````";
        let mut report = common::report(hostile, "1.0.0", &[hostile, "core::panic"]);
        report.breadcrumbs = vec![hostile.to_owned()];
        let capture = Capture::default();
        let log = capture.0.clone();
        let mut intake = CrashIntake::new(GithubSink::new("o/r", capture), IntakeConfig::default());
        intake.ingest(&report, 1).expect("ok");
        let posted: serde_json::Value = serde_json::from_str(&log.borrow()[0].1).expect("json");
        let title = posted["title"].as_str().expect("title");
        for bad in ['\n', '\r', '`', '@', '[', ']', '!', '<', '>', '#'] {
            assert!(!title.contains(bad), "title has {bad:?}: {title}");
        }
        assert!(title.len() <= 125);
        let body = posted["body"].as_str().expect("body");
        let fence_line = body
            .lines()
            .find(|l| l.starts_with("`") && l.ends_with("json"))
            .expect("opening fence");
        let fence = fence_line.trim_end_matches("json");
        assert!(
            fence.len() > 7,
            "the fence must outgrow the longest backtick run: {fence}"
        );
        // The fence closes exactly once, at the end.
        assert_eq!(body.lines().filter(|l| *l == fence).count(), 1);
        assert!(body.ends_with(fence));
    }

    #[test]
    fn fences_and_titles_are_built_safely() {
        assert_eq!(fence_for("no ticks"), "```");
        assert_eq!(fence_for("a ``` b"), "````");
        assert_eq!(fence_for("``````"), "```````");
        assert_eq!(
            safe_title("Crash at src/a.rs:10 (1.0)"),
            "Crash at src/a.rs:10 (1.0)"
        );
        assert!(safe_title(&"a".repeat(500)).len() <= 120);
    }
}

#[cfg(feature = "github")]
mod github {
    use super::*;
    use spot_unattended_support::intake::github::{GithubSink, HttpPost};

    #[derive(Default)]
    struct Fake(Rc<RefCell<Vec<(String, String)>>>);

    impl HttpPost for Fake {
        fn post(&mut self, path: &str, body: &str) -> Result<(u16, String), String> {
            self.0.borrow_mut().push((path.to_owned(), body.to_owned()));
            Ok((201, "{\"number\": 77}".to_owned()))
        }
    }

    #[test]
    fn a_group_is_one_issue_and_later_counts_are_comments() {
        let fake = Fake::default();
        let log = fake.0.clone();
        let sink = GithubSink::new("owner/repo", fake);
        let config = IntakeConfig {
            window_secs: 100,
            max_new_per_window: 5,
            min_update_secs: 0,
            ..IntakeConfig::default()
        };
        let mut intake = CrashIntake::new(sink, config);
        intake.ingest(&crash("src/a.rs:1"), 1).expect("ok");
        intake.ingest(&crash("src/a.rs:1"), 2).expect("ok");
        let log = log.borrow();
        assert_eq!(log[0].0, "/repos/owner/repo/issues");
        assert!(log[0].1.contains("crash-report"));
        assert_eq!(log[1].0, "/repos/owner/repo/issues/77/comments");
        assert!(log[1].1.contains("Seen 2 times"));
    }
}
