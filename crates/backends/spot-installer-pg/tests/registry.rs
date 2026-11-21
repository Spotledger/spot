//! Behaviour the contract suite does not cover: failure injection between the registry write and
//! the journal write, a restart mid-run, site isolation, the upgrade race, purge and the strict
//! decoding of stored rows.
//!
//! Skips cleanly when no server is configured.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::{digest, install_change, installed, registry, row, site};
use semver::Version;
use spot_installer::{
    AppRegistryStore, DocTypeAccess, Endpoint, FileAccess, GrantedCapabilities, InstallJournal,
    InstalledApp, LoadedComponent, SiteLifecycle, SnapshotId, StepId, StepKind, StepState, Undo,
    UpgradeChange,
};
use spot_postgres::testing::TestDb;

fn capabilities() -> GrantedCapabilities {
    GrantedCapabilities {
        network: vec![Endpoint {
            host: "api.example.org".to_owned(),
            port: 443,
        }],
        email: true,
        files: FileAccess::ReadWrite,
        doctypes: DocTypeAccess {
            read: vec!["Customer".to_owned()],
            write: vec!["Sales Invoice".to_owned()],
        },
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_journal_failure_after_the_registry_write_leaves_neither() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = registry(&db, 4, "inject").await;
    let s = site("inject-site");
    // Fault injection: the journal insert for step 7 raises after the registry write ran.
    db.raw_client()
        .await
        .batch_execute(
            "CREATE FUNCTION fail_step_seven() RETURNS trigger AS $$ \
             BEGIN IF NEW.step_index = 7 THEN RAISE EXCEPTION 'injected'; END IF; RETURN NEW; END \
             $$ LANGUAGE plpgsql; \
             CREATE TRIGGER fail_step_seven BEFORE INSERT ON spot_installer_journal \
             FOR EACH ROW EXECUTE FUNCTION fail_step_seven();",
        )
        .await
        .unwrap();

    let installed_app = installed("a", 1);
    store
        .restore(&s, &installed_app, row("a", 1, StepState::Done))
        .await
        .unwrap();

    let install = store
        .commit_install(&s, "b", install_change(2), row("b", 7, StepState::Done))
        .await;
    assert!(install.is_err());
    let upgrade = store
        .commit_upgrade(
            &s,
            "a",
            UpgradeChange {
                from: Version::new(1, 0, 0),
                to: Version::new(2, 0, 0),
                digest: digest(9),
                approved: GrantedCapabilities::default(),
                applied_patches: Vec::new(),
            },
            row("a", 7, StepState::Done),
        )
        .await;
    assert!(upgrade.is_err());
    assert!(
        store
            .commit_uninstall(&s, "a", true, row("a", 7, StepState::Done))
            .await
            .is_err()
    );
    assert!(
        store
            .restore(&s, &installed("c", 3), row("c", 7, StepState::Undone))
            .await
            .is_err()
    );

    let state = store.load(&s).await.unwrap();
    let names: Vec<_> = state.apps.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(names, ["a"], "no failed commit may leave a registry change");
    assert_eq!(state.get("a"), Some(&installed_app), "`a` is untouched");
    let rows = store.journal(&s).await.unwrap();
    assert_eq!(rows.len(), 1, "no failed commit may leave a journal row");
    assert_eq!(rows[0].step.index, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restart_returns_the_same_journal_so_the_run_resumes() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let s = site("restart-site");
    let rich = InstallJournal::new(
        "hash-1",
        StepId::new(3, StepKind::Commit, "a"),
        StepState::Done,
        1_700_000_000,
        Some(Undo::Registry {
            app: "a".to_owned(),
            before: Some(Box::new(InstalledApp {
                name: "a".to_owned(),
                version: Version::parse("1.2.3-beta.1+build5").unwrap(),
                digest: digest(4),
                approved: capabilities(),
                applied_patches: vec!["p1".to_owned(), "p2".to_owned()],
                order: 2,
                depends: Vec::new(),
            })),
            after: Some(LoadedComponent::new(Version::new(2, 0, 0), digest(5))),
            doctypes: vec!["Invoice".to_owned()],
        }),
    );
    let others = [
        InstallJournal::new(
            "hash-1",
            StepId::new(0, StepKind::Snapshot, ""),
            StepState::Done,
            1,
            Some(Undo::Snapshot(SnapshotId::new("snap-1"))),
        ),
        InstallJournal::new(
            "hash-1",
            StepId::new(1, StepKind::Patch, "p1"),
            StepState::Started,
            2,
            Some(Undo::Patch {
                app: "a".to_owned(),
                patch_id: "p1".to_owned(),
                reversible: true,
            }),
        ),
        InstallJournal::new(
            "hash-1",
            StepId::new(2, StepKind::Fixture, "f.json"),
            StepState::Undone,
            3,
            Some(Undo::FixtureLoaded {
                app: "a".to_owned(),
                fixture: "f.json".to_owned(),
            }),
        ),
        InstallJournal::new(
            "hash-1",
            StepId::new(4, StepKind::Load, "a"),
            StepState::Done,
            4,
            Some(Undo::Component {
                app: "a".to_owned(),
                before: None,
            }),
        ),
        InstallJournal::new(
            "hash-1",
            StepId::new(5, StepKind::RemoveFixture, "g.json"),
            StepState::Done,
            5,
            Some(Undo::FixtureRemoved {
                app: "a".to_owned(),
                fixture: "g.json".to_owned(),
            }),
        ),
        row("a", 6, StepState::Done),
    ];

    let first = registry(&db, 2, "before_restart").await;
    first
        .set_lifecycle(&s, SiteLifecycle::Idle, SiteLifecycle::Installing)
        .await
        .unwrap();
    // Out of order on purpose: the journal comes back ordered by step index.
    first.record(&s, others[3].clone()).await.unwrap();
    first.record(&s, others[0].clone()).await.unwrap();
    first.record(&s, others[2].clone()).await.unwrap();
    first.record(&s, others[1].clone()).await.unwrap();
    first.record(&s, others[4].clone()).await.unwrap();
    first.record(&s, others[5].clone()).await.unwrap();
    first
        .commit_install(&s, "a", install_change(1), rich.clone())
        .await
        .unwrap();
    let before = first.journal(&s).await.unwrap();
    drop(first); // the pool goes with it: every connection is closed

    let second = registry(&db, 2, "after_restart").await;
    let after = second.journal(&s).await.unwrap();
    assert_eq!(before, after, "a restart returns the same journal");
    let indexes: Vec<_> = after.iter().map(|r| r.step.index).collect();
    assert_eq!(indexes, [0, 1, 2, 3, 4, 5, 6]);
    assert_eq!(after[3], rich, "undo data round-trips exactly");
    assert_eq!(
        second.lifecycle(&s).await.unwrap(),
        SiteLifecycle::Installing,
        "the lifecycle flag survives the restart"
    );
    assert_eq!(second.load(&s).await.unwrap().apps.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_sites_never_see_each_others_rows() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = registry(&db, 4, "isolation").await;
    let one = site("site-one");
    let two = site("site-two");

    store
        .commit_install(&one, "a", install_change(1), row("a", 0, StepState::Done))
        .await
        .unwrap();
    store
        .register_owned_doctypes(&one, "a", &["Invoice".to_owned()])
        .await
        .unwrap();
    store
        .set_lifecycle(&one, SiteLifecycle::Idle, SiteLifecycle::Installing)
        .await
        .unwrap();

    assert!(store.load(&two).await.unwrap().apps.is_empty());
    assert!(store.journal(&two).await.unwrap().is_empty());
    assert!(store.owned_doctypes(&two, "a").await.unwrap().is_empty());
    assert_eq!(store.lifecycle(&two).await.unwrap(), SiteLifecycle::Idle);

    // The same app and the same step index on the other site is a separate row.
    store
        .commit_install(
            &two,
            "a",
            install_change(2),
            row("a", 0, StepState::Started),
        )
        .await
        .unwrap();
    assert_eq!(
        store.load(&one).await.unwrap().get("a").unwrap().digest,
        digest(1)
    );
    assert_eq!(
        store.load(&two).await.unwrap().get("a").unwrap().digest,
        digest(2)
    );

    // Writes against one site change nothing on the other.
    store
        .commit_uninstall(&two, "a", true, row("a", 1, StepState::Done))
        .await
        .unwrap();
    store.clear_journal(&two).await.unwrap();
    assert_eq!(store.load(&one).await.unwrap().apps.len(), 1);
    assert_eq!(store.journal(&one).await.unwrap().len(), 1);
    assert_eq!(
        store.owned_doctypes(&one, "a").await.unwrap(),
        ["Invoice".to_owned()]
    );
    assert_eq!(
        store.lifecycle(&one).await.unwrap(),
        SiteLifecycle::Installing
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn racing_upgrades_apply_exactly_one() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = Arc::new(registry(&db, 8, "race").await);
    let s = site("race-site");
    store
        .commit_install(&s, "a", install_change(1), row("a", 0, StepState::Done))
        .await
        .unwrap();
    let mut tasks = Vec::new();
    for n in 0..6u8 {
        let store = Arc::clone(&store);
        let s = s.clone();
        tasks.push(tokio::spawn(async move {
            store
                .commit_upgrade(
                    &s,
                    "a",
                    UpgradeChange {
                        from: Version::new(1, 0, 0),
                        to: Version::new(2, u64::from(n), 0),
                        digest: digest(10 + n),
                        approved: GrantedCapabilities::default(),
                        applied_patches: Vec::new(),
                    },
                    row("a", 1 + u32::from(n), StepState::Done),
                )
                .await
        }));
    }
    let mut won = 0;
    for task in tasks {
        if task.await.unwrap().is_ok() {
            won += 1;
        }
    }
    assert_eq!(won, 1, "exactly one racing upgrade may apply");
    assert_eq!(
        store.journal(&s).await.unwrap().len(),
        2,
        "only the winner journals"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn racing_idle_to_installing_lets_exactly_one_win() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    // Two registries on two pools stand for two nodes.
    let nodes = [
        Arc::new(registry(&db, 4, "node-a").await),
        Arc::new(registry(&db, 4, "node-b").await),
    ];
    let s = site("lifecycle-race");
    let mut tasks = Vec::new();
    for i in 0..8 {
        let store = Arc::clone(&nodes[i % 2]);
        let s = s.clone();
        tasks.push(tokio::spawn(async move {
            store
                .transition_lifecycle(&s, SiteLifecycle::Idle, SiteLifecycle::Installing)
                .await
        }));
    }
    let mut won = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(()) => won += 1,
            Err(error) => {
                assert_eq!(error.code, "installer.lease_held");
                assert!(error.message.contains("installing"), "{}", error.message);
                assert!(error.message.contains("idle"), "{}", error.message);
            }
        }
    }
    assert_eq!(won, 1, "exactly one node may start the install");
    assert_eq!(
        nodes[0].lifecycle(&s).await.unwrap(),
        SiteLifecycle::Installing
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lifecycle_move_from_the_wrong_state_changes_nothing() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = registry(&db, 4, "lifecycle-wrong").await;
    let s = site("lifecycle-wrong-site");
    // Never touched: reads as idle, so moving "from installing" is refused.
    let error = store
        .transition_lifecycle(&s, SiteLifecycle::Installing, SiteLifecycle::Idle)
        .await
        .unwrap_err();
    assert_eq!(error.code, "installer.step_failed");
    assert_eq!(store.lifecycle(&s).await.unwrap(), SiteLifecycle::Idle);

    store
        .transition_lifecycle(&s, SiteLifecycle::Idle, SiteLifecycle::Installing)
        .await
        .unwrap();
    store
        .transition_lifecycle(&s, SiteLifecycle::Installing, SiteLifecycle::Idle)
        .await
        .unwrap();
    assert_eq!(store.lifecycle(&s).await.unwrap(), SiteLifecycle::Idle);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_commit_installs_get_distinct_ords() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = Arc::new(registry(&db, 8, "ord-race").await);
    let s = site("ord-race-site");
    let n: u8 = 8;
    let mut tasks = Vec::new();
    for i in 0..n {
        let store = Arc::clone(&store);
        let s = s.clone();
        tasks.push(tokio::spawn(async move {
            let app = format!("app{i}");
            store
                .commit_install(
                    &s,
                    &app,
                    install_change(i + 1),
                    row(&app, u32::from(i), StepState::Done),
                )
                .await
        }));
    }
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    let state = store.load(&s).await.unwrap();
    let mut ords: Vec<u32> = state.apps.iter().map(|app| app.order).collect();
    ords.sort_unstable();
    assert_eq!(
        ords,
        (0..u32::from(n)).collect::<Vec<_>>(),
        "every concurrent install gets its own ord, with no gap"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn ords_are_allocated_per_site() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = Arc::new(registry(&db, 8, "ord-sites").await);
    let mut tasks = Vec::new();
    for i in 0..6u8 {
        let store = Arc::clone(&store);
        tasks.push(tokio::spawn(async move {
            let s = site(&format!("ord-site-{}", i % 2));
            let app = format!("app{i}");
            store
                .commit_install(
                    &s,
                    &app,
                    install_change(i + 1),
                    row(&app, u32::from(i), StepState::Done),
                )
                .await
        }));
    }
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    for name in ["ord-site-0", "ord-site-1"] {
        let state = store.load(&site(name)).await.unwrap();
        let mut ords: Vec<u32> = state.apps.iter().map(|app| app.order).collect();
        ords.sort_unstable();
        assert_eq!(ords, vec![0, 1, 2], "site {name}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn upgrade_replaces_capabilities_and_patches() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = registry(&db, 4, "upgrade").await;
    let s = site("upgrade-site");
    store
        .commit_install(&s, "a", install_change(1), row("a", 0, StepState::Done))
        .await
        .unwrap();
    store
        .commit_upgrade(
            &s,
            "a",
            UpgradeChange {
                from: Version::new(1, 0, 0),
                to: Version::new(1, 1, 0),
                digest: digest(2),
                approved: capabilities(),
                applied_patches: vec!["p1".to_owned()],
            },
            row("a", 1, StepState::Done),
        )
        .await
        .unwrap();
    let state = store.load(&s).await.unwrap();
    let app = state.get("a").unwrap();
    assert_eq!(app.version, Version::new(1, 1, 0));
    assert_eq!(app.digest, digest(2));
    assert_eq!(app.approved, capabilities());
    assert_eq!(app.applied_patches, ["p1".to_owned()]);
    // Upgrading an app that is not installed is refused, not created.
    let missing = store
        .commit_upgrade(
            &s,
            "ghost",
            UpgradeChange {
                from: Version::new(1, 0, 0),
                to: Version::new(2, 0, 0),
                digest: digest(3),
                approved: GrantedCapabilities::default(),
                applied_patches: Vec::new(),
            },
            row("ghost", 2, StepState::Done),
        )
        .await
        .unwrap_err();
    assert_eq!(missing.code, "installer.not_installed");
    assert_eq!(store.load(&s).await.unwrap().apps.len(), 1);
    assert_eq!(store.journal(&s).await.unwrap().len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn install_is_idempotent_for_the_same_bytes_and_refused_otherwise() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = registry(&db, 4, "install").await;
    let s = site("install-site");
    store
        .commit_install(&s, "a", install_change(1), row("a", 0, StepState::Started))
        .await
        .unwrap();
    store
        .commit_install(&s, "a", install_change(1), row("a", 0, StepState::Done))
        .await
        .unwrap();
    let rows = store.journal(&s).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].state, StepState::Done);
    let refused = store
        .commit_install(&s, "a", install_change(2), row("a", 1, StepState::Done))
        .await
        .unwrap_err();
    assert_eq!(refused.code, "installer.already_installed");
    let state = store.load(&s).await.unwrap();
    assert_eq!(state.apps.len(), 1);
    assert_eq!(state.get("a").unwrap().digest, digest(1));
    assert_eq!(store.journal(&s).await.unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn install_order_follows_install_sequence() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = registry(&db, 4, "order").await;
    let s = site("order-site");
    for (i, app) in ["zeta", "alpha", "mid"].into_iter().enumerate() {
        store
            .commit_install(
                &s,
                app,
                install_change(1),
                row(app, u32::try_from(i).unwrap(), StepState::Done),
            )
            .await
            .unwrap();
    }
    let names: Vec<_> = store
        .load(&s)
        .await
        .unwrap()
        .apps
        .iter()
        .map(|a| (a.name.clone(), a.order))
        .collect();
    assert_eq!(
        names,
        [
            ("zeta".to_owned(), 0),
            ("alpha".to_owned(), 1),
            ("mid".to_owned(), 2)
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn uninstall_without_purge_keeps_ownership_and_with_purge_forgets_it() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = registry(&db, 4, "purge").await;
    let s = site("purge-site");
    for app in ["keep", "purge"] {
        store
            .commit_install(&s, app, install_change(1), row(app, 0, StepState::Done))
            .await
            .unwrap();
        store
            .register_owned_doctypes(&s, app, &["Invoice".to_owned(), "Item".to_owned()])
            .await
            .unwrap();
        // Registering twice changes nothing.
        store
            .register_owned_doctypes(&s, app, &["Invoice".to_owned()])
            .await
            .unwrap();
    }
    assert_eq!(
        store.owned_doctypes(&s, "keep").await.unwrap(),
        ["Invoice".to_owned(), "Item".to_owned()]
    );
    store
        .commit_uninstall(&s, "keep", false, row("keep", 1, StepState::Done))
        .await
        .unwrap();
    store
        .commit_uninstall(&s, "purge", true, row("purge", 2, StepState::Done))
        .await
        .unwrap();
    assert_eq!(store.owned_doctypes(&s, "keep").await.unwrap().len(), 2);
    assert!(store.owned_doctypes(&s, "purge").await.unwrap().is_empty());
    let again = store
        .commit_uninstall(&s, "keep", false, row("keep", 3, StepState::Done))
        .await
        .unwrap_err();
    assert_eq!(again.code, "installer.not_installed");
    assert_eq!(store.journal(&s).await.unwrap().len(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn restore_puts_back_every_field() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = registry(&db, 4, "restore").await;
    let s = site("restore-site");
    let app = InstalledApp {
        name: "a".to_owned(),
        version: Version::new(3, 1, 4),
        digest: digest(7),
        approved: capabilities(),
        applied_patches: vec!["x".to_owned(), "y".to_owned()],
        order: 5,
        depends: vec![spot_manifest_dependency("core", ">=1.0.0, <2.0.0")],
    };
    store
        .restore(&s, &app, row("a", 0, StepState::Undone))
        .await
        .unwrap();
    assert_eq!(store.load(&s).await.unwrap().get("a"), Some(&app));
    // Restoring over an existing row replaces it exactly.
    let changed = InstalledApp {
        version: Version::new(3, 2, 0),
        ..app.clone()
    };
    store
        .restore(&s, &changed, row("a", 1, StepState::Undone))
        .await
        .unwrap();
    assert_eq!(store.load(&s).await.unwrap().get("a"), Some(&changed));
}

fn spot_manifest_dependency(app: &str, req: &str) -> spot_manifest::AppDependency {
    spot_manifest::AppDependency {
        app: spot_storage_contract::Name::new(app).unwrap(),
        version: semver::VersionReq::parse(req).unwrap(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_damaged_row_is_an_error_not_a_default() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = registry(&db, 4, "corrupt").await;
    let s = site("corrupt-site");
    store
        .commit_install(&s, "a", install_change(1), row("a", 0, StepState::Done))
        .await
        .unwrap();
    db.raw_client()
        .await
        .batch_execute("UPDATE spot_installer_app SET version = 'not-a-version'")
        .await
        .unwrap();
    let error = store.load(&s).await.unwrap_err();
    assert_eq!(error.code, "installer_pg.corrupt_row");

    db.raw_client()
        .await
        .batch_execute("UPDATE spot_installer_journal SET undo = '{\"tag\": \"bogus\"}'::jsonb")
        .await
        .unwrap();
    let error = store.journal(&s).await.unwrap_err();
    assert_eq!(error.code, "installer_pg.corrupt_row");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_database_rejects_what_the_code_would_never_write() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let _store = registry(&db, 2, "constraints").await;
    let client = db.raw_client().await;
    let bad_digest = client
        .execute(
            "INSERT INTO spot_installer_app VALUES ('s', 'a', '1.0.0', 'XYZ', '{}', '[]', 0, '[]')",
            &[],
        )
        .await;
    assert!(bad_digest.is_err(), "the digest column holds only hex");
    let bad_flag = client
        .execute("INSERT INTO spot_installer_site VALUES ('s', 'weird')", &[])
        .await;
    assert!(bad_flag.is_err());
    // A step index beyond the column is refused before any statement runs.
    let store = registry(&db, 2, "constraints2").await;
    let too_big = store
        .record(&site("s"), row("a", u32::MAX, StepState::Done))
        .await
        .unwrap_err();
    assert_eq!(too_big.code, "installer_pg.corrupt_row");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn prepare_is_idempotent_and_safe_to_race() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let mut tasks = Vec::new();
    for n in 0..4 {
        let pool = common::pool(&db, 2, &format!("prepare{n}"));
        tasks.push(tokio::spawn(async move {
            spot_installer_pg::schema::prepare(&pool).await
        }));
    }
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    let store = registry(&db, 2, "prepare_again").await;
    store.prepare().await.unwrap();
}
