//! The store against a real PostgreSQL (`SPOT_TEST_PG_URL`; skipped with a message when unset).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::thread;

use common::{backend, bridge_of, fixture, live, migrate, node, plan_of, wrapped};
use spot_journal_pii_secrets::testing::MemoryWrappedKeyStore;
use spot_journal_pii_secrets::{StoredKey, WrappedKeyStore};
use spot_journal_pii_secrets_pg::{DDL, PgWrappedKeyStore, TABLE, ensure_present, extra_table};
use spot_pg_migrate::{ApplyOptions, ExtraTable, Risk, apply};

/// The behaviour of `MemoryWrappedKeyStore`, which every store must show.
fn behaviour_table(store: &dyn WrappedKeyStore) {
    // Nothing stored: absent, and destroy of an absent entry says no live key was removed.
    assert_eq!(store.get("e1").unwrap(), None);
    // Put twice: the first stores, the second loses and changes nothing.
    assert!(store.put_if_absent("e1", wrapped(1)).unwrap());
    assert!(!store.put_if_absent("e1", wrapped(2)).unwrap());
    assert_eq!(store.get("e1").unwrap(), Some(live(1)));
    // Destroy returns true once, then leaves a tombstone.
    assert!(store.destroy("e1").unwrap());
    assert_eq!(store.get("e1").unwrap(), Some(StoredKey::Destroyed));
    assert!(!store.destroy("e1").unwrap());
    // Destroy then put: the tombstone is never overwritten.
    assert!(!store.put_if_absent("e1", wrapped(3)).unwrap());
    assert_eq!(store.get("e1").unwrap(), Some(StoredKey::Destroyed));
    // Destroy of an entry that never existed leaves a tombstone, and returns false.
    assert!(!store.destroy("e2").unwrap());
    assert_eq!(store.get("e2").unwrap(), Some(StoredKey::Destroyed));
    assert!(!store.put_if_absent("e2", wrapped(4)).unwrap());
    // Entries are independent.
    assert!(store.put_if_absent("e3", wrapped(5)).unwrap());
    assert_eq!(store.get("e3").unwrap(), Some(live(5)));
}

#[test]
fn the_memory_store_shows_the_behaviour_table() {
    behaviour_table(&MemoryWrappedKeyStore::new());
}

#[test]
fn the_pg_store_shows_the_same_behaviour_table() {
    let Some(fx) = fixture() else { return };
    behaviour_table(&node(&fx, "pii_table"));
}

#[test]
fn two_concurrent_puts_give_exactly_one_true() {
    let Some(fx) = fixture() else { return };
    let store = Arc::new(node(&fx, "pii_race_put"));
    for round in 0..10u8 {
        let entry = format!("race-{round}");
        let handles: Vec<_> = (0..4u8)
            .map(|i| {
                let store = Arc::clone(&store);
                let entry = entry.clone();
                thread::spawn(move || store.put_if_absent(&entry, wrapped(i)).unwrap())
            })
            .collect();
        let wins = handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|won| *won)
            .count();
        assert_eq!(wins, 1, "round {round}");
    }
}

#[test]
fn concurrent_destroys_give_exactly_one_true_and_one_tombstone() {
    let Some(fx) = fixture() else { return };
    let store = Arc::new(node(&fx, "pii_race_destroy"));
    assert!(store.put_if_absent("victim", wrapped(1)).unwrap());
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let store = Arc::clone(&store);
            thread::spawn(move || store.destroy("victim").unwrap())
        })
        .collect();
    let removed = handles
        .into_iter()
        .map(|h| h.join().unwrap())
        .filter(|r| *r)
        .count();
    assert_eq!(removed, 1);
    assert_eq!(store.get("victim").unwrap(), Some(StoredKey::Destroyed));
}

#[test]
fn a_put_racing_a_destroy_never_leaves_a_live_key_after_the_destroy() {
    let Some(fx) = fixture() else { return };
    let store = Arc::new(node(&fx, "pii_race_mixed"));
    for round in 0..20u8 {
        let entry = format!("mixed-{round}");
        let putter = {
            let store = Arc::clone(&store);
            let entry = entry.clone();
            thread::spawn(move || store.put_if_absent(&entry, wrapped(round)).unwrap())
        };
        let destroyer = {
            let store = Arc::clone(&store);
            let entry = entry.clone();
            thread::spawn(move || store.destroy(&entry).unwrap())
        };
        let put = putter.join().unwrap();
        let removed = destroyer.join().unwrap();
        // The destroy ran last in the database whatever the order: the entry is a tombstone, and
        // a live key was removed exactly when the put won the race.
        assert_eq!(store.get(&entry).unwrap(), Some(StoredKey::Destroyed));
        assert_eq!(removed, put, "round {round}");
    }
}

#[test]
fn a_second_store_instance_on_the_same_database_sees_the_key_after_a_restart() {
    let Some(fx) = fixture() else { return };
    {
        let first = node(&fx, "pii_restart_a");
        assert!(first.put_if_absent("kept", wrapped(7)).unwrap());
        assert!(first.put_if_absent("gone", wrapped(8)).unwrap());
        assert!(first.destroy("gone").unwrap());
    }
    // A new bridge, a new pool: nothing in memory survives.
    let second = node(&fx, "pii_restart_b");
    assert_eq!(second.get("kept").unwrap(), Some(live(7)));
    assert_eq!(second.get("gone").unwrap(), Some(StoredKey::Destroyed));
    assert!(!second.put_if_absent("gone", wrapped(9)).unwrap());
}

#[test]
fn a_destroyed_entry_has_no_wrapped_bytes_in_the_table() {
    let Some(fx) = fixture() else { return };
    let store = node(&fx, "pii_raw");
    assert!(store.put_if_absent("secret", wrapped(0xab)).unwrap());
    assert!(store.destroy("secret").unwrap());
    assert!(!store.destroy("never").unwrap());
    let client = fx.rt.block_on(fx.db.raw_client());
    let rows = fx
        .rt
        .block_on(client.query(
            "SELECT entry, state, key_id, key_version, wrapped, created_at, destroyed_at \
             FROM spot_journal_subject_key ORDER BY entry",
            &[],
        ))
        .unwrap();
    assert_eq!(rows.len(), 2);
    for row in &rows {
        assert_eq!(row.get::<_, String>(1), "destroyed");
        assert!(row.get::<_, Option<String>>(2).is_none(), "key_id");
        assert!(row.get::<_, Option<i64>>(3).is_none(), "key_version");
        assert!(row.get::<_, Option<Vec<u8>>>(4).is_none(), "wrapped");
        assert!(
            row.get::<_, Option<std::time::SystemTime>>(6).is_some(),
            "destroyed_at"
        );
    }
    // No byte of the wrapped key is left anywhere in the table's pages either: the row version
    // that held it is dead and the live one has none. A text search over the row is enough here.
    let hits = fx
        .rt
        .block_on(client.query_one(
            "SELECT count(*) FROM spot_journal_subject_key t WHERE t::text LIKE '%abab%'",
            &[],
        ))
        .unwrap();
    assert_eq!(hits.get::<_, i64>(0), 0);
}

#[test]
fn the_table_refuses_rows_that_are_neither_live_nor_a_tombstone() {
    let Some(fx) = fixture() else { return };
    let _store = node(&fx, "pii_check");
    let client = fx.rt.block_on(fx.db.raw_client());
    let bad = [
        "INSERT INTO spot_journal_subject_key (entry, state) VALUES ('a', 'live')",
        "INSERT INTO spot_journal_subject_key (entry, state, destroyed_at) VALUES ('b', 'destroyed', now())\
         ; UPDATE spot_journal_subject_key SET wrapped = '\\x00' WHERE entry = 'b'",
        "INSERT INTO spot_journal_subject_key (entry, state, destroyed_at) VALUES ('c', 'gone', now())",
        "INSERT INTO spot_journal_subject_key (entry, state, key_id, key_version, wrapped) \
         VALUES ('d', 'live', 'k', -1, '\\x00')",
        "INSERT INTO spot_journal_subject_key (entry, state, key_id, key_version, wrapped) \
         VALUES ('e', 'live', 'k', 4294967296, '\\x00')",
    ];
    for sql in bad {
        assert!(
            fx.rt.block_on(client.batch_execute(sql)).is_err(),
            "accepted: {sql}"
        );
    }
}

#[test]
fn the_extra_table_is_one_statement_the_hook_accepts() {
    // Needs no server. The hook refuses a semicolon, so the constant ends without one.
    let extra = extra_table().unwrap();
    assert_eq!(extra.table, TABLE);
    assert_eq!(extra.create, vec![DDL.to_owned()]);
    assert!(!DDL.contains(';'));
    assert_eq!(ExtraTable::new(TABLE, vec![DDL.to_owned()]).unwrap(), extra);
}

#[test]
fn the_migrator_plans_exactly_one_additive_step_and_a_second_plan_is_empty() {
    let Some(fx) = fixture() else { return };
    let backend = backend(&fx, 8);
    let plan = plan_of(&fx, &backend);
    assert!(plan.refused.is_empty(), "{:?}", plan.refused);
    let extra: Vec<_> = plan
        .steps
        .iter()
        .filter(|s| s.key.starts_with("extra_table:"))
        .collect();
    assert_eq!(extra.len(), 1, "{:?}", plan.steps);
    assert_eq!(extra[0].key, "extra_table:spot_journal_subject_key");
    assert_eq!(extra[0].risk, Risk::Additive);
    assert!(
        plan.steps
            .iter()
            .all(|s| s.key == extra[0].key || s.key == "finalise"),
        "{:?}",
        plan.steps
    );
    let report = fx
        .rt
        .block_on(apply(&backend, &plan, ApplyOptions::default()))
        .unwrap();
    assert!(report.executed.contains(&extra[0].key));
    let again = plan_of(&fx, &backend);
    assert!(again.is_empty(), "{:?}", again.steps);
}

#[test]
fn ensure_present_fails_without_the_table_creates_nothing_and_passes_after_the_migration() {
    let Some(fx) = fixture() else { return };
    let backend = backend(&fx, 8);
    let bridge = bridge_of(&backend, "pii_ensure");
    let err = ensure_present(&bridge).unwrap_err();
    assert_eq!(err.code(), "journal_pii_secrets_pg.table_missing");
    assert!(
        err.to_string()
            .starts_with("journal_pii_secrets_pg.table_missing")
    );
    // It created nothing and took no lock: the table is still absent, and a second call says so.
    assert!(!table_exists(&fx));
    assert!(ensure_present(&bridge).is_err());
    assert!(!table_exists(&fx));
    migrate(&fx, &backend);
    ensure_present(&bridge).unwrap();
    ensure_present(&bridge).unwrap();
}

#[test]
fn ensure_present_fails_on_a_table_that_lacks_a_column_or_has_a_wrong_type() {
    let Some(fx) = fixture() else { return };
    let backend = backend(&fx, 8);
    let bridge = bridge_of(&backend, "pii_columns");
    migrate(&fx, &backend);
    ensure_present(&bridge).unwrap();
    let client = fx.rt.block_on(fx.db.raw_client());
    fx.rt
        .block_on(client.batch_execute(
            "ALTER TABLE spot_journal_subject_key DROP CONSTRAINT spot_journal_subject_key_shape_check; \
             ALTER TABLE spot_journal_subject_key DROP COLUMN destroyed_at",
        ))
        .unwrap();
    assert_eq!(
        ensure_present(&bridge).unwrap_err().code(),
        "journal_pii_secrets_pg.table_missing"
    );
    fx.rt
        .block_on(
            client
                .batch_execute("ALTER TABLE spot_journal_subject_key ADD COLUMN destroyed_at text"),
        )
        .unwrap();
    assert_eq!(
        ensure_present(&bridge).unwrap_err().code(),
        "journal_pii_secrets_pg.table_missing"
    );
}

fn table_exists(fx: &common::Fixture) -> bool {
    let client = fx.rt.block_on(fx.db.raw_client());
    fx.rt
        .block_on(client.query_one(
            "SELECT to_regclass('spot_journal_subject_key') IS NOT NULL",
            &[],
        ))
        .unwrap()
        .get(0)
}

#[test]
fn the_migrator_made_table_has_the_same_named_constraints_as_before() {
    let Some(fx) = fixture() else { return };
    let backend = backend(&fx, 8);
    migrate(&fx, &backend);
    let client = fx.rt.block_on(fx.db.raw_client());
    // Primary key and check constraints (a server that lists NOT NULL as constraints adds more
    // rows of another type, which are left out here).
    let rows = fx
        .rt
        .block_on(client.query(
            "SELECT conname::text, pg_get_constraintdef(oid) FROM pg_constraint \
             WHERE conrelid = 'spot_journal_subject_key'::regclass AND contype IN ('p', 'c') \
             ORDER BY conname",
            &[],
        ))
        .unwrap();
    let got: Vec<(String, String)> = rows.iter().map(|r| (r.get(0), r.get(1))).collect();
    let names: Vec<&str> = got.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        [
            "spot_journal_subject_key_pkey",
            "spot_journal_subject_key_shape_check",
            "spot_journal_subject_key_state_check",
            "spot_journal_subject_key_version_check",
        ]
    );
    let def = |name: &str| got.iter().find(|(n, _)| n == name).unwrap().1.clone();
    assert_eq!(def("spot_journal_subject_key_pkey"), "PRIMARY KEY (entry)");
    assert!(def("spot_journal_subject_key_state_check").contains("'live'"));
    assert!(def("spot_journal_subject_key_state_check").contains("'destroyed'"));
    assert!(def("spot_journal_subject_key_version_check").contains("4294967295"));
    let shape = def("spot_journal_subject_key_shape_check");
    for part in [
        "'live'",
        "'destroyed'",
        "wrapped IS NOT NULL",
        "wrapped IS NULL",
        "destroyed_at IS NULL",
    ] {
        assert!(shape.contains(part), "{part} missing from {shape}");
    }
    for (name, _) in &got {
        assert!(DDL.contains(name.as_str()));
    }
}

#[test]
fn a_database_failure_is_an_error_from_every_method_never_absent() {
    let Some(fx) = fixture() else { return };
    let backend = backend(&fx, 8);
    migrate(&fx, &backend);
    let bridge = bridge_of(&backend, "pii_closed");
    let store = PgWrappedKeyStore::new(bridge.clone());
    assert!(store.put_if_absent("k", wrapped(1)).unwrap());
    backend.pool().close();
    for err in [
        store.get("k").unwrap_err(),
        store.put_if_absent("k2", wrapped(2)).unwrap_err(),
        store.destroy("k").unwrap_err(),
    ] {
        assert!(
            err.0.starts_with("journal_pii_secrets_pg.store_failed"),
            "{err:?}"
        );
    }
    let err = ensure_present(&bridge).unwrap_err();
    assert_eq!(err.code(), "journal_pii_secrets_pg.store_failed");
    assert!(
        err.to_string()
            .starts_with("journal_pii_secrets_pg.store_failed")
    );
}

#[test]
fn a_missing_table_is_an_error_and_a_malformed_row_fails_closed() {
    let Some(fx) = fixture() else { return };
    let backend = backend(&fx, 8);
    let bridge = bridge_of(&backend, "pii_missing");
    let store = PgWrappedKeyStore::new(bridge.clone());
    // Nothing migrated: the table is missing, which is an error and not "absent".
    let err = store.get("k").unwrap_err();
    assert!(err.0.contains("SQLSTATE 42P01"), "{err:?}");
    migrate(&fx, &backend);
    // A row that violates the shape cannot be written through the constraint, so drop it to
    // plant one and see the store refuse to answer from it.
    let client = fx.rt.block_on(fx.db.raw_client());
    fx.rt
        .block_on(client.batch_execute(
            "ALTER TABLE spot_journal_subject_key DROP CONSTRAINT spot_journal_subject_key_shape_check; \
             INSERT INTO spot_journal_subject_key (entry, state) VALUES ('bad', 'live')",
        ))
        .unwrap();
    let err = store.get("bad").unwrap_err();
    assert!(
        err.0.starts_with("journal_pii_secrets_pg.row_invalid"),
        "{err:?}"
    );
}

#[test]
fn a_debug_print_of_the_store_shows_no_entry_and_no_key() {
    let Some(fx) = fixture() else { return };
    let store = node(&fx, "pii_debug");
    assert!(store.put_if_absent("an-entry-id", wrapped(0xcd)).unwrap());
    let printed = format!("{store:?} {:?}", store.get("an-entry-id").unwrap());
    assert!(!printed.contains("an-entry-id"));
    assert!(!printed.contains("205"), "no wrapped byte: {printed}");
}
