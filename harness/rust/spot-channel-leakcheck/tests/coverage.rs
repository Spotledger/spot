//! The coverage guard: every `Channel` has a registered test (or an explicit pending entry),
//! the registered test exists by name in this crate's `tests/`, and the pending list is
//! printed so the lead can see it.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::path::Path;

use spot_channel_leakcheck::{CHANNELS, Coverage, coverage_of};
use spot_field_guard::Channel;

/// An exhaustive match: adding a `Channel` variant fails to compile here until it is added to
/// this list and to `CHANNELS`.
fn listed(channel: Channel) -> &'static str {
    match channel {
        Channel::Version => "version",
        Channel::Export => "export",
        Channel::Print => "print",
        Channel::Report => "report",
        Channel::ListColumn => "list_column",
        Channel::LinkTitle => "link_title",
        Channel::Search => "search",
        Channel::Notification => "notification",
        Channel::Journal => "journal",
        Channel::ApiScim => "api_scim",
    }
}

fn test_sources() -> String {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut all = String::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "rs") {
            all.push_str(&std::fs::read_to_string(path).unwrap());
            all.push('\n');
        }
    }
    all
}

#[test]
fn every_channel_is_registered_once() {
    let mut seen = BTreeSet::new();
    for channel in Channel::ALL {
        assert_eq!(listed(channel), channel.as_str(), "{channel:?}");
        assert!(
            coverage_of(channel).is_some(),
            "{channel:?} has no entry in CHANNELS: add a test or a pending entry"
        );
        assert!(seen.insert(channel), "{channel:?} registered twice");
    }
    assert_eq!(CHANNELS.len(), Channel::ALL.len());
}

#[test]
fn every_registered_test_exists_and_the_pending_list_is_printed() {
    let sources = test_sources();
    let mut pending = Vec::new();
    for (channel, coverage) in CHANNELS {
        match coverage {
            Coverage::Test { test, producer } => {
                assert!(
                    sources.contains(&format!("async fn {test}("))
                        || sources.contains(&format!("fn {test}(")),
                    "{channel:?}: the registered test `{test}` ({producer}) is not in tests/"
                );
            }
            Coverage::Pending { reason, task } => {
                assert!(!reason.is_empty() && !task.is_empty(), "{channel:?}");
                pending.push(format!("  {:?}: {reason} [{task}]", channel));
            }
        }
    }
    // The lead reads this in the test log (`cargo test -- --nocapture`).
    println!("channels pending an end-to-end test ({}):", pending.len());
    for line in &pending {
        println!("{line}");
    }
}

#[test]
fn a_covered_channel_names_a_producer_that_calls_the_guard() {
    // The producers wired to the guard today, as a dependency of the producer crate.
    for (channel, coverage) in CHANNELS {
        if let Coverage::Test { producer, .. } = coverage {
            let manifest = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../../crates/framework")
                .join(producer)
                .join("Cargo.toml");
            let text = std::fs::read_to_string(&manifest)
                .unwrap_or_else(|e| panic!("{channel:?}: {}: {e}", manifest.display()));
            assert!(
                text.contains("spot-field-guard"),
                "{channel:?}: `{producer}` does not depend on spot-field-guard"
            );
        }
    }
}
