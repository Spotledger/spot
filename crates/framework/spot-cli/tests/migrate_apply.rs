//! `spot migrate-apply` and the `require_plan` setting (WP-MIG-07.T3): argument handling, the
//! per-site flags, and over a real host (`SPOT_TEST_PG_URL`; skipped with a printed reason when
//! unset) the legacy `migrate --apply` gate and the maintenance gate. The apply engine itself is
//! proved in `spot-migration-apply(-pg)` and the verb in `spot-migration-ops`.
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
fn the_command_parses_apply_resume_and_maintenance() {
    let command = commands::migrate_apply::command();
    let apply = command
        .clone()
        .try_get_matches_from([
            "migrate-apply",
            "apply",
            &"a".repeat(64),
            "--site",
            "alpha",
            "--resume",
        ])
        .unwrap();
    let (name, sub) = apply.subcommand().unwrap();
    assert_eq!(name, "apply");
    assert!(sub.get_flag("resume"));
    assert!(
        command
            .clone()
            .try_get_matches_from(["migrate-apply", "apply", "abc"])
            .is_err(),
        "--site is required"
    );
    for verb in ["enter", "leave"] {
        assert!(
            command
                .clone()
                .try_get_matches_from(["migrate-apply", "maintenance", verb, "--site", "a"])
                .is_ok()
        );
    }
    assert!(command.try_get_matches_from(["migrate-apply"]).is_err());
}

#[test]
fn the_site_flags_default_to_off_and_an_unreadable_file_turns_them_on() {
    let root = std::env::temp_dir().join(format!("spot_migrate_apply_{}", std::process::id()));
    let dir = root.join("sites").join("alpha");
    std::fs::create_dir_all(&dir).unwrap();
    assert!(!spot_cli::ops::requires_plan(&root, "alpha"));
    assert!(!spot_cli::ops::requires_rehearsal(&root, "alpha"));
    std::fs::write(dir.join("migrations.toml"), "require_plan = true\n").unwrap();
    assert!(spot_cli::ops::requires_plan(&root, "alpha"));
    assert!(!spot_cli::ops::requires_rehearsal(&root, "alpha"));
    std::fs::write(dir.join("migrations.toml"), "require_rehearsal = true\n").unwrap();
    assert!(!spot_cli::ops::requires_plan(&root, "alpha"));
    assert!(spot_cli::ops::requires_rehearsal(&root, "alpha"));
    std::fs::write(dir.join("migrations.toml"), "require_plan = [").unwrap();
    assert!(
        spot_cli::ops::requires_plan(&root, "alpha"),
        "a site that cannot say it is exempt must use the lifecycle"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "multi_thread")]
async fn legacy_apply_works_until_the_site_requires_a_plan() {
    let Some(env) = common::Env::new("").await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let root = env.root.0.clone();
    let _secret = env.site();
    // Not required: the STO-11 path still works.
    let ok = common::run(&root, &["migrate", "--site", &env.site, "--apply"], None);
    assert!(
        ok.status.success(),
        "{}",
        String::from_utf8_lossy(&ok.stderr)
    );
    // Required: refused, naming the way forward, and nothing is changed.
    std::fs::write(
        env.site_dir().join("migrations.toml"),
        "require_plan = true\n",
    )
    .unwrap();
    let refused = common::run(&root, &["migrate", "--site", &env.site, "--apply"], None);
    assert!(!refused.status.success());
    let text = String::from_utf8_lossy(&refused.stderr);
    assert!(text.contains("migration.not_approved"), "{text}");
    assert!(text.contains("migrate-apply"), "{text}");
    // A plain plan (no --apply) is still allowed.
    let plan = common::run(&root, &["migrate", "--site", &env.site], None);
    assert!(plan.status.success());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_maintenance_gate_closes_and_opens_from_the_command() {
    let Some(env) = common::Env::new("").await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let root = env.root.0.clone();
    let _secret = env.site();
    let closed = common::ok(
        &root,
        &[
            "migrate-apply",
            "maintenance",
            "enter",
            "--site",
            &env.site,
            "--reason",
            "test",
        ],
    );
    assert!(closed.contains("closed"), "{closed}");
    let open = common::ok(
        &root,
        &["migrate-apply", "maintenance", "leave", "--site", &env.site],
    );
    assert!(open.contains("open"), "{open}");
    // An unknown plan is refused with exit 2 and the site is not touched.
    let refused = common::run(
        &root,
        &[
            "migrate-apply",
            "apply",
            &"a".repeat(64),
            "--site",
            &env.site,
        ],
        None,
    );
    assert_eq!(refused.status.code(), Some(2));
}
