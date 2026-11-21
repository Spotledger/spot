//! Cross-tenant and cross-link isolation: every method of the store, with two tenants on one
//! database and two links under one tenant.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_bpm_conversations::{ConversationError, ConversationStore};
use spot_link_exchange::RejectCode;

#[test]
fn a_tenant_sees_nothing_of_another_in_any_method() {
    let Some(fx) = fixture() else { return };
    let (bridge, a) = open_store(fx.db.config(), "tenant_a");
    let b = sibling(&bridge, fx.db.config(), "tenant_b");
    let l = link(1);

    let mine = a.create(rich_record(l, "c1")).unwrap();
    a.note_rejection(l, mid(5), RejectCode::NotGranted).unwrap();

    // Every read of B comes back empty.
    assert!(b.load(&l, &conv("c1")).unwrap().is_none());
    assert!(b.waiting().unwrap().is_empty());
    assert!(b.of_case("case-1").unwrap().is_empty());
    assert_eq!(b.open_on(&l).unwrap(), 0);
    assert_eq!(b.rejection(&l, &mid(5)).unwrap(), None);

    // B cannot save A's record: it does not exist for B, and nothing of A moved.
    let mut hijack = mine.clone();
    hijack.role = "mallory".to_owned();
    assert_eq!(b.save(hijack), Err(ConversationError::Conflict));
    assert_eq!(a.load(&l, &conv("c1")).unwrap().unwrap(), mine);

    // B can create the same key: its own, independent record at version 1.
    let theirs = b.create(open_record(l, "c1", 8)).unwrap();
    assert_eq!(theirs.version, 1);
    assert!(theirs.waits.is_empty());
    assert_eq!(a.load(&l, &conv("c1")).unwrap().unwrap(), mine);
    assert_eq!(b.load(&l, &conv("c1")).unwrap().unwrap(), theirs);

    // B's rejection for the same id does not change A's answer, and the reverse.
    b.note_rejection(l, mid(5), RejectCode::PayloadInvalid)
        .unwrap();
    assert_eq!(
        a.rejection(&l, &mid(5)).unwrap(),
        Some(RejectCode::NotGranted)
    );
    assert_eq!(
        b.rejection(&l, &mid(5)).unwrap(),
        Some(RejectCode::PayloadInvalid)
    );

    // Each scan holds only its own.
    assert_eq!(a.waiting().unwrap(), vec![mine.clone()]);
    assert!(b.waiting().unwrap().is_empty());
    assert_eq!(a.of_case("case-1").unwrap().len(), 1);
    assert!(b.of_case("case-1").unwrap().is_empty());
    assert_eq!(a.open_on(&l).unwrap(), 0, "A's record has ended");
    assert_eq!(b.open_on(&l).unwrap(), 1, "B's record is open");
}

#[test]
fn a_record_of_one_link_is_invisible_under_another() {
    let Some(fx) = fixture() else { return };
    let (_bridge, store) = open_store(fx.db.config(), "acme");
    let (la, lb) = (link(1), link(2));
    let on_a = store.create(rich_record(la, "c1")).unwrap();

    assert!(store.load(&lb, &conv("c1")).unwrap().is_none());
    assert_eq!(store.open_on(&lb).unwrap(), 0);
    assert_eq!(store.rejection(&lb, &mid(1)).unwrap(), None);

    // A save addressed to link B does not touch the record of link A.
    let mut wrong = on_a.clone();
    wrong.link = lb;
    assert_eq!(store.save(wrong), Err(ConversationError::Conflict));
    assert_eq!(store.load(&la, &conv("c1")).unwrap().unwrap(), on_a);

    // Scans return the record with its own link, so a caller can tell them apart.
    store.create(open_record(lb, "c1", 4)).unwrap();
    assert_eq!(store.open_on(&la).unwrap(), 0);
    assert_eq!(store.open_on(&lb).unwrap(), 1);
    let waiting = store.waiting().unwrap();
    assert_eq!(waiting.len(), 1);
    assert_eq!(waiting[0].link, la);
}

#[test]
fn the_tenant_name_must_be_usable() {
    let Some(fx) = fixture() else { return };
    let (bridge, pool) = open_bridge(fx.db.config());
    for bad in ["", &"t".repeat(129), "a\nb", "a\u{0}b"] {
        let err = spot_bpm_conversations_pg::PgConversationStore::new(
            std::sync::Arc::clone(&bridge),
            &pool,
            bad,
        )
        .unwrap_err();
        assert_eq!(err.code(), "bpm_conversations_pg.bad_setup");
    }
}
