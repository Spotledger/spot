//! Two tenants on one ledger (WP-SYN-02.T3): the relay passes its own configured tenant on every
//! ledger call, so one tenant's ack never satisfies another tenant's mandatory event.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use common::*;
use spot_conditions::MemorySink;
use spot_sync_acks::{AckError, AckLedger, MemoryStore, NodeReportRow, NodeStateStore, StreamHead};
use spot_sync_hub::{
    AckBook, HubError, HubRelay, LedgerBook, MemoryStream, RelayConfig, StreamEntry,
};
use spot_sync_obligations::{Lane, MandatoryEvent, ObligationSettings};
use std::cell::RefCell;
use std::rc::Rc;

type Ledger = AckLedger<MemoryStore, MemorySink>;
type Relay = HubRelay<SharedQueue, MemoryStream, Src>;

const A: &str = "tenant-a";
const B: &str = "tenant-b";
const NOW: i64 = 1_000_000;

/// One ledger owned by the test and shared by the relays of several tenants.
#[derive(Clone)]
struct Shared(Rc<RefCell<Ledger>>);

impl AckBook for Shared {
    fn on_ack(
        &mut self,
        node: &str,
        tenant: &str,
        prefix: u64,
        head: StreamHead,
        at: i64,
    ) -> Result<(), AckError> {
        self.0
            .borrow_mut()
            .on_ack(node, tenant, prefix, head, at)
            .map(|_| ())
    }
    fn on_checkin(&mut self, node: &str, tenant: &str, at: i64) -> Result<(), AckError> {
        let prefix = self
            .0
            .borrow()
            .store()
            .load(node, tenant)
            .map(|r| r.applied_prefix)
            .ok_or_else(|| AckError::UnknownNode {
                node: node.into(),
                tenant: tenant.into(),
            })?;
        self.on_ack(node, tenant, prefix, StreamHead::new(prefix), at)
    }
    fn set_holds(
        &mut self,
        node: &str,
        tenant: &str,
        holds: Vec<u64>,
        head: StreamHead,
        at: i64,
    ) -> Result<(), AckError> {
        self.0
            .borrow_mut()
            .set_holds(node, tenant, holds, head, at)
            .map(|_| ())
    }
    fn register_node(
        &mut self,
        node: &str,
        tenant: &str,
        version: &str,
        at: i64,
    ) -> Result<bool, AckError> {
        self.0.borrow_mut().register_node(node, tenant, version, at)
    }
    fn register_mandatory(&mut self, tenant: &str, event: MandatoryEvent) -> bool {
        self.0.borrow_mut().register_mandatory(tenant, event)
    }
    fn mandatory_events(&self, tenant: &str) -> Vec<spot_sync_freshness::MandatoryEvent> {
        self.0
            .borrow()
            .store()
            .events(tenant)
            .into_iter()
            .map(|e| spot_sync_freshness::MandatoryEvent {
                position: e.position,
                published_at: e.published_at_millis,
                effective_from: e.effective_from_millis,
                record: e.record,
            })
            .collect()
    }
    fn who_is_missing(&self, tenant: &str, position: u64) -> Result<Vec<NodeReportRow>, AckError> {
        self.0.borrow().who_is_missing(tenant, position)
    }
}

fn ledger() -> Rc<RefCell<Ledger>> {
    Rc::new(RefCell::new(
        AckLedger::with_catalogue(
            MemoryStore::new(),
            MemorySink::new(),
            ObligationSettings::default(),
        )
        .unwrap(),
    ))
}

fn tagged(tenant: &str, position: u64, lane: Lane) -> StreamEntry {
    let mut e = entry(
        position,
        &format!("x{position}"),
        "tax",
        &format!("R{position}"),
        1,
    );
    tenant.clone_into(&mut e.tenant);
    e.lane = lane;
    e
}

fn relay(tenant: &str, led: &Rc<RefCell<Ledger>>, entries: Vec<StreamEntry>) -> Relay {
    let mut stream = MemoryStream::new();
    for e in entries {
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
    r.set_ledger(Box::new(Shared(led.clone())));
    r.add_destination(dest("n1", &["tax"])).unwrap();
    r
}

fn mandatory(position: u64) -> MandatoryEvent {
    MandatoryEvent {
        position,
        published_at_millis: NOW,
        effective_from_millis: None,
        record: format!("tax/R{position}"),
    }
}

#[test]
fn an_ack_through_one_relay_never_satisfies_the_other_tenants_event() {
    let led = ledger();
    let mut ra = relay(A, &led, vec![tagged(A, 1, Lane::Mandatory)]);
    let mut rb = relay(B, &led, vec![tagged(B, 1, Lane::Mandatory)]);
    // Both nodes records exist; the same position is a mandatory event in both tenants.
    assert!(ra.register_node("n1", "1", 0).unwrap());
    assert!(rb.register_node("n1", "1", 0).unwrap());
    assert!(
        ra.register_mandatory(&tagged(A, 1, Lane::Mandatory), NOW, None)
            .unwrap()
    );
    assert!(
        rb.register_mandatory(&tagged(B, 1, Lane::Mandatory), NOW, None)
            .unwrap()
    );
    ra.tick(NOW).unwrap();
    rb.tick(NOW).unwrap();

    ra.on_ack_at("n1", 1, NOW + 1).unwrap();

    let l = led.borrow();
    assert_eq!(l.store().load("n1", A).unwrap().applied_prefix, 1);
    assert_eq!(l.store().load("n1", A).unwrap().crossings.len(), 1);
    assert_eq!(l.store().load("n1", B).unwrap().applied_prefix, 0);
    assert!(l.store().load("n1", B).unwrap().crossings.is_empty());
    drop(l);
    assert!(ra.who_is_missing(1).unwrap().is_empty());
    let missing = rb.who_is_missing(1).unwrap();
    assert_eq!(missing.len(), 1);
    assert_eq!(missing[0].node, "n1");
    assert_eq!(missing[0].tenant, B);
}

#[test]
fn a_node_registered_for_one_tenant_only_is_refused_through_the_other_relay() {
    let led = ledger();
    let mut ra = relay(A, &led, vec![tagged(A, 1, Lane::Bulk)]);
    let mut rb = relay(B, &led, vec![tagged(B, 1, Lane::Bulk)]);
    ra.register_node("n1", "1", 0).unwrap();
    ra.tick(NOW).unwrap();
    rb.tick(NOW).unwrap();

    let err = rb.on_ack_at("n1", 1, NOW + 1).unwrap_err();
    assert!(
        matches!(err, HubError::UnknownDestination { .. }),
        "{err:?}"
    );
    assert!(led.borrow().store().load("n1", B).is_none());
    assert_eq!(
        led.borrow().store().load("n1", A).unwrap().applied_prefix,
        0
    );
    // A check-in through relay B changes nothing either.
    assert!(rb.on_checkin_at("n1", NOW + 2).is_err());
    assert_eq!(
        led.borrow()
            .store()
            .load("n1", A)
            .unwrap()
            .last_contact_millis,
        0
    );
}

#[test]
fn a_check_in_bumps_only_the_relays_own_tenant_record() {
    let led = ledger();
    let mut ra = relay(A, &led, vec![]);
    let mut rb = relay(B, &led, vec![]);
    ra.register_node("n1", "1", 0).unwrap();
    rb.register_node("n1", "1", 0).unwrap();
    ra.on_checkin_at("n1", 500).unwrap();
    let l = led.borrow();
    assert_eq!(l.store().load("n1", A).unwrap().last_contact_millis, 500);
    assert_eq!(l.store().load("n1", B).unwrap().last_contact_millis, 0);
}

#[test]
fn holds_are_registered_and_bounded_per_tenant() {
    let led = ledger();
    let mut ra = relay(A, &led, vec![tagged(A, 1, Lane::Mandatory)]);
    let mut rb = relay(B, &led, vec![tagged(B, 1, Lane::Mandatory)]);
    ra.register_node("n1", "1", 0).unwrap();
    rb.register_node("n1", "1", 0).unwrap();
    ra.register_mandatory(&tagged(A, 1, Lane::Mandatory), NOW, None)
        .unwrap();
    // Only tenant A has the event: B cannot hold it.
    let err = rb.set_holds_at("n1", vec![1], NOW).unwrap_err();
    assert_eq!(err.code(), "sync_acks.hold_unknown");
    ra.set_holds_at("n1", vec![1], NOW).unwrap();
    assert_eq!(led.borrow().store().load("n1", A).unwrap().holds, vec![1]);
    assert!(led.borrow().store().load("n1", B).unwrap().holds.is_empty());
    // A position above the head of A's stream is refused and marks the node suspect.
    let err = ra.set_holds_at("n1", vec![9], NOW).unwrap_err();
    assert_eq!(err.code(), "sync_acks.hold_beyond_head");
    let report = ra.tick(NOW + 1).unwrap();
    assert_eq!(report.suspect, vec!["n1".to_owned()]);
}

#[test]
fn entries_of_another_tenant_source_lane_or_role_register_nothing() {
    let led = ledger();
    let mut ra = relay(A, &led, vec![]);
    // Tenant B's mandatory entry read by relay A.
    assert!(
        !ra.register_mandatory(&tagged(B, 1, Lane::Mandatory), NOW, None)
            .unwrap()
    );
    // Another source.
    let mut other = tagged(A, 2, Lane::Mandatory);
    "other-hub".clone_into(&mut other.source);
    assert!(!ra.register_mandatory(&other, NOW, None).unwrap());
    // Bulk lane.
    assert!(
        !ra.register_mandatory(&tagged(A, 3, Lane::Bulk), NOW, None)
            .unwrap()
    );
    let l = led.borrow();
    assert!(l.store().events(A).is_empty());
    assert!(l.store().events(B).is_empty());
    drop(l);
    assert!(ra.who_is_missing(1).is_err());
}

#[test]
fn the_same_position_in_two_tenants_is_two_events() {
    let led = ledger();
    let mut ra = relay(A, &led, vec![]);
    let mut rb = relay(B, &led, vec![]);
    assert!(
        ra.register_mandatory(&tagged(A, 7, Lane::Mandatory), NOW, None)
            .unwrap()
    );
    assert!(
        rb.register_mandatory(&tagged(B, 7, Lane::Mandatory), NOW, None)
            .unwrap()
    );
    assert!(
        !ra.register_mandatory(&tagged(A, 7, Lane::Mandatory), NOW, None)
            .unwrap()
    );
    let _ = mandatory(7);
}

#[test]
fn the_ledger_book_serves_one_tenant_per_call_and_forgets_none() {
    let mut book = LedgerBook::new(
        AckLedger::with_catalogue(
            MemoryStore::new(),
            MemorySink::new(),
            ObligationSettings::default(),
        )
        .unwrap(),
    );
    book.register_node("n1", A, "1", 0).unwrap();
    book.register_node("n1", B, "1", 0).unwrap();
    book.on_ack("n1", A, 2, StreamHead::new(2), 5).unwrap();
    assert_eq!(
        book.ledger().store().load("n1", A).unwrap().applied_prefix,
        2
    );
    assert_eq!(
        book.ledger().store().load("n1", B).unwrap().applied_prefix,
        0
    );
    book.on_checkin("n1", B, 9).unwrap();
    assert_eq!(
        book.ledger()
            .store()
            .load("n1", A)
            .unwrap()
            .last_contact_millis,
        5
    );
    assert!(matches!(
        book.on_checkin("n1", "nope", 9),
        Err(AckError::UnknownNode { .. })
    ));
}
