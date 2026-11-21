//! The schema and the security properties: the migrator accepts the tables, statements take input
//! as data, a backend failure or a corrupt row is an error (never an empty result), errors carry
//! no content, a refused write leaves nothing behind, a record above the cap is refused, and
//! racing writers have exactly one winner.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::{Arc, Barrier};
use std::thread;

use common::*;
use spot_bpm_conversations::{ConversationError, ConversationStore};
use spot_bpm_conversations_pg::{BAD_SETUP, MAX_BODY_BYTES, MIGRATION_FAILED};
use spot_link_exchange::RejectCode;
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
        assert!(code.starts_with("bpm_conversations_pg."));
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
                "SELECT indexname::text FROM pg_indexes \
                 WHERE tablename LIKE 'spot_bpm_conversation%' ORDER BY indexname",
                &[],
            )
            .await
            .unwrap()
            .iter()
            .map(|r| r.get(0))
            .collect()
    });
    for wanted in [
        "spot_bpm_conversation_pkey",
        "spot_bpm_conversation_case_idx",
        "spot_bpm_conversation_open_idx",
        "spot_bpm_conversation_waiting_idx",
        "spot_bpm_conversation_rejection_pkey",
        "spot_bpm_conversation_rejection_age_idx",
    ] {
        assert!(
            names.iter().any(|n| n == wanted),
            "missing {wanted} in {names:?}"
        );
    }
}

#[test]
fn input_is_data_never_sql() {
    let Some(fx) = fixture() else { return };
    let (_bridge, store) = open_store(fx.db.config(), "acme");
    let nasty = "x'); DROP TABLE spot_bpm_conversation; --";
    let l = link(1);
    let mut r = open_record(l, "c1", 1);
    r.case = Some(nasty.to_owned());
    r.role = nasty.to_owned();
    r.contract = nasty.to_owned();
    let stored = store.create(r).unwrap();
    assert_eq!(stored.case.as_deref(), Some(nasty));
    assert_eq!(store.of_case(nasty).unwrap().len(), 1);
    assert_eq!(
        store.load(&l, &conv("c1")).unwrap().unwrap().role,
        nasty,
        "stored and read back as plain text"
    );
    assert!(store.of_case("' OR ''='").unwrap().is_empty());
    // The tables are still there.
    assert_eq!(store.open_on(&l).unwrap(), 1);
}

#[test]
fn a_corrupt_row_is_an_error_not_skipped() {
    let Some(fx) = fixture() else { return };
    let (_bridge, store) = open_store(fx.db.config(), "acme");
    let l = link(1);
    for name in ["ok", "bad_body", "bad_cols", "extra_key", "bad_event"] {
        let mut r = open_record(l, name, 1);
        r.case = Some("case-1".to_owned());
        store.create(r).unwrap();
    }
    fx.rt.block_on(async {
        let client = fx.db.raw_client().await;
        for statement in [
            // A body that is not a record.
            "UPDATE spot_bpm_conversation SET body = '{\"version\": 7}'::jsonb \
             WHERE conversation = 'bad_body'",
            // A column that disagrees with its body.
            "UPDATE spot_bpm_conversation SET open = false WHERE conversation = 'bad_cols'",
            // A body with a key no record has.
            "UPDATE spot_bpm_conversation SET body = body || '{\"extra\": 1}'::jsonb \
             WHERE conversation = 'extra_key'",
            // An event whose direction is not one.
            "UPDATE spot_bpm_conversation SET body = jsonb_set(body, '{events,0,direction}', \
             '\"sideways\"'::jsonb) WHERE conversation = 'bad_event'",
        ] {
            client.batch_execute(statement).await.unwrap();
        }
    });
    for name in ["bad_body", "bad_cols", "extra_key", "bad_event"] {
        assert_eq!(
            store.load(&l, &conv(name)),
            Err(ConversationError::Store),
            "{name}"
        );
    }
    // Every scan that would include a corrupt row is an error, not a shorter list.
    assert_eq!(store.of_case("case-1"), Err(ConversationError::Store));
    // A save over a corrupt row still goes through the version check, not through a read; the
    // healthy row is readable on its own.
    assert!(store.load(&l, &conv("ok")).unwrap().is_some());
    // A corrupt rejection code is an error too.
    fx.rt.block_on(async {
        fx.db
            .raw_client()
            .await
            .batch_execute(&format!(
                "INSERT INTO spot_bpm_conversation_rejection VALUES ('acme', '{l}', '{}', 'bogus')",
                mid(9)
            ))
            .await
            .unwrap();
    });
    assert_eq!(store.rejection(&l, &mid(9)), Err(ConversationError::Store));
}

#[test]
fn a_backend_failure_is_an_error_without_content() {
    let Some(fx) = fixture() else { return };
    let (_bridge, store) = open_store(fx.db.config(), "acme");
    let l = link(1);
    let mut r = open_record(l, "c1", 1);
    r.case = Some("case-marker-7731".to_owned());
    let stored = store.create(r.clone()).unwrap();
    fx.rt.block_on(async {
        fx.db
            .raw_client()
            .await
            .batch_execute(
                "DROP TABLE spot_bpm_conversation; DROP TABLE spot_bpm_conversation_rejection",
            )
            .await
            .unwrap();
    });
    let errors = [
        store.load(&l, &conv("c1")).map(|_| ()).unwrap_err(),
        store.create(r.clone()).map(|_| ()).unwrap_err(),
        store.save(stored).map(|_| ()).unwrap_err(),
        store.waiting().map(|_| ()).unwrap_err(),
        store.of_case("case-marker-7731").map(|_| ()).unwrap_err(),
        store.open_on(&l).map(|_| ()).unwrap_err(),
        store.rejection(&l, &mid(1)).map(|_| ()).unwrap_err(),
        store
            .note_rejection(l, mid(1), RejectCode::NotGranted)
            .unwrap_err(),
    ];
    for error in errors {
        assert_eq!(error, ConversationError::Store);
        assert_eq!(error.code(), "bpm_conversations.store_failed");
        let shown = format!("{error} {error:?}");
        for leak in ["marker-7731", "spot_bpm_conversation", "SELECT", "INSERT"] {
            assert!(!shown.contains(leak), "`{leak}` leaked into: {shown}");
        }
    }
}

#[test]
fn a_refused_write_leaves_no_partial_row() {
    let Some(fx) = fixture() else { return };
    let (_bridge, store) = open_store(fx.db.config(), "acme");
    let l = link(1);
    let stored = store.create(rich_record(l, "c1")).unwrap();

    // A duplicate create, a stale save and a save to nothing: conflicts, nothing changes.
    assert_eq!(
        store.create(open_record(l, "c1", 9)),
        Err(ConversationError::Conflict)
    );
    let mut stale = stored.clone();
    stale.version = 9;
    stale.role = "changed".to_owned();
    assert_eq!(store.save(stale), Err(ConversationError::Conflict));
    assert_eq!(store.load(&l, &conv("c1")).unwrap().unwrap(), stored);

    // A record above the cap is refused before a statement runs, on create and on save, and the
    // stored record is exactly as it was.
    let big = |mut r: spot_bpm_conversations::ConversationRecord| {
        r.events.last_mut().unwrap().payload =
            Some(serde_json::json!({ "blob": "x".repeat(MAX_BODY_BYTES) }));
        r
    };
    assert_eq!(
        store.create(big(open_record(l, "huge", 2))),
        Err(ConversationError::TooLong)
    );
    assert!(store.load(&l, &conv("huge")).unwrap().is_none());
    let mut oversized = stored.clone();
    oversized.events.last_mut().unwrap().payload =
        Some(serde_json::json!({ "blob": "x".repeat(MAX_BODY_BYTES) }));
    assert_eq!(store.save(oversized), Err(ConversationError::TooLong));
    assert_eq!(store.load(&l, &conv("c1")).unwrap().unwrap(), stored);
    assert_eq!(store.waiting().unwrap(), vec![stored]);
}

#[test]
fn the_table_itself_refuses_a_body_above_the_cap() {
    let Some(fx) = fixture() else { return };
    let (_bridge, store) = open_store(fx.db.config(), "acme");
    store.create(open_record(link(1), "c1", 1)).unwrap();
    fx.rt.block_on(async {
        let client = fx.db.raw_client().await;
        let err = client
            .execute(
                "UPDATE spot_bpm_conversation SET body = body || \
                 jsonb_build_object('pad', repeat('x', $1::int))",
                &[&i32::try_from(MAX_BODY_BYTES).unwrap()],
            )
            .await
            .unwrap_err();
        assert_eq!(err.as_db_error().map(|d| d.code().code()), Some("23514"));
    });
    assert!(store.load(&link(1), &conv("c1")).unwrap().is_some());
}

#[test]
fn two_concurrent_saves_have_exactly_one_winner() {
    let Some(fx) = fixture() else { return };
    for round in 0..20 {
        let (_bridge, store) = open_store(fx.db.config(), &format!("race_{round}"));
        let l = link(1);
        let stored = store.create(open_record(l, "c1", 1)).unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let handles: Vec<_> = ["alice", "bob"]
            .into_iter()
            .map(|who| {
                let store = Arc::clone(&store);
                let barrier = Arc::clone(&barrier);
                let mut mine = stored.clone();
                mine.case = Some(who.to_owned());
                thread::spawn(move || {
                    barrier.wait();
                    store.save(mine)
                })
            })
            .collect();
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let winners: Vec<_> = results.iter().filter_map(|r| r.as_ref().ok()).collect();
        assert_eq!(winners.len(), 1, "exactly one save wins: {results:?}");
        assert!(results.contains(&Err(ConversationError::Conflict)));
        let after = store.load(&l, &conv("c1")).unwrap().unwrap();
        assert_eq!(after.version, 2);
        assert_eq!(after.case, winners[0].case);
    }
}

#[test]
fn many_threads_creating_one_key_make_one_record() {
    let Some(fx) = fixture() else { return };
    let (_bridge, store) = open_store(fx.db.config(), "acme");
    let l = link(1);
    let barrier = Arc::new(Barrier::new(16));
    let handles: Vec<_> = (0..16u32)
        .map(|i| {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let mut r = open_record(l, "c1", 1);
                r.case = Some(format!("case-{i}"));
                barrier.wait();
                store.create(r)
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|r| **r == Err(ConversationError::Conflict))
            .count(),
        15
    );
    assert_eq!(store.open_on(&l).unwrap(), 1);
}

#[test]
fn a_chain_of_retrying_writers_loses_no_update() {
    let Some(fx) = fixture() else { return };
    let (_bridge, store) = open_store(fx.db.config(), "acme");
    let l = link(1);
    let before: u32 = store
        .create(open_record(l, "c1", 1))
        .unwrap()
        .counts
        .values()
        .sum();
    let handles: Vec<_> = (0..8u32)
        .map(|n| {
            let store = Arc::clone(&store);
            thread::spawn(move || {
                for _ in 0..5u32 {
                    // 8 threads x 5 increments, each retrying on conflict.
                    // `update` gives up after 8 conflicts; eight racing writers may need more.
                    loop {
                        let mut r = store.load(&l, &conv("c1")).unwrap().unwrap();
                        *r.counts.entry(format!("t{n}")).or_insert(0) += 1;
                        match store.save(r) {
                            Ok(_) => break,
                            Err(ConversationError::Conflict) => {}
                            Err(other) => panic!("{other:?}"),
                        }
                    }
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let after = store.load(&l, &conv("c1")).unwrap().unwrap();
    assert_eq!(after.version, 41);
    assert_eq!(after.counts.values().sum::<u32>(), before + 40);
}

#[test]
fn the_rejection_memory_is_bounded_and_forgets_the_oldest() {
    let Some(fx) = fixture() else { return };
    let (_bridge, store) = open_store(fx.db.config(), "acme");
    let l = link(1);
    let cap = spot_bpm_conversations::record::MAX_REMEMBERED_REJECTIONS;
    // Preload `cap` rejections in one statement (ids older than any `mid`), then add one newer
    // one through the store.
    let id_of = |n: u64| format!("01800000-0000-7000-8000-{n:012x}");
    fx.rt.block_on(async {
        fx.db
            .raw_client()
            .await
            .execute(
                "INSERT INTO spot_bpm_conversation_rejection \
                 SELECT 'acme', $1, '01800000-0000-7000-8000-' || lpad(to_hex(g), 12, '0'), \
                        'not_granted' FROM generate_series(1, $2::int) g",
                &[&l.to_string(), &i32::try_from(cap).unwrap()],
            )
            .await
            .unwrap();
    });
    let parse = |n: u64| id_of(n).parse::<spot_link_exchange::MessageId>().unwrap();
    assert_eq!(
        store.rejection(&l, &parse(1)).unwrap(),
        Some(RejectCode::NotGranted)
    );
    store
        .note_rejection(l, mid(1_000_000), RejectCode::PayloadInvalid)
        .unwrap();
    assert_eq!(
        store.rejection(&l, &parse(1)).unwrap(),
        None,
        "the oldest is forgotten"
    );
    assert!(store.rejection(&l, &parse(2)).unwrap().is_some());
    assert_eq!(
        store.rejection(&l, &mid(1_000_000)).unwrap(),
        Some(RejectCode::PayloadInvalid)
    );
    let count: i64 = fx.rt.block_on(async {
        fx.db
            .raw_client()
            .await
            .query_one("SELECT count(*) FROM spot_bpm_conversation_rejection", &[])
            .await
            .unwrap()
            .get(0)
    });
    assert_eq!(usize::try_from(count).unwrap(), cap);
}

#[test]
fn a_call_from_inside_a_runtime_is_refused_not_run() {
    let Some(fx) = fixture() else { return };
    let (_bridge, store) = open_store(fx.db.config(), "acme");
    let l = link(1);
    let result = fx.rt.block_on(async { store.load(&l, &conv("c1")) });
    assert_eq!(result, Err(ConversationError::Store));
    // The same call from a plain thread works.
    assert!(store.load(&l, &conv("c1")).unwrap().is_none());
}
