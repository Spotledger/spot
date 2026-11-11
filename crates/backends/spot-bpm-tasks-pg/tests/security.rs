//! The schema and the security properties: the migrator accepts the tables, the audit trail is
//! append-only, statements take input as data, a backend failure or a corrupt row is an error
//! (never an empty result), errors carry no content, and a refused write leaves nothing behind.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_bpm_tasks::{TaskAction, TaskEvent, TaskFilter, TaskStatus, TaskStore, TasksError};
use spot_bpm_tasks_pg::{BAD_SETUP, MIGRATION_FAILED};
use spot_pg_migrate::{LiveSchema, plan};
use spot_postgres::PgBackend;
use spot_storage_contract::{Catalog, Collation};

#[test]
fn setup_codes_are_catalogued() {
    let catalogue = spot_errors::Catalogue::load();
    assert_eq!(catalogue.load_error(), None);
    for code in [BAD_SETUP, MIGRATION_FAILED] {
        assert!(
            catalogue.lookup(code).is_some(),
            "no catalogue entry for `{code}`"
        );
    }
}

#[test]
fn prepare_is_idempotent_and_the_migrator_accepts_the_tables() {
    let Some(fx) = fixture() else { return };
    let (_bridge, store) = open_store(fx.db.config(), "acme");
    store.prepare().unwrap();
    store.prepare().unwrap();
    let refused = fx.rt.block_on(async {
        let backend = PgBackend::connect(fx.db.config()).await.unwrap();
        let live = LiveSchema::read(&backend).await.unwrap();
        let wanted = Catalog::new(1, Collation::new("en").unwrap(), vec![]).unwrap();
        plan(&live, &wanted).refused.len()
    });
    assert_eq!(
        refused, 0,
        "the migrator refuses nothing: every table carries the spot_ prefix"
    );
    let names: Vec<String> = fx.rt.block_on(async {
        let client = fx.db.raw_client().await;
        client
            .query(
                "SELECT indexname::text FROM pg_indexes WHERE tablename LIKE 'spot_bpm_task%' \
                 ORDER BY indexname",
                &[],
            )
            .await
            .unwrap()
            .iter()
            .map(|r| r.get(0))
            .collect()
    });
    for wanted in [
        "spot_bpm_task_assignees_idx",
        "spot_bpm_task_due_idx",
        "spot_bpm_task_node_idx",
        "spot_bpm_task_roles_idx",
        "spot_bpm_task_status_idx",
        "spot_bpm_task_pkey",
        "spot_bpm_task_event_pkey",
    ] {
        assert!(
            names.iter().any(|n| n == wanted),
            "missing {wanted} in {names:?}"
        );
    }
}

#[test]
fn the_event_table_refuses_update_delete_and_truncate() {
    let Some(fx) = fixture() else { return };
    let (_bridge, store) = open_store(fx.db.config(), "acme");
    let t = task("t_audit", "case1", "approve", &["alice"]);
    store.create(t.clone(), created(&t)).unwrap();
    fx.rt.block_on(async {
        let client = fx.db.raw_client().await;
        for statement in [
            "UPDATE spot_bpm_task_event SET actor = 'mallory'",
            "UPDATE spot_bpm_task_event SET body = '{}'::jsonb",
            "DELETE FROM spot_bpm_task_event",
            "TRUNCATE spot_bpm_task_event",
        ] {
            let err = client.batch_execute(statement).await.unwrap_err();
            let text = err
                .as_db_error()
                .map(|d| d.message().to_owned())
                .unwrap_or_default();
            assert!(text.contains("append-only"), "{statement}: {text}");
        }
        // A task row cannot be deleted from under its events either.
        assert!(
            client
                .batch_execute("DELETE FROM spot_bpm_task")
                .await
                .is_err()
        );
    });
    let trail = store.events(&t.id).unwrap();
    assert_eq!(trail.len(), 1);
    assert_eq!(trail[0].actor, "system:engine");
}

#[test]
fn input_is_data_never_sql() {
    let Some(fx) = fixture() else { return };
    let (_bridge, store) = open_store(fx.db.config(), "acme");
    let nasty = "x'); DROP TABLE spot_bpm_task; --";
    let mut t = task("t_inj", nasty, "n'\"ode", &["al'ice", nasty]);
    t.title = nasty.to_owned();
    t.process = nasty.to_owned();
    let event = TaskEvent::new(&t.id, TaskAction::Created, nasty, now()).with_reason(nasty);
    let stored = store.create(t.clone(), event).unwrap();
    assert_eq!(stored.case, nasty);
    let back = store.find_by_node(nasty, "n'\"ode").unwrap().unwrap();
    assert_eq!(back.title, nasty);
    assert_eq!(back.process, nasty);
    assert!(back.assignees.contains(nasty));
    let filter = TaskFilter {
        case: Some(nasty.to_owned()),
        process: Some(nasty.to_owned()),
        assignees: Some(vec![nasty.to_owned()]),
        ..TaskFilter::default()
    };
    assert_eq!(store.list(&filter).unwrap().len(), 1);
    let trail = store.events(&t.id).unwrap();
    assert_eq!(trail[0].actor, nasty);
    assert_eq!(trail[0].reason.as_deref(), Some(nasty));
    // The table is still there.
    assert_eq!(store.list(&TaskFilter::default()).unwrap().len(), 1);
}

#[test]
fn a_corrupt_row_is_an_error_not_skipped() {
    let Some(fx) = fixture() else { return };
    let (_bridge, store) = open_store(fx.db.config(), "acme");
    for (id, case) in [("t_ok", "c1"), ("t_bad", "c2"), ("t_dis", "c3")] {
        let t = task(id, case, "n", &["alice"]);
        store.create(t.clone(), created(&t)).unwrap();
    }
    fx.rt.block_on(async {
        let client = fx.db.raw_client().await;
        // A body that is not a task.
        client
            .batch_execute(
                "UPDATE spot_bpm_task SET body = '{\"id\": 7}'::jsonb WHERE task_id = 't_bad'",
            )
            .await
            .unwrap();
        // A column that disagrees with its body.
        client
            .batch_execute("UPDATE spot_bpm_task SET status = 'completed' WHERE task_id = 't_dis'")
            .await
            .unwrap();
    });
    for id in ["t_bad", "t_dis"] {
        let id = spot_bpm_tasks::TaskId::new(id).unwrap();
        match store.load(&id) {
            Err(TasksError::StoreFailed { detail }) => {
                assert!(detail.contains("not readable"), "{detail}")
            }
            other => panic!("expected StoreFailed, got {other:?}"),
        }
    }
    assert!(matches!(
        store.list(&TaskFilter::default()),
        Err(TasksError::StoreFailed { .. })
    ));
    assert!(matches!(
        store.find_by_node("c2", "n"),
        Err(TasksError::StoreFailed { .. })
    ));
    // The healthy row is still readable on its own.
    assert!(
        store
            .load(&spot_bpm_tasks::TaskId::new("t_ok").unwrap())
            .unwrap()
            .is_some()
    );
}

#[test]
fn a_backend_failure_is_an_error_without_content() {
    let Some(fx) = fixture() else { return };
    let (_bridge, store) = open_store(fx.db.config(), "acme");
    let mut t = task("t_fail", "case1", "approve", &["alice"]);
    t.title = "title-marker-7731".to_owned();
    t.form = serde_json::json!({ "note": "form-marker-7731" });
    store.create(t.clone(), created(&t)).unwrap();
    fx.rt.block_on(async {
        fx.db
            .raw_client()
            .await
            .batch_execute("DROP TABLE spot_bpm_task_event; DROP TABLE spot_bpm_task")
            .await
            .unwrap();
    });
    let errors = [
        store.load(&t.id).map(|_| ()).unwrap_err(),
        store
            .find_by_node("case1", "approve")
            .map(|_| ())
            .unwrap_err(),
        store.events(&t.id).map(|_| ()).unwrap_err(),
        store.list(&TaskFilter::default()).map(|_| ()).unwrap_err(),
        store
            .list_by_role(&["r".to_owned()])
            .map(|_| ())
            .unwrap_err(),
        store
            .create(t.clone(), created(&t))
            .map(|_| ())
            .unwrap_err(),
        store
            .commit(&t.id, 1, t.clone(), Some(claimed(&t, "alice", 1)))
            .map(|_| ())
            .unwrap_err(),
    ];
    for error in errors {
        let TasksError::StoreFailed { detail } = &error else {
            panic!("expected StoreFailed, got {error:?}");
        };
        assert!(detail.starts_with("pg task store:"), "{detail}");
        assert!(
            detail.contains("sqlstate class 42"),
            "undefined table is class 42: {detail}"
        );
        for leak in [
            "marker-7731",
            "case1",
            "alice",
            "spot_bpm_task",
            "SELECT",
            "INSERT",
        ] {
            assert!(!detail.contains(leak), "`{leak}` leaked into: {detail}");
        }
        assert!(!error.to_string().contains("marker-7731"));
    }
}

#[test]
fn a_refused_write_leaves_no_partial_row() {
    let Some(fx) = fixture() else { return };
    let (_bridge, store) = open_store(fx.db.config(), "acme");
    let t = task("t_one", "case1", "approve", &["alice"]);
    let stored = store.create(t.clone(), created(&t)).unwrap();

    // A stale version: nothing changes, no event.
    let mut stale = stored.clone();
    stale.status = TaskStatus::Completed;
    assert!(matches!(
        store.commit(&t.id, 9, stale, Some(claimed(&t, "alice", 1))),
        Err(TasksError::VersionConflict { .. })
    ));

    // A commit that moves the task to another case or node is refused.
    let mut moved = stored.clone();
    moved.node = "elsewhere".to_owned();
    assert!(matches!(
        store.commit(&t.id, 1, moved, Some(claimed(&t, "alice", 1))),
        Err(TasksError::StoreFailed { .. })
    ));

    // An event of another task, or a task of another id, is refused before any statement.
    let other = task("t_two", "case2", "approve", &["bob"]);
    assert!(matches!(
        store.commit(&t.id, 1, stored.clone(), Some(created(&other))),
        Err(TasksError::StoreFailed { .. })
    ));
    assert!(matches!(
        store.commit(&t.id, 1, other.clone(), None),
        Err(TasksError::StoreFailed { .. })
    ));
    assert!(matches!(
        store.create(other.clone(), created(&t)),
        Err(TasksError::StoreFailed { .. })
    ));
    assert!(store.load(&other.id).unwrap().is_none());

    // A task id already used by another node: refused, the first task is untouched.
    let clash = task("t_one", "case3", "approve", &["mallory"]);
    assert!(matches!(
        store.create(clash.clone(), created(&clash)),
        Err(TasksError::StoreFailed { .. })
    ));

    let after = store.load(&t.id).unwrap().unwrap();
    assert_eq!(after, stored);
    assert_eq!(store.events(&t.id).unwrap().len(), 1);
    assert_eq!(store.list(&TaskFilter::default()).unwrap().len(), 1);
}

#[test]
fn a_failing_event_insert_rolls_back_the_task_update() {
    let Some(fx) = fixture() else { return };
    let (_bridge, store) = open_store(fx.db.config(), "acme");
    let t = task("t_atomic", "case1", "approve", &["alice"]);
    let stored = store.create(t.clone(), created(&t)).unwrap();
    // Make every event insert fail: the one statement must then change nothing.
    fx.rt.block_on(async {
        fx.db
            .raw_client()
            .await
            .batch_execute("ALTER TABLE spot_bpm_task_event ADD CONSTRAINT no_more CHECK (seq < 2)")
            .await
            .unwrap();
    });
    let mut changed = stored.clone();
    changed.status = TaskStatus::Claimed;
    let err = store
        .commit(&t.id, 1, changed, Some(claimed(&t, "alice", 1)))
        .unwrap_err();
    assert!(matches!(err, TasksError::StoreFailed { .. }), "{err:?}");
    let after = store.load(&t.id).unwrap().unwrap();
    assert_eq!(after.status, TaskStatus::Open);
    assert_eq!(after.version, 1);
    assert_eq!(store.events(&t.id).unwrap().len(), 1);
}
