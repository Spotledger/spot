//! The journal against a real PostgreSQL (`SPOT_TEST_PG_URL`; skipped with a message when unset).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::thread;

use common::{backend, bridge_config, fixture, node};
use spot_fix_pipeline::{FixId, FixJournal, InMemoryJournal, JournalRecord, codes};
use spot_fix_pipeline_pg::{PgFixJournal, codes as pg_codes};
use spot_pg_bridge::Bridge;

fn fix(name: &str) -> FixId {
    FixId::new(name).unwrap()
}

fn note(text: &str) -> JournalRecord {
    JournalRecord::ApprovalRequested {
        reason: text.to_owned(),
    }
}

/// The cases every `FixJournal` must show; run against both journals.
fn behaviour_table(journal: &dyn FixJournal) {
    let (a, b) = (fix("fix-a"), fix("fix-b"));
    assert!(journal.all().unwrap().is_empty());
    assert!(journal.entries(&a).unwrap().is_empty());
    assert_eq!(journal.append(&a, 10, note("one")).unwrap(), 1);
    assert_eq!(
        journal
            .append(&b, 11, JournalRecord::VerifyStarted)
            .unwrap(),
        2
    );
    assert_eq!(journal.append(&a, 12, note("three")).unwrap(), 3);
    // A fix sees only its own records, in order, with the time it was given.
    let mine = journal.entries(&a).unwrap();
    assert_eq!(
        mine.iter().map(|e| (e.seq, e.at)).collect::<Vec<_>>(),
        vec![(1, 10), (3, 12)]
    );
    assert_eq!(mine[0].record, note("one"));
    assert_eq!(mine[0].fix, a);
    // `all` is every record in sequence order.
    let all = journal.all().unwrap();
    assert_eq!(all.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![1, 2, 3]);
    assert_eq!(all[1].record, JournalRecord::VerifyStarted);
    assert_eq!(all[1].fix, b);
    // Negative and zero instants are stored as given.
    assert_eq!(journal.append(&b, -5, note("past")).unwrap(), 4);
    assert_eq!(journal.entries(&b).unwrap()[1].at, -5);
}

#[test]
fn the_memory_journal_shows_the_behaviour_table() {
    behaviour_table(&InMemoryJournal::new());
}

#[test]
fn the_pg_journal_shows_the_same_behaviour_table() {
    let Some(fx) = fixture() else { return };
    behaviour_table(&node(&fx, "fix_table"));
}

#[test]
fn a_hundred_appends_from_four_connections_give_one_to_a_hundred_with_no_gap_or_duplicate() {
    let Some(fx) = fixture() else { return };
    let backend = backend(&fx, 8);
    // Four nodes: four bridges, each with its own connection, appending in parallel.
    let nodes: Vec<Arc<PgFixJournal>> = (0..4)
        .map(|i| {
            let bridge = Arc::new(
                Bridge::for_backend(bridge_config(&format!("fix_conc_{i}"), 1), &backend).unwrap(),
            );
            Arc::new(PgFixJournal::new(bridge, &backend.pool()).unwrap())
        })
        .collect();
    nodes[0].ensure_schema().unwrap();
    let handles: Vec<_> = nodes
        .iter()
        .enumerate()
        .map(|(i, journal)| {
            let journal = Arc::clone(journal);
            thread::spawn(move || {
                let id = FixId::new(&format!("fix-conc-{i}")).unwrap();
                (0..25)
                    .map(|n| journal.append(&id, n, note("x")).unwrap())
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    let mut seen: Vec<u64> = Vec::new();
    for handle in handles {
        let mine = handle.join().unwrap();
        assert!(mine.windows(2).all(|w| w[0] < w[1]), "per-caller order");
        seen.extend(mine);
    }
    seen.sort_unstable();
    assert_eq!(seen, (1..=100).collect::<Vec<u64>>());
    let all = nodes[0].all().unwrap();
    assert_eq!(
        all.iter().map(|e| e.seq).collect::<Vec<_>>(),
        (1..=100).collect::<Vec<u64>>()
    );
}

/// Runs `sql` in a transaction as `role` (or the owner), returning the error text if it failed.
fn try_sql(fx: &common::Fixture, role: Option<&str>, sql: &str) -> Result<(), String> {
    fn text(e: &tokio_postgres::Error) -> String {
        e.as_db_error()
            .map_or_else(|| e.to_string(), |d| d.message().to_owned())
    }
    let backend = backend(fx, 4);
    fx.rt.block_on(async {
        let mut client = backend.pool().get().await.map_err(|e| e.to_string())?;
        let tx = client.transaction().await.map_err(|e| text(&e))?;
        if let Some(role) = role {
            tx.batch_execute(&format!("SET LOCAL ROLE {role}"))
                .await
                .map_err(|e| text(&e))?;
        }
        tx.batch_execute(sql).await.map_err(|e| text(&e))?;
        tx.commit().await.map_err(|e| text(&e))
    })
}

#[test]
fn update_and_delete_fail_for_the_application_role_and_for_the_owner() {
    let Some(fx) = fixture() else { return };
    let journal = node(&fx, "fix_append_only");
    journal.append(&fix("fix-a"), 1, note("kept")).unwrap();
    let role = Some(spot_fix_pipeline_pg::schema::APPLICATION_ROLE);
    let table = "spot_control.spot_fix_journal";
    for sql in [
        format!("UPDATE {table} SET at = 99"),
        format!("DELETE FROM {table}"),
        format!("TRUNCATE {table}"),
    ] {
        // The role lacks the privilege.
        let denied = try_sql(&fx, role, &sql).unwrap_err();
        assert!(denied.contains("permission denied"), "{sql}: {denied}");
        // The owner has the privilege, and the trigger still refuses.
        let refused = try_sql(&fx, None, &sql).unwrap_err();
        assert!(refused.contains("append-only"), "{sql}: {refused}");
    }
    // The role may append through the bridge's own statements but not touch the head beyond a bump.
    try_sql(
        &fx,
        role,
        "SELECT seq FROM spot_control.spot_fix_journal; \
         UPDATE spot_control.spot_fix_journal_head SET last_seq = last_seq WHERE id = 1",
    )
    .unwrap();
    try_sql(&fx, role, "DELETE FROM spot_control.spot_fix_journal_head").unwrap_err();
    // Nothing changed.
    let all = journal.all().unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].at, 1);
    assert_eq!(all[0].record, note("kept"));
}

#[test]
fn state_survives_a_restart() {
    let Some(fx) = fixture() else { return };
    let a = fix("fix-restart");
    {
        let first = node(&fx, "fix_restart_one");
        assert_eq!(first.append(&a, 1, note("before")).unwrap(), 1);
        assert_eq!(
            first.append(&a, 2, JournalRecord::VerifyStarted).unwrap(),
            2
        );
    }
    let second = node(&fx, "fix_restart_two");
    assert_eq!(second.entries(&a).unwrap().len(), 2);
    // The next number continues; nothing was reset by the second ensure_schema.
    assert_eq!(second.append(&a, 3, note("after")).unwrap(), 3);
}

#[test]
fn a_rolled_back_append_consumes_no_number() {
    let Some(fx) = fixture() else { return };
    let journal = node(&fx, "fix_rollback");
    let a = fix("fix-a");
    // An owner plants a row at number 1: the next append bumps the head to 1 and the insert
    // then violates the primary key, so the whole transaction must roll back.
    try_sql(
        &fx,
        None,
        "INSERT INTO spot_control.spot_fix_journal (seq, fix, at, record) \
         VALUES (1, 'planted', 0, '{\"record\":\"verify_started\"}')",
    )
    .unwrap();
    let error = journal.append(&a, 5, note("x")).unwrap_err();
    assert_eq!(error.code(), codes::JOURNAL_FAILED);
    assert!(error.to_string().contains("SQLSTATE 23505"), "{error}");
    let head = fx.rt.block_on(async {
        let client = backend(&fx, 4).pool().get().await.unwrap();
        let row = client
            .query_one(
                "SELECT last_seq FROM spot_control.spot_fix_journal_head",
                &[],
            )
            .await
            .unwrap();
        row.get::<_, i64>(0)
    });
    assert_eq!(head, 0, "the failed append did not consume a number");
    assert!(journal.entries(&a).unwrap().is_empty());
}

#[test]
fn page_after_streams_the_journal_in_order() {
    let Some(fx) = fixture() else { return };
    let journal = node(&fx, "fix_pages");
    let a = fix("fix-a");
    for n in 0..7 {
        journal.append(&a, n, note("x")).unwrap();
    }
    let mut after = 0;
    let mut got = Vec::new();
    loop {
        let page = journal.page_after(after, 3).unwrap();
        if page.is_empty() {
            break;
        }
        assert!(page.len() <= 3);
        after = page.last().unwrap().seq;
        got.extend(page.into_iter().map(|e| e.seq));
    }
    assert_eq!(got, (1..=7).collect::<Vec<u64>>());
}

#[test]
fn reads_cross_a_page_boundary_in_order() {
    let Some(fx) = fixture() else { return };
    let journal = node(&fx, "fix_big");
    let (a, b) = (fix("fix-a"), fix("fix-b"));
    for n in 0..(spot_fix_pipeline_pg::PAGE as i64 + 20) {
        journal
            .append(if n % 2 == 0 { &a } else { &b }, n, note("x"))
            .unwrap();
    }
    let all = journal.all().unwrap();
    assert_eq!(all.len(), spot_fix_pipeline_pg::PAGE + 20);
    assert!(all.windows(2).all(|w| w[0].seq + 1 == w[1].seq));
    let mine = journal.entries(&a).unwrap();
    assert_eq!(mine.len(), (spot_fix_pipeline_pg::PAGE + 20) / 2);
    assert!(mine.windows(2).all(|w| w[0].seq < w[1].seq));
}

#[test]
fn a_row_that_no_longer_deserialises_is_an_error_that_names_no_content() {
    let Some(fx) = fixture() else { return };
    let journal = node(&fx, "fix_bad_row");
    let a = fix("fix-a");
    journal.append(&a, 1, note("fine")).unwrap();
    try_sql(
        &fx,
        None,
        "INSERT INTO spot_control.spot_fix_journal (seq, fix, at, record) \
         VALUES (2, 'fix-a', 0, '{\"record\":\"nonsense\",\"secret\":\"hunter2\"}')",
    )
    .unwrap();
    for error in [
        journal.entries(&a).unwrap_err(),
        journal.all().unwrap_err(),
        journal.page_after(0, 10).unwrap_err(),
    ] {
        assert_eq!(error.code(), codes::JOURNAL_FAILED);
        for text in [format!("{error}"), format!("{error:?}")] {
            assert!(!text.contains("hunter2"), "{text}");
            assert!(text.contains("row 2"), "{text}");
        }
    }
}

#[test]
fn a_call_from_inside_a_runtime_is_refused_not_deadlocked() {
    let Some(fx) = fixture() else { return };
    let journal = node(&fx, "fix_wrong_thread");
    let a = fix("fix-a");
    let error = fx
        .rt
        .block_on(async { journal.append(&a, 1, note("x")) })
        .unwrap_err();
    assert_eq!(error.code(), codes::JOURNAL_FAILED);
    assert!(
        error.to_string().contains("pg_bridge.wrong_thread"),
        "{error}"
    );
    let error = fx.rt.block_on(async { journal.all() }).unwrap_err();
    assert!(
        error.to_string().contains("pg_bridge.wrong_thread"),
        "{error}"
    );
}

#[test]
fn the_constructor_refuses_a_bridge_that_does_not_fit_the_pool_or_a_current_thread_runtime() {
    let Some(fx) = fixture() else { return };
    let big = backend(&fx, 8);
    let bridge = Arc::new(Bridge::for_backend(bridge_config("fix_ctor", 6), &big).unwrap());
    let small = backend(&fx, 4);
    let error = PgFixJournal::new(Arc::clone(&bridge), &small.pool()).unwrap_err();
    assert_eq!(error.code(), pg_codes::SETUP_FAILED);
    let current = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let error = current
        .block_on(async { PgFixJournal::new(Arc::clone(&bridge), &big.pool()) })
        .unwrap_err();
    assert_eq!(error.code(), pg_codes::SETUP_FAILED);
    PgFixJournal::new(bridge, &big.pool()).unwrap();
}

#[test]
fn a_stopped_database_is_a_store_failure_with_no_partial_row_and_no_secret() {
    let Some(fx) = fixture() else { return };
    let good = node(&fx, "fix_stopped_good");
    let a = fix("fix-a");
    good.append(&a, 1, note("before")).unwrap();
    // A pool pointing at a port nothing listens on, with a recognisable password.
    let mut config = fx.db.config();
    config.port = 1;
    config.password = "hunter2-secret".to_owned();
    config.pool_max_size = 8;
    config.connect_timeout = std::time::Duration::from_millis(500);
    config.pool_wait_timeout = std::time::Duration::from_millis(500);
    let dead = spot_postgres::PgBackend::connect_unchecked(config).unwrap();
    let bridge = Arc::new(Bridge::for_backend(bridge_config("fix_stopped", 6), &dead).unwrap());
    let journal = PgFixJournal::new(bridge, &dead.pool()).unwrap();
    let errors = [
        journal.append(&a, 2, note("hunter2-record")).unwrap_err(),
        journal.entries(&a).unwrap_err(),
        journal.all().unwrap_err(),
        journal.page_after(0, 5).unwrap_err(),
    ];
    for error in &errors {
        assert_eq!(error.code(), codes::JOURNAL_FAILED);
        for text in [format!("{error}"), format!("{error:?}")] {
            for secret in [
                "hunter2",
                "127.0.0.1",
                "postgres://",
                "spot_fix_journal",
                "SELECT",
                "INSERT",
            ] {
                assert!(!text.contains(secret), "{secret} in {text}");
            }
        }
    }
    let setup = journal.ensure_schema().unwrap_err();
    assert_eq!(setup.code(), pg_codes::SETUP_FAILED);
    assert!(!format!("{setup} {setup:?}").contains("hunter2"));
    // The real database holds only what was stored before: no partial row.
    let all = good.all().unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(good.append(&a, 3, note("next")).unwrap(), 2);
}

#[test]
fn an_append_that_times_out_waiting_for_the_head_lock_writes_nothing() {
    let Some(fx) = fixture() else { return };
    node(&fx, "fix_timeout_setup");
    let backend = backend(&fx, 8);
    let mut config = bridge_config("fix_timeout", 2);
    config.default_deadline = std::time::Duration::from_secs(2);
    config.max_deadline = std::time::Duration::from_secs(2);
    let bridge = Arc::new(Bridge::for_backend(config, &backend).unwrap());
    let journal = PgFixJournal::new(bridge, &backend.pool()).unwrap();
    let a = fix("fix-a");
    journal.append(&a, 1, note("first")).unwrap();
    // Another session holds the head row's lock, as a slow concurrent append would.
    let outcome = fx.rt.block_on(async {
        let mut holder = backend.pool().get().await.unwrap();
        let tx = holder.transaction().await.unwrap();
        tx.query_one(
            "SELECT last_seq FROM spot_control.spot_fix_journal_head WHERE id = 1 FOR UPDATE",
            &[],
        )
        .await
        .unwrap();
        let journal = journal.clone();
        let a = a.clone();
        // A plain thread: a spawn_blocking thread belongs to the runtime and the bridge refuses it.
        let result = thread::spawn(move || journal.append(&a, 2, note("late")))
            .join()
            .unwrap();
        tx.rollback().await.unwrap();
        result
    });
    let error = outcome.unwrap_err();
    assert_eq!(error.code(), codes::JOURNAL_FAILED);
    assert!(error.to_string().contains("pg_bridge.timeout"), "{error}");
    let all = journal.all().unwrap();
    assert_eq!(all.len(), 1, "the timed-out append left no row");
    assert_eq!(journal.append(&a, 3, note("after")).unwrap(), 2);
}
