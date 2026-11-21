//! The `QuarantineStore` behaviour of `PgQuarantineStore` on a real database: ids and order,
//! the payload round trip, metadata-only updates, `find`, filtered `list`, write-once payloads,
//! restart persistence and fail-closed refusals.
//!
//! Skips cleanly (a `SKIPPED:` line from `TestDb::create`) when `SPOT_TEST_PG_URL` is unset; the
//! CI Postgres service sets it and the suite runs.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{close, ghost, name, note, put, record, record_from, sender, source, store};
use spot_postgres::testing::TestDb;
use spot_quarantine::{
    ClosureOutcome, ItemFilter, ItemId, NewItem, QuarantineError, QuarantineStore, RecordRef, State,
};
use spot_storage_contract::RecordId;
use spot_sync_protocol::RecordName;

#[tokio::test]
async fn ids_increase_with_insertion_and_list_keeps_insertion_order() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, "order").await;
    let mut ids = Vec::new();
    for seq in 1..=5_u64 {
        let rec = record(ghost(RecordId::new_v7(), "x", 1), 1, seq, vec![]);
        let item = put(&store, &rec, "replica_apply.unknown_entity", 100).await;
        assert_eq!(item.state, State::Held);
        assert_eq!(item.attempts, 0);
        ids.push(item.id);
    }
    assert!(ids.windows(2).all(|w| w[0] < w[1]), "ids increase: {ids:?}");
    let listed = store.list(&ItemFilter::all()).await.unwrap();
    assert_eq!(listed.iter().map(|i| i.id).collect::<Vec<_>>(), ids);
    assert_eq!(
        listed.iter().map(|i| i.seq).collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5]
    );
}

#[tokio::test]
async fn the_payload_comes_back_exactly_as_it_arrived() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, "payload").await;
    let mut rec = record(
        note(RecordId::new_v7(), "root", 3),
        3,
        9,
        vec![
            note(RecordId::new_v7(), "entry one", 1),
            note(RecordId::new_v7(), "entry two", 1),
        ],
    );
    rec.topic = Some(name("link_orders"));
    rec.conversation = Some("conv-7".to_owned());
    rec.meta_version = 4;
    let item = put(&store, &rec, "replica_apply.unknown_field", 5).await;

    let stored = store.record(item.id).await.unwrap().expect("a payload");
    assert_eq!(stored, rec);
    assert_eq!(store.item(item.id).await.unwrap(), Some(item));
}

#[tokio::test]
async fn unknown_ids_are_none_or_not_found() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, "unknown").await;
    let rec = record(ghost(RecordId::new_v7(), "x", 1), 1, 1, vec![]);
    let item = put(&store, &rec, "r", 0).await;

    assert_eq!(store.item(ItemId(9_999)).await.unwrap(), None);
    assert!(store.record(ItemId(9_999)).await.unwrap().is_none());
    // An id past what the database can hold is unknown too, not an error.
    assert_eq!(store.item(ItemId(u64::MAX)).await.unwrap(), None);
    assert!(store.record(ItemId(u64::MAX)).await.unwrap().is_none());

    let mut ghosted = item.clone();
    ghosted.id = ItemId(9_999);
    assert_eq!(
        store.update(&ghosted).await.unwrap_err(),
        QuarantineError::NotFound { id: ItemId(9_999) }
    );
    ghosted.id = ItemId(u64::MAX);
    assert_eq!(
        store.update(&ghosted).await.unwrap_err(),
        QuarantineError::NotFound {
            id: ItemId(u64::MAX)
        }
    );
}

#[tokio::test]
async fn update_replaces_metadata_and_leaves_the_payload_alone() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, "update").await;
    let rec = record(ghost(RecordId::new_v7(), "x", 1), 1, 1, vec![]);
    let mut item = put(&store, &rec, "replica_apply.unknown_entity", 10).await;

    item.attempts = 2;
    item.state = State::Retrying;
    item.reason_code = "replica_apply.unknown_field".to_owned();
    item.detail = "changed".to_owned();
    store.update(&item).await.unwrap();
    assert_eq!(store.item(item.id).await.unwrap(), Some(item.clone()));

    let released = close(&store, &item, ClosureOutcome::Released, "ada", 99)
        .await
        .unwrap();
    assert_eq!(released.state, State::Released);
    let closure = store.closure(item.id).await.unwrap().expect("a closure");
    assert_eq!((closure.by.as_str(), closure.at), ("ada", 99));
    assert_eq!(
        store.item(item.id).await.unwrap().unwrap().state,
        State::Released
    );
    assert_eq!(
        store.record(item.id).await.unwrap().expect("payload kept"),
        rec,
        "release keeps the payload for the record"
    );
}

#[tokio::test]
async fn identity_changes_and_reopening_a_closed_item_are_refused() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, "refused").await;
    let rec = record(ghost(RecordId::new_v7(), "x", 1), 1, 4, vec![]);
    let item = put(&store, &rec, "r", 10).await;

    type Tamper = fn(&mut spot_quarantine::QuarantineItem);
    let tampers: [Tamper; 5] = [
        |i| i.seq = 5,
        |i| i.first_seen = 11,
        |i| i.source = sender("other.site.app#hub"),
        |i| i.record_ref.version = 2,
        |i| i.record_ref.entity = "note".to_owned(),
    ];
    for tamper in tampers {
        let mut changed = item.clone();
        tamper(&mut changed);
        assert!(
            matches!(
                store.update(&changed).await,
                Err(QuarantineError::Store { .. })
            ),
            "an identity change is refused"
        );
    }
    assert_eq!(store.item(item.id).await.unwrap(), Some(item.clone()));

    let discarded = close(&store, &item, ClosureOutcome::Discarded, "ada", 12)
        .await
        .unwrap();
    {
        let mut change = discarded.clone();
        change.state = State::Held;
        assert!(matches!(
            store.update(&change).await,
            Err(QuarantineError::ItemClosed { .. })
        ));
    }
    assert_eq!(
        store.item(item.id).await.unwrap().unwrap().state,
        State::Discarded,
        "a discarded item stays discarded"
    );
    // Even a re-save of the same closed item is refused: closed is terminal.
    assert!(matches!(
        store.update(&discarded).await,
        Err(QuarantineError::ItemClosed { .. })
    ));
}

#[tokio::test]
async fn find_returns_the_open_item_of_that_sender_only() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, "find").await;
    let id = RecordId::new_v7();
    let hub = source();
    let branch = sender("branch.shop.app#hub");

    let a = record_from(&hub, ghost(id, "x", 1), 1, 1, vec![]);
    let b = record_from(&branch, ghost(id, "x", 1), 1, 1, vec![]);
    assert_eq!(a.name, b.name, "same record name");
    let item_a = put(&store, &a, "r", 1).await;
    let item_b = put(&store, &b, "r", 2).await;
    assert_ne!(
        item_a.id, item_b.id,
        "two senders never collide on name and version"
    );

    assert_eq!(
        store.find(&hub, &a.name, 1).await.unwrap().map(|i| i.id),
        Some(item_a.id)
    );
    assert_eq!(
        store.find(&branch, &a.name, 1).await.unwrap().map(|i| i.id),
        Some(item_b.id)
    );
    assert_eq!(
        store.find(&hub, &a.name, 2).await.unwrap(),
        None,
        "another version"
    );
    let other = RecordName::new("note:nothing").unwrap();
    assert_eq!(store.find(&hub, &other, 1).await.unwrap(), None);
    assert_eq!(store.find(&hub, &a.name, u64::MAX).await.unwrap(), None);

    // Retrying is still open; released and discarded are not.
    let mut moved = item_a.clone();
    moved.state = State::Retrying;
    store.update(&moved).await.unwrap();
    assert!(store.find(&hub, &a.name, 1).await.unwrap().is_some());
    close(&store, &moved, ClosureOutcome::Released, "ada", 5)
        .await
        .unwrap();
    assert_eq!(store.find(&hub, &a.name, 1).await.unwrap(), None);
    assert!(store.find(&branch, &a.name, 1).await.unwrap().is_some());

    // Once closed, the same key can be quarantined again.
    let again = put(&store, &a, "r", 3).await;
    assert_eq!(
        store.find(&hub, &a.name, 1).await.unwrap().map(|i| i.id),
        Some(again.id)
    );
}

#[tokio::test]
async fn a_second_open_item_with_the_same_key_fails_whole_and_leaks_nothing() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, "dupe").await;
    let rec = record(ghost(RecordId::new_v7(), "secret title", 1), 1, 1, vec![]);
    put(&store, &rec, "r", 1).await;

    let err = store
        .insert(common::new_item(&rec, "r", 2), rec.clone())
        .await
        .unwrap_err();
    assert!(matches!(err, QuarantineError::Store { .. }));
    let text = format!("{err} {err:?}");
    assert!(
        !text.contains(rec.name.as_str()),
        "no record name in {text}"
    );
    assert!(!text.contains("secret title"), "no field value in {text}");
    assert_eq!(
        store.list(&ItemFilter::all()).await.unwrap().len(),
        1,
        "the failed insert left no item and no payload"
    );
    let raw = db.raw_client().await;
    let payloads: i64 = raw
        .query_one("SELECT count(*) FROM spot_quarantine_payload", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(payloads, 1);
}

#[tokio::test]
async fn an_item_that_does_not_describe_its_record_is_refused() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, "mismatch").await;
    let rec = record(ghost(RecordId::new_v7(), "x", 1), 1, 1, vec![]);
    let other = record(ghost(RecordId::new_v7(), "y", 1), 1, 1, vec![]);

    let wrong_ref = NewItem::held(
        rec.source.clone(),
        rec.seq,
        RecordRef::of(&other),
        "r",
        "d",
        0,
    );
    assert!(store.insert(wrong_ref, rec.clone()).await.is_err());
    let wrong_source = NewItem::held(
        sender("branch.shop.app#hub"),
        rec.seq,
        RecordRef::of(&rec),
        "r",
        "d",
        0,
    );
    assert!(store.insert(wrong_source, rec.clone()).await.is_err());
    let wrong_seq = NewItem::held(rec.source.clone(), 99, RecordRef::of(&rec), "r", "d", 0);
    assert!(store.insert(wrong_seq, rec.clone()).await.is_err());
    assert!(store.list(&ItemFilter::all()).await.unwrap().is_empty());
}

#[tokio::test]
async fn list_applies_every_filter_in_sql_and_keeps_insertion_order() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, "filters").await;
    let branch = sender("branch.shop.app#hub");
    let mut ids = Vec::new();
    for (n, (from, reason)) in [
        (source(), "replica_apply.unknown_entity"),
        (branch.clone(), "replica_apply.unknown_entity"),
        (source(), "replica_apply.unknown_field"),
        (branch.clone(), "replica_apply.unknown_field"),
    ]
    .into_iter()
    .enumerate()
    {
        let rec = record_from(
            &from,
            ghost(RecordId::new_v7(), "x", 1),
            1,
            1 + n as u64,
            vec![],
        );
        ids.push(put(&store, &rec, reason, 0).await.id);
    }
    // Move item 2 to released.
    let second = store.item(ids[1]).await.unwrap().unwrap();
    close(&store, &second, ClosureOutcome::Released, "ada", 5)
        .await
        .unwrap();

    let list = |f: ItemFilter| {
        let store = &store;
        async move {
            store
                .list(&f)
                .await
                .unwrap()
                .into_iter()
                .map(|i| i.id)
                .collect::<Vec<_>>()
        }
    };
    assert_eq!(list(ItemFilter::all()).await, ids);
    assert_eq!(
        list(ItemFilter {
            state: Some(State::Held),
            ..ItemFilter::all()
        })
        .await,
        vec![ids[0], ids[2], ids[3]]
    );
    assert_eq!(
        list(ItemFilter {
            state: Some(State::Released),
            ..ItemFilter::all()
        })
        .await,
        vec![ids[1]]
    );
    assert_eq!(
        list(ItemFilter {
            reason_code: Some("replica_apply.unknown_field".to_owned()),
            ..ItemFilter::all()
        })
        .await,
        vec![ids[2], ids[3]]
    );
    assert_eq!(
        list(ItemFilter {
            source: Some(branch.clone()),
            ..ItemFilter::all()
        })
        .await,
        vec![ids[1], ids[3]]
    );
    assert_eq!(
        list(ItemFilter {
            state: Some(State::Held),
            reason_code: Some("replica_apply.unknown_field".to_owned()),
            source: Some(branch),
        })
        .await,
        vec![ids[3]]
    );
    // A filter value that looks like SQL is only a value.
    assert!(
        list(ItemFilter {
            reason_code: Some("x' OR '1'='1".to_owned()),
            ..ItemFilter::all()
        })
        .await
        .is_empty()
    );
}

#[tokio::test]
async fn the_payload_and_the_items_are_write_once_even_for_a_raw_client() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = store(&db, "immutable").await;
    let rec = record(ghost(RecordId::new_v7(), "x", 1), 1, 1, vec![]);
    let item = put(&store, &rec, "r", 1).await;
    let id = i64::try_from(item.id.0).unwrap();

    let raw = db.raw_client().await;
    for statement in [
        "UPDATE spot_quarantine_payload SET payload = '{}' WHERE item_id = $1",
        "UPDATE spot_quarantine_payload SET topic = 'x' WHERE item_id = $1",
        "DELETE FROM spot_quarantine_payload WHERE item_id = $1",
        "DELETE FROM spot_quarantine_item WHERE id = $1",
    ] {
        let err = raw.execute(statement, &[&id]).await.unwrap_err();
        assert!(err.as_db_error().is_some(), "{statement} must be refused");
    }
    for statement in [
        "TRUNCATE spot_quarantine_payload",
        "TRUNCATE spot_quarantine_item CASCADE",
    ] {
        assert!(raw.batch_execute(statement).await.is_err(), "{statement}");
    }

    assert_eq!(store.record(item.id).await.unwrap().expect("payload"), rec);
    assert_eq!(store.item(item.id).await.unwrap(), Some(item));
}

#[tokio::test]
async fn a_restart_loses_nothing_and_ids_continue() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let mut recs = Vec::new();
    {
        let first = store(&db, "before").await;
        for seq in 1..=3_u64 {
            let rec = record(ghost(RecordId::new_v7(), "money", 1), 1, seq, vec![]);
            put(&first, &rec, "replica_apply.unknown_entity", 7).await;
            recs.push(rec);
        }
        let mut second = first.item(ItemId(2)).await.unwrap().unwrap();
        second.attempts = 3;
        second.state = State::Retrying;
        first.update(&second).await.unwrap();
        // The first store and its pool are dropped here: the process "restarts".
    }

    let after = store(&db, "after").await; // prepare again over existing tables
    let items = after.list(&ItemFilter::all()).await.unwrap();
    assert_eq!(
        items.iter().map(|i| i.id).collect::<Vec<_>>(),
        [ItemId(1), ItemId(2), ItemId(3)]
    );
    assert_eq!(items[1].attempts, 3);
    assert_eq!(items[1].state, State::Retrying);
    for (item, rec) in items.iter().zip(&recs) {
        assert_eq!(after.record(item.id).await.unwrap().as_ref(), Some(rec));
    }
    let next = put(
        &after,
        &record(ghost(RecordId::new_v7(), "x", 1), 1, 4, vec![]),
        "r",
        8,
    )
    .await;
    assert_eq!(
        next.id,
        ItemId(4),
        "the id counter continues from the database"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn prepare_is_idempotent_and_safe_from_several_nodes_at_once() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let stores: Vec<_> = (0..4)
        .map(|n| spot_quarantine_pg::PgQuarantineStore::new(common::pool(&db, 2, &format!("p{n}"))))
        .collect();
    let mut tasks = Vec::new();
    for s in stores {
        tasks.push(tokio::spawn(async move { s.prepare().await }));
    }
    for t in tasks {
        t.await.unwrap().unwrap();
    }
    store(&db, "again").await; // a further prepare over finished tables
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_inserts_get_distinct_ids() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = std::sync::Arc::new(store(&db, "conc").await);
    let mut tasks = Vec::new();
    for seq in 1..=20_u64 {
        let store = std::sync::Arc::clone(&store);
        tasks.push(tokio::spawn(async move {
            let rec = record(ghost(RecordId::new_v7(), "x", 1), 1, seq, vec![]);
            put(&store, &rec, "r", 0).await.id
        }));
    }
    let mut ids = Vec::new();
    for t in tasks {
        ids.push(t.await.unwrap());
    }
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 20);
    assert_eq!(store.list(&ItemFilter::all()).await.unwrap().len(), 20);
}
