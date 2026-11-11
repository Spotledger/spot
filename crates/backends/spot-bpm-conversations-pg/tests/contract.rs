//! One contract, run against the in-memory reference and the PostgreSQL store: every behaviour
//! of `MemoryConversationStore` must hold for `PgConversationStore`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_bpm_conversations::{
    ConversationError, ConversationStore, MemoryConversationStore, update,
};
use spot_link_exchange::RejectCode;

/// The shared contract. The store holds nothing when it is called.
fn contract_of(store: &dyn ConversationStore) {
    let (a, b) = (link(1), link(2));

    // Nothing there yet.
    assert!(store.load(&a, &conv("c1")).unwrap().is_none());
    assert!(store.waiting().unwrap().is_empty());
    assert!(store.of_case("case-1").unwrap().is_empty());
    assert_eq!(store.open_on(&a).unwrap(), 0);

    // create: stored at version 1, returned as stored, read back equal.
    let rich = rich_record(a, "c1");
    let created = store.create(rich.clone()).unwrap();
    assert_eq!(created.version, 1);
    assert_eq!(store.load(&a, &conv("c1")).unwrap().unwrap(), created);
    let mut expected = rich.clone();
    expected.version = 1;
    assert_eq!(created, expected, "every field survives the round trip");

    // create on an existing key is a conflict and writes nothing, even with other content.
    let mut other = open_record(a, "c1", 9);
    other.role = "someone_else".to_owned();
    assert_eq!(store.create(other), Err(ConversationError::Conflict));
    assert_eq!(store.load(&a, &conv("c1")).unwrap().unwrap(), created);

    // The same name on another link is another record.
    let on_b = store.create(open_record(b, "c1", 3)).unwrap();
    assert_eq!(on_b.link, b);
    assert_eq!(store.load(&a, &conv("c1")).unwrap().unwrap(), created);

    // save: compare-and-set on the version, returning the record as stored.
    let mut next = created.clone();
    next.compensated = false;
    next.attention = None;
    let saved = store.save(next.clone()).unwrap();
    assert_eq!(saved.version, 2);
    assert!(!saved.compensated && saved.attention.is_none());
    assert_eq!(store.load(&a, &conv("c1")).unwrap().unwrap(), saved);

    // A stale version, a missing record, a record never created: all conflicts, nothing written.
    assert_eq!(store.save(next), Err(ConversationError::Conflict));
    let ghost = open_record(a, "ghost", 4);
    assert_eq!(store.save(ghost), Err(ConversationError::Conflict));
    assert!(store.load(&a, &conv("ghost")).unwrap().is_none());
    assert_eq!(store.load(&a, &conv("c1")).unwrap().unwrap(), saved);

    // update retries from a fresh load and returns the stored record.
    let (answer, after) = update(store, &a, &conv("c1"), |r| {
        r.case = Some("case-2".to_owned());
        Ok(41 + 1)
    })
    .unwrap();
    assert_eq!(answer, 42);
    assert_eq!(after.version, 3);
    assert_eq!(after.case.as_deref(), Some("case-2"));
    assert!(matches!(
        update(store, &a, &conv("nope"), |_| Ok(())),
        Err(ConversationError::NoConversation)
    ));

    // of_case, in key order; waiting, in key order.
    assert_eq!(store.of_case("case-2").unwrap().len(), 1);
    assert!(store.of_case("case-1").unwrap().is_empty());
    let mut with_case = open_record(b, "c2", 5);
    with_case.case = Some("case-2".to_owned());
    store.create(with_case).unwrap();
    let of_case = store.of_case("case-2").unwrap();
    assert_eq!(
        of_case
            .iter()
            .map(|r| (r.link, r.conversation.to_string()))
            .collect::<Vec<_>>(),
        vec![(a, "c1".to_owned()), (b, "c2".to_owned())]
    );
    let waiting = store.waiting().unwrap();
    assert_eq!(waiting.len(), 1, "only c1 on A has waits");
    assert_eq!(waiting[0].conversation, conv("c1"));

    // open_on counts records without an ending, per link.
    // The rich record has received its closing message, so it is not open.
    assert!(created.ending.is_some());
    assert_eq!(store.open_on(&a).unwrap(), 0);
    assert_eq!(store.open_on(&b).unwrap(), 2);
    let (_, ended) = update(store, &b, &conv("c2"), |r| {
        r.append_received(&contract(), "approve", mid(6), 4_000, serde_json::json!({}))
    })
    .unwrap();
    assert!(ended.ending.is_some());
    assert_eq!(store.open_on(&b).unwrap(), 1);
    assert_eq!(store.open_on(&link(3)).unwrap(), 0);

    // Clearing the waits takes the record out of waiting().
    update(store, &a, &conv("c1"), |r| {
        r.waits.clear();
        Ok(())
    })
    .unwrap();
    assert!(store.waiting().unwrap().is_empty());

    // Rejections are per link and replace the earlier code for the same key.
    assert_eq!(store.rejection(&a, &mid(1)).unwrap(), None);
    store
        .note_rejection(a, mid(1), RejectCode::PayloadInvalid)
        .unwrap();
    assert_eq!(
        store.rejection(&a, &mid(1)).unwrap(),
        Some(RejectCode::PayloadInvalid)
    );
    assert_eq!(
        store.rejection(&b, &mid(1)).unwrap(),
        None,
        "the same id on another link is another message"
    );
    store
        .note_rejection(a, mid(1), RejectCode::NotGranted)
        .unwrap();
    assert_eq!(
        store.rejection(&a, &mid(1)).unwrap(),
        Some(RejectCode::NotGranted)
    );
    for (n, code) in [
        RejectCode::VersionUnsupported,
        RejectCode::OutOfOrderExpired,
        RejectCode::CapacityExceeded,
    ]
    .into_iter()
    .enumerate()
    {
        let id = mid(100 + u32::try_from(n).unwrap());
        store.note_rejection(b, id, code).unwrap();
        assert_eq!(store.rejection(&b, &id).unwrap(), Some(code));
    }
}

#[test]
fn the_memory_store_satisfies_the_contract() {
    contract_of(&MemoryConversationStore::new());
}

#[test]
fn the_postgres_store_satisfies_the_contract() {
    let Some(fx) = fixture() else { return };
    let (_bridge, store) = open_store(fx.db.config(), "acme");
    contract_of(&*store);
}

#[test]
fn a_record_survives_a_restart() {
    let Some(fx) = fixture() else { return };
    let (bridge, store) = open_store(fx.db.config(), "acme");
    let a = link(1);
    let created = store.create(rich_record(a, "c1")).unwrap();
    let saved = store.save(created).unwrap();
    store
        .note_rejection(a, mid(77), RejectCode::PayloadInvalid)
        .unwrap();
    drop(store);
    bridge.shutdown(std::time::Duration::from_secs(2)).unwrap();

    // A new process: a new bridge, a new pool, a new store; prepare is idempotent.
    let (_bridge2, again) = open_store(fx.db.config(), "acme");
    assert_eq!(again.load(&a, &conv("c1")).unwrap().unwrap(), saved);
    assert_eq!(again.waiting().unwrap(), vec![saved.clone()]);
    assert_eq!(
        again.rejection(&a, &mid(77)).unwrap(),
        Some(RejectCode::PayloadInvalid)
    );
    // And it can carry on: the next save continues the version sequence.
    let next = again.save(saved).unwrap();
    assert_eq!(next.version, 3);
}
