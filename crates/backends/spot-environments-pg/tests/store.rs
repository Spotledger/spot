//! The store against a real PostgreSQL (`SPOT_TEST_PG_URL`; skipped with a message when unset).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::thread;

use common::{backend, bridge_config, fixture, node, set};
use spot_environments::{EnvironmentStore, MemoryEnvironmentStore, codes};
use spot_environments_pg::{LIST_CEILING, PgEnvironmentStore, codes as pg_codes};
use spot_pg_bridge::Bridge;

/// The cases every `EnvironmentStore` must show; run against both stores.
fn behaviour_table(store: &dyn EnvironmentStore) {
    // Empty.
    assert!(store.list().unwrap().is_empty());
    assert_eq!(
        store.get("alpha").unwrap_err().code(),
        codes::STORE_NOT_FOUND
    );
    // Create stores version 1; a second create of the name is refused and changes nothing.
    let a = store.create(set("alpha", 1)).unwrap();
    assert_eq!(a.version, 1);
    assert_eq!(a.set, set("alpha", 1));
    assert_eq!(
        store.create(set("alpha", 2)).unwrap_err().code(),
        codes::STORE_NAME_TAKEN
    );
    assert_eq!(store.get("alpha").unwrap(), a);
    // Replace is a compare-and-swap.
    let b = store.replace(set("alpha", 2), 1).unwrap();
    assert_eq!(b.version, 2);
    assert_eq!(store.get("alpha").unwrap().set, set("alpha", 2));
    assert_eq!(
        store.replace(set("alpha", 3), 1).unwrap_err().code(),
        codes::STORE_VERSION_CONFLICT
    );
    assert_eq!(
        store.replace(set("beta", 3), 1).unwrap_err().code(),
        codes::STORE_NOT_FOUND
    );
    assert_eq!(
        store.replace(set("alpha", 3), u64::MAX).unwrap_err().code(),
        codes::STORE_VERSION_CONFLICT
    );
    assert_eq!(
        store.replace(set("beta", 3), u64::MAX).unwrap_err().code(),
        codes::STORE_NOT_FOUND
    );
    assert_eq!(store.get("alpha").unwrap().version, 2);
    // List is ordered by name.
    store.create(set("zulu", 1)).unwrap();
    store.create(set("bravo", 2)).unwrap();
    let names: Vec<String> = store
        .list()
        .unwrap()
        .iter()
        .map(|s| s.set.name().to_owned())
        .collect();
    assert_eq!(names, ["alpha", "bravo", "zulu"]);
}

#[test]
fn the_memory_store_shows_the_behaviour_table() {
    behaviour_table(&MemoryEnvironmentStore::new());
}

#[test]
fn the_pg_store_shows_the_same_behaviour_table() {
    let Some(fx) = fixture() else { return };
    behaviour_table(&node(&fx, "env_table"));
}

#[test]
fn fifty_concurrent_replaces_of_one_version_give_one_winner() {
    let Some(fx) = fixture() else { return };
    let store = Arc::new(node(&fx, "env_cas"));
    store.create(set("race", 0)).unwrap();
    let handles: Vec<_> = (0..50u8)
        .map(|i| {
            let store = Arc::clone(&store);
            thread::spawn(move || store.replace(set("race", i), 1))
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let wins = results.iter().filter(|r| r.is_ok()).count();
    assert_eq!(wins, 1);
    for r in results.iter().filter_map(|r| r.as_ref().err()) {
        assert_eq!(r.code(), codes::STORE_VERSION_CONFLICT);
    }
    assert_eq!(store.get("race").unwrap().version, 2);
}

#[test]
fn a_name_race_has_one_creator() {
    let Some(fx) = fixture() else { return };
    let store = Arc::new(node(&fx, "env_name"));
    let handles: Vec<_> = (0..16u8)
        .map(|i| {
            let store = Arc::clone(&store);
            thread::spawn(move || store.create(set("same", i)))
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    for r in results.iter().filter_map(|r| r.as_ref().err()) {
        assert_eq!(r.code(), codes::STORE_NAME_TAKEN);
    }
    assert_eq!(store.list().unwrap().len(), 1);
}

#[test]
fn a_document_edited_into_an_invalid_set_is_refused_on_get() {
    let Some(fx) = fixture() else { return };
    let store = node(&fx, "env_invalid");
    store.create(set("edited", 1)).unwrap();
    let raw = fx.rt.block_on(fx.db.raw_client());
    // An empty app set breaks the set's rules; a renamed document no longer matches its row.
    fx.rt
        .block_on(raw.batch_execute(
            "UPDATE spot_control.spot_environment_set \
             SET doc = jsonb_set(doc, '{apps}', '[]') WHERE name = 'edited'",
        ))
        .unwrap();
    let error = store.get("edited").unwrap_err();
    assert_eq!(error.code(), pg_codes::ROW_INVALID);
    assert_eq!(store.list().unwrap_err().code(), pg_codes::ROW_INVALID);
    fx.rt
        .block_on(raw.batch_execute(
            "UPDATE spot_control.spot_environment_set \
             SET doc = jsonb_set(set_doc.doc, '{apps}', '[{\"name\":\"a\",\"version\":\"1\"}]') \
             FROM (SELECT doc FROM spot_control.spot_environment_set) set_doc \
             WHERE name = 'edited'",
        ))
        .unwrap();
    store.get("edited").unwrap();
    fx.rt
        .block_on(raw.batch_execute(
            "UPDATE spot_control.spot_environment_set \
             SET doc = jsonb_set(doc, '{name}', '\"other\"') WHERE name = 'edited'",
        ))
        .unwrap();
    assert_eq!(
        store.get("edited").unwrap_err().code(),
        pg_codes::ROW_INVALID
    );
}

#[test]
fn state_survives_a_restart() {
    let Some(fx) = fixture() else { return };
    let store = node(&fx, "env_restart_a");
    store.create(set("kept", 1)).unwrap();
    store.replace(set("kept", 2), 1).unwrap();
    drop(store);
    let again = node(&fx, "env_restart_b");
    let got = again.get("kept").unwrap();
    assert_eq!(got.version, 2);
    assert_eq!(got.set, set("kept", 2));
}

#[test]
fn list_pages_through_more_than_one_page_in_order() {
    let Some(fx) = fixture() else { return };
    let store = node(&fx, "env_pages");
    store.create(set("seed", 1)).unwrap();
    let raw = fx.rt.block_on(fx.db.raw_client());
    fx.rt
        .block_on(raw.batch_execute(
            "INSERT INTO spot_control.spot_environment_set (name, version, doc) \
             SELECT 's' || lpad(g::text, 4, '0'), 1, \
                    jsonb_set(doc, '{name}', to_jsonb('s' || lpad(g::text, 4, '0'))) \
             FROM spot_control.spot_environment_set, generate_series(1, 250) g \
             WHERE name = 'seed'",
        ))
        .unwrap();
    let all = store.list().unwrap();
    assert_eq!(all.len(), 251);
    let names: Vec<&str> = all.iter().map(|s| s.set.name()).collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted);
}

#[test]
fn list_above_the_ceiling_fails_instead_of_truncating() {
    let Some(fx) = fixture() else { return };
    let store = node(&fx, "env_ceiling");
    let raw = fx.rt.block_on(fx.db.raw_client());
    fx.rt
        .block_on(raw.batch_execute(&format!(
            "INSERT INTO spot_control.spot_environment_set (name, version, doc) \
             SELECT 'n' || g, 1, '{{}}' FROM generate_series(1, {}) g",
            LIST_CEILING + 1
        )))
        .unwrap();
    assert_eq!(store.list().unwrap_err().code(), codes::STORE_FAILED);
}

#[test]
fn a_call_from_inside_a_runtime_is_refused_not_deadlocked() {
    let Some(fx) = fixture() else { return };
    let store = node(&fx, "env_wrong_thread");
    let error = fx.rt.block_on(async { store.get("anything") }).unwrap_err();
    assert_eq!(error.code(), codes::STORE_FAILED);
    assert!(
        error.to_string().contains("pg_bridge.wrong_thread"),
        "{error}"
    );
}

#[test]
fn the_constructor_refuses_a_bridge_that_does_not_fit_the_pool_or_a_current_thread_runtime() {
    let Some(fx) = fixture() else { return };
    let big = backend(&fx, 8);
    let bridge = Arc::new(Bridge::for_backend(bridge_config("env_ctor", 6), &big).unwrap());
    let small = backend(&fx, 4);
    let error = PgEnvironmentStore::new(Arc::clone(&bridge), &small.pool()).unwrap_err();
    assert_eq!(error.code(), pg_codes::SETUP_FAILED);
    let current = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let error = current
        .block_on(async { PgEnvironmentStore::new(Arc::clone(&bridge), &big.pool()) })
        .unwrap_err();
    assert_eq!(error.code(), pg_codes::SETUP_FAILED);
    PgEnvironmentStore::new(bridge, &big.pool()).unwrap();
}

#[test]
fn a_stopped_database_is_a_store_failure_with_no_partial_row_and_no_secret() {
    let Some(fx) = fixture() else { return };
    let good = node(&fx, "env_stopped_good");
    good.create(set("before", 1)).unwrap();
    // A pool pointing at a port nothing listens on, with a recognisable password.
    let mut config = fx.db.config();
    config.port = 1;
    config.password = "hunter2-secret".to_owned();
    config.pool_max_size = 8;
    config.connect_timeout = std::time::Duration::from_millis(500);
    config.pool_wait_timeout = std::time::Duration::from_millis(500);
    let dead = spot_postgres::PgBackend::connect_unchecked(config).unwrap();
    let bridge = Arc::new(Bridge::for_backend(bridge_config("env_stopped", 6), &dead).unwrap());
    let store = PgEnvironmentStore::new(bridge, &dead.pool()).unwrap();
    let errors = [
        store.create(set("ghost", 1)).unwrap_err(),
        store.get("before").unwrap_err(),
        store.replace(set("before", 2), 1).unwrap_err(),
        store.list().unwrap_err(),
        store.ensure_schema().unwrap_err(),
    ];
    for error in &errors {
        for text in [format!("{error}"), format!("{error:?}")] {
            for secret in [
                "hunter2",
                "127.0.0.1",
                "postgres://",
                "spot_environment_set",
                "SELECT",
            ] {
                assert!(!text.contains(secret), "{secret} in {text}");
            }
        }
    }
    for error in &errors[..4] {
        assert_eq!(error.code(), codes::STORE_FAILED);
    }
    // The real database holds only what was stored before: no partial row.
    let names: Vec<String> = good
        .list()
        .unwrap()
        .iter()
        .map(|s| s.set.name().to_owned())
        .collect();
    assert_eq!(names, ["before"]);
    assert_eq!(good.get("before").unwrap().version, 1);
}
