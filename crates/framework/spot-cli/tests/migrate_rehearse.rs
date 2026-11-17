//! `spot migrate-rehearse`: argument handling and the scratch naming that keeps a rehearsal away
//! from the live database. The engine itself is proved in `spot-migration-ops` (fakes) and
//! `spot-rehearsal`; the PostgreSQL case needs `pg_dump` and skips with a printed reason without it.
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    dead_code,
    unused_imports
)]

use spot_migration_ops::PlanHash;

/// The command modules, driven without a process.
#[path = "../src/commands/mod.rs"]
mod commands;

use commands::migrate_rehearse::{SCRATCH_PREFIX, scratch_name, site_scratch_prefix};

#[test]
fn the_command_parses_run_and_clean() {
    let run = commands::migrate_rehearse::command()
        .try_get_matches_from([
            "migrate-rehearse",
            "run",
            &"a".repeat(64),
            "--site",
            "alpha",
            "--budget-minutes",
            "5",
            "--json",
        ])
        .unwrap();
    let (name, sub) = run.subcommand().unwrap();
    assert_eq!(name, "run");
    assert_eq!(*sub.get_one::<u64>("budget_minutes").unwrap(), 5);
    assert!(sub.get_flag("json"));
    assert!(
        commands::migrate_rehearse::command()
            .try_get_matches_from(["migrate-rehearse", "run", "abc"])
            .is_err(),
        "--site is required"
    );
    assert!(
        commands::migrate_rehearse::command()
            .try_get_matches_from([
                "migrate-rehearse",
                "run",
                "abc",
                "--site",
                "a",
                "--budget-minutes",
                "0"
            ])
            .is_err(),
        "a zero budget is refused"
    );
    assert!(
        commands::migrate_rehearse::command()
            .try_get_matches_from(["migrate-rehearse", "clean", "--site", "alpha"])
            .is_ok()
    );
}

#[test]
fn the_scratch_is_named_from_the_site_the_plan_and_the_clock_and_is_never_the_live_name() {
    let hash = PlanHash::parse(&format!("{}{}", "abcdef012345", "0".repeat(52))).unwrap();
    let name = scratch_name("Head-Office", &hash, 1_700_000_000);
    assert_eq!(name, "spot_rehearsal_head_office_abcdef012345_1700000000");
    assert!(name.starts_with(SCRATCH_PREFIX));
    assert!(name.starts_with(&site_scratch_prefix("Head-Office")));
    assert!(
        name.len() <= 63,
        "a PostgreSQL identifier is at most 63 bytes"
    );
    // Another clock reading is another database, so two rehearsals never share one.
    assert_ne!(name, scratch_name("Head-Office", &hash, 1_700_000_001));
    // The live database name (a plain tenant name) can never collide with the prefix.
    assert_ne!(scratch_name("alpha", &hash, 1), "alpha");
}
