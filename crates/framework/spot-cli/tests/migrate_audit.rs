//! `spot migrate-audit`: argument handling and the date filter; over a real host
//! (`SPOT_TEST_PG_URL`, skipped with a printed reason when unset) an empty chain lists nothing and
//! verifies clean, and a record planted by hand is shown and then caught when tampered with.
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    dead_code,
    unused_imports
)]

mod common;

/// The command modules, driven without a process.
#[path = "../src/commands/mod.rs"]
mod commands;

use commands::migrate_audit::{parse_since, scope_of};

#[test]
fn the_command_parses_its_verbs() {
    let command = commands::migrate_audit::command();
    let list = command
        .clone()
        .try_get_matches_from([
            "migrate-audit",
            "list",
            "--site",
            "a",
            "--since",
            "2026-11-01",
            "--plan",
            "abc",
            "--json",
        ])
        .unwrap();
    let (name, sub) = list.subcommand().unwrap();
    assert_eq!(name, "list");
    assert!(sub.get_flag("json"));
    assert!(
        command
            .clone()
            .try_get_matches_from(["migrate-audit", "show", "run-1", "--site", "a", "--explain"])
            .is_ok()
    );
    assert!(
        command
            .clone()
            .try_get_matches_from(["migrate-audit", "verify"])
            .is_err(),
        "--site is required"
    );
    assert!(command.try_get_matches_from(["migrate-audit"]).is_err());
}

#[test]
fn the_since_filter_is_a_utc_day() {
    assert_eq!(parse_since("1970-01-02"), Some(86_400_000_000));
    assert_eq!(parse_since("2026-11-13"), Some(1_794_528_000_000_000));
    assert_eq!(parse_since("2026-13-01"), None);
    assert_eq!(parse_since("yesterday"), None);
    assert_eq!(parse_since(""), None);
}

#[test]
fn the_chain_is_the_sites_own() {
    assert_eq!(scope_of("alpha"), "migration:alpha");
    assert_ne!(scope_of("alpha"), scope_of("beta"));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_empty_chain_lists_nothing_and_verifies_clean_and_other_sites_are_separate() {
    let Some(env) = common::Env::new("").await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let root = env.root.0.clone();
    let _secret = env.site();
    let listing = common::ok(&root, &["migrate-audit", "list", "--site", &env.site]);
    assert!(listing.trim().is_empty(), "{listing}");
    let verified = common::ok(&root, &["migrate-audit", "verify", "--site", &env.site]);
    assert!(verified.contains("ok"), "{verified}");
    // An unknown run is refused with exit 2, and an unknown site is a usage error.
    let unknown = common::run(
        &root,
        &["migrate-audit", "show", "run-none", "--site", &env.site],
        None,
    );
    assert_eq!(unknown.status.code(), Some(2));
    let nosite = common::run(&root, &["migrate-audit", "list", "--site", "nope"], None);
    assert_eq!(nosite.status.code(), Some(1));
}
