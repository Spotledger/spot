//! One ledger under the relays of two tenants through `SharedBook` (WP-SYN-02.T11): the relay
//! supplies its own tenant on every call, so the tenants stay apart.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use common::*;
use spot_conditions::MemorySink;
use spot_sync_acks::{AckError, AckLedger, MemoryStore, NodeStateStore, StreamHead};
use spot_sync_hub::{
    AckBook, HubRelay, LedgerBook, MemoryStream, RelayConfig, SharedBook, SharedLedgerBook,
    StreamEntry,
};
use spot_sync_obligations::{Lane, ObligationSettings};
use std::panic::{AssertUnwindSafe, catch_unwind};

type Book = SharedLedgerBook<MemoryStore, MemorySink>;
type Relay = HubRelay<SharedQueue, MemoryStream, Src>;

const A: &str = "a";
const B: &str = "b";
const NOW: i64 = 1_000_000;

fn ledger_book() -> LedgerBook<MemoryStore, MemorySink> {
    LedgerBook::new(
        AckLedger::with_catalogue(
            MemoryStore::new(),
            MemorySink::new(),
            ObligationSettings::default(),
        )
        .unwrap(),
    )
}

fn shared() -> Book {
    SharedBook::new(ledger_book())
}

/// Entries `1..=through` of one tenant, all on the mandatory lane of the one entity `tax`.
fn stream_of(tenant: &str, through: u64) -> Vec<StreamEntry> {
    (1..=through)
        .map(|p| {
            let mut e = entry(p, &format!("x{p}"), "tax", &format!("R{p}"), 1);
            tenant.clone_into(&mut e.tenant);
            e.lane = Lane::Mandatory;
            e
        })
        .collect()
}

/// A relay for `tenant` over `through` stream entries, with `n1` registered, on the shared book.
fn relay(tenant: &str, through: u64, book: &Book) -> Relay {
    let mut stream = MemoryStream::new();
    for e in stream_of(tenant, through) {
        stream.append(e);
    }
    let mut r = HubRelay::new(
        RelayConfig::new(tenant, SOURCE),
        stream,
        SharedQueue::default(),
        Src::default(),
        Box::new(Refs),
    )
    .unwrap();
    r.set_ledger(Box::new(book.clone()));
    r.add_destination(dest("n1", &["tax"])).unwrap();
    assert!(r.register_node("n1", "1", 0).unwrap());
    r
}

fn mandatory_at(r: &mut Relay, tenant: &str, position: u64) {
    let entry = stream_of(tenant, position).pop().unwrap();
    assert!(r.register_mandatory(&entry, NOW, None).unwrap());
}

fn record(book: &Book, tenant: &str) -> spot_sync_acks::NodeRecord {
    book.with_book(|b| b.ledger().store().load("n1", tenant))
        .unwrap()
}

#[test]
fn an_ack_through_one_relay_never_satisfies_the_other_tenants_event_at_the_same_position() {
    let book = shared();
    let mut ra = relay(A, 5, &book);
    let mut rb = relay(B, 5, &book);
    mandatory_at(&mut ra, A, 5);
    mandatory_at(&mut rb, B, 5);
    ra.tick(NOW).unwrap();
    rb.tick(NOW).unwrap();
    assert_eq!(ra.who_is_missing(5).unwrap().len(), 1);
    assert_eq!(rb.who_is_missing(5).unwrap().len(), 1);

    ra.on_ack_at("n1", 5, NOW + 1).unwrap();

    assert!(ra.who_is_missing(5).unwrap().is_empty());
    let missing = rb.who_is_missing(5).unwrap();
    assert_eq!(missing.len(), 1);
    assert_eq!(missing[0].node, "n1");
    assert_eq!(missing[0].tenant, B);
    assert_eq!(record(&book, A).applied_prefix, 5);
    assert_eq!(record(&book, B).applied_prefix, 0);
}

#[test]
fn a_check_in_through_one_relay_changes_only_its_own_tenant() {
    let book = shared();
    let mut ra = relay(A, 5, &book);
    let _rb = relay(B, 5, &book);
    let before_b = record(&book, B);
    assert_eq!(record(&book, A).last_contact_millis, 0);

    ra.on_checkin_at("n1", 500).unwrap();

    assert_eq!(record(&book, A).last_contact_millis, 500);
    assert_eq!(record(&book, B), before_b);
    assert_eq!(record(&book, B).last_contact_millis, 0);
}

#[test]
fn a_suspect_ack_is_refused_for_its_own_relay_and_changes_nothing_in_the_other() {
    let book = shared();
    let mut ra = relay(A, 5, &book);
    let mut rb = relay(B, 5, &book);
    ra.tick(NOW).unwrap();
    rb.tick(NOW).unwrap();
    let before_a = record(&book, A);
    let before_b = record(&book, B);

    // One above relay A's own head.
    assert!(ra.on_ack_at("n1", 6, NOW + 1).is_err());

    assert_eq!(ra.tick(NOW + 2).unwrap().suspect, vec!["n1".to_owned()]);
    assert!(rb.tick(NOW + 2).unwrap().suspect.is_empty());
    assert_eq!(record(&book, A), before_a);
    assert_eq!(record(&book, B), before_b);
}

#[test]
fn holds_set_through_one_relay_leave_the_other_tenants_holds_alone() {
    let book = shared();
    let mut ra = relay(A, 5, &book);
    let mut rb = relay(B, 5, &book);
    mandatory_at(&mut ra, A, 5);
    mandatory_at(&mut rb, B, 5);
    ra.tick(NOW).unwrap();
    rb.tick(NOW).unwrap();

    ra.set_holds_at("n1", vec![5], NOW + 1).unwrap();

    assert_eq!(record(&book, A).holds, vec![5]);
    assert!(record(&book, B).holds.is_empty());
    // The held event stays missing for A; B never held it.
    assert_eq!(ra.who_is_missing(5).unwrap().len(), 1);
}

#[test]
fn two_relays_driven_from_two_threads_lose_no_update() {
    const ACKS: u64 = 200;
    let book = shared();
    // The node is registered for both tenants first; each thread then builds its own relay (a
    // relay holds non-`Send` doubles) over a clone of the handle.
    for tenant in [A, B] {
        book.with_book(|b| b.register_node("n1", tenant, "1", 0))
            .unwrap();
    }

    std::thread::scope(|s| {
        for tenant in [A, B] {
            let handle = book.clone();
            s.spawn(move || {
                let mut stream = MemoryStream::new();
                for e in stream_of(tenant, ACKS) {
                    stream.append(e);
                }
                let mut r = HubRelay::new(
                    RelayConfig::new(tenant, SOURCE),
                    stream,
                    SharedQueue::default(),
                    Src::default(),
                    Box::new(Refs),
                )
                .unwrap();
                r.set_ledger(Box::new(handle));
                r.add_destination(dest("n1", &["tax"])).unwrap();
                r.tick(NOW).unwrap();
                for prefix in 1..=ACKS {
                    r.on_ack_at("n1", prefix, NOW + i64::try_from(prefix).unwrap())
                        .unwrap();
                }
            });
        }
    });

    for tenant in [A, B] {
        let rec = record(&book, tenant);
        assert_eq!(rec.applied_prefix, ACKS, "tenant {tenant}");
        assert_eq!(
            rec.last_contact_millis,
            NOW + i64::try_from(ACKS).unwrap(),
            "tenant {tenant}"
        );
    }
}

#[test]
#[allow(clippy::expect_used, clippy::unnecessary_literal_unwrap)]
fn a_panic_inside_one_call_poisons_the_lock_and_the_next_call_recovers() {
    let book = shared();
    let mut handle = book.clone();
    handle.register_node("n1", A, "1", 0).unwrap();
    handle.on_ack("n1", A, 3, StreamHead::new(3), 10).unwrap();

    let outcome = catch_unwind(AssertUnwindSafe(|| {
        book.with_book(|_| {
            let boom: Option<()> = None;
            boom.expect("a store panicked inside a call")
        })
    }));
    assert!(outcome.is_err());

    // The handle recovers the guard: the stored state is intact and the book keeps working.
    handle.on_ack("n1", A, 4, StreamHead::new(4), 11).unwrap();
    assert_eq!(record(&book, A).applied_prefix, 4);
    assert_eq!(record(&book, A).last_contact_millis, 11);
}

#[test]
fn ledger_book_check_in_names_the_tenant_and_applies_nothing() {
    let mut book = ledger_book();
    book.register_node("n1", A, "1", 0).unwrap();
    book.on_ack("n1", A, 7, StreamHead::new(7), 5).unwrap();
    book.on_checkin("n1", A, 9).unwrap();
    let rec = book.ledger().store().load("n1", A).unwrap();
    assert_eq!(rec.applied_prefix, 7);
    assert_eq!(rec.last_contact_millis, 9);

    // A pair that is not registered, also when the node is registered for another tenant.
    match book.on_checkin("n1", B, 10) {
        Err(AckError::UnknownNode { node, tenant }) => {
            assert_eq!(node, "n1");
            assert_eq!(tenant, B);
        }
        other => panic!("expected UnknownNode, got {other:?}"),
    }
    assert!(book.ledger().store().load("n1", B).is_none());
    assert_eq!(
        book.ledger()
            .store()
            .load("n1", A)
            .unwrap()
            .last_contact_millis,
        9
    );
}
