//! `spot migrate-rescue`: argument handling; over a real host (`SPOT_TEST_PG_URL`, skipped with a
//! printed reason when unset) an unknown plan is refused with exit 2 and nothing is touched.
//! The two-person, token, gate and restore rules are proved in `spot-migration-ops` (fake host).
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

#[test]
fn the_command_parses_preview_and_confirm() {
    let command = commands::migrate_rescue::command();
    let preview = command
        .clone()
        .try_get_matches_from([
            "migrate-rescue",
            &"a".repeat(64),
            "--site",
            "alpha",
            "--mode",
            "restore",
        ])
        .unwrap();
    assert!(preview.get_one::<String>("confirm").is_none());
    let confirm = command
        .clone()
        .try_get_matches_from([
            "migrate-rescue",
            &"a".repeat(64),
            "--site",
            "alpha",
            "--mode",
            "down",
            "--confirm",
            "tok",
            "--approved-by",
            "bob",
        ])
        .unwrap();
    assert_eq!(confirm.get_one::<String>("approved_by").unwrap(), "bob");
    for args in [
        vec!["migrate-rescue", "abc", "--site", "a"],
        vec!["migrate-rescue", "abc", "--site", "a", "--mode", "sideways"],
        vec!["migrate-rescue", "--site", "a", "--mode", "down"],
    ] {
        assert!(command.clone().try_get_matches_from(args).is_err());
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_plan_is_refused_and_an_unknown_site_is_usage() {
    let Some(env) = common::Env::new("").await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let root = env.root.0.clone();
    let _secret = env.site();
    let refused = common::run(
        &root,
        &[
            "migrate-rescue",
            &"a".repeat(64),
            "--site",
            &env.site,
            "--mode",
            "restore",
        ],
        None,
    );
    assert_eq!(refused.status.code(), Some(2));
    let nosite = common::run(
        &root,
        &[
            "migrate-rescue",
            &"a".repeat(64),
            "--site",
            "nope",
            "--mode",
            "restore",
        ],
        None,
    );
    assert_eq!(nosite.status.code(), Some(1));
}
