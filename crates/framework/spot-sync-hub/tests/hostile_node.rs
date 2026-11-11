//! The three hostile-node bounds, in one place (WP-SYN-02.T2): an ack past the hub's head, a
//! stamp that tries to place itself outside the window, and a catch-up gap holding another
//! node's audit event. Each is driven by a node that lies.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use common::*;
use spot_conditions::MemorySink;
use spot_sync_acks::{AckLedger, MemoryStore, NodeStateStore};
use spot_sync_catchup::{
    ClassCounts, Event, EventClass, GapSummary, Payload, RecordId, Settings, StaticTopics,
};
use spot_sync_freshness::{Bounds, SyncStamp};
use spot_sync_hub::{
    AckBook, GapSource, HubAudit, HubError, HubRelay, MemoryStream, RelayState, StreamEntry,
};
use spot_sync_obligations::{MandatoryEvent, ObligationSettings};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

type Ledger = AckLedger<MemoryStore, MemorySink>;
type Relay = HubRelay<SharedQueue, MemoryStream, Src>;

const DAY: i64 = 86_400_000;
/// The one tenant these tests serve.
const T: &str = "t1";
const NOW: i64 = 20_000 * DAY;

/// A ledger the test keeps a handle on after the relay owns the other one.
#[derive(Clone)]
struct Shared(Rc<RefCell<Ledger>>);

impl AckBook for Shared {
    fn on_ack(
        &mut self,
        node: &str,
        tenant: &str,
        prefix: u64,
        head: spot_sync_acks::StreamHead,
        at: i64,
    ) -> Result<(), spot_sync_acks::AckError> {
        self.0
            .borrow_mut()
            .on_ack(node, tenant, prefix, head, at)
            .map(|_| ())
    }
    fn on_checkin(&mut self, _: &str, _: &str, _: i64) -> Result<(), spot_sync_acks::AckError> {
        Ok(())
    }
    fn set_holds(
        &mut self,
        _: &str,
        _: &str,
        _: Vec<u64>,
        _: spot_sync_acks::StreamHead,
        _: i64,
    ) -> Result<(), spot_sync_acks::AckError> {
        Ok(())
    }
    fn register_node(
        &mut self,
        node: &str,
        tenant: &str,
        version: &str,
        at: i64,
    ) -> Result<bool, spot_sync_acks::AckError> {
        self.0.borrow_mut().register_node(node, tenant, version, at)
    }
    fn register_mandatory(&mut self, tenant: &str, event: MandatoryEvent) -> bool {
        self.0.borrow_mut().register_mandatory(tenant, event)
    }
    fn who_is_missing(
        &self,
        tenant: &str,
        position: u64,
    ) -> Result<Vec<spot_sync_acks::NodeReportRow>, spot_sync_acks::AckError> {
        self.0.borrow().who_is_missing(tenant, position)
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
}

#[derive(Clone, Default)]
struct Notes(Rc<RefCell<Vec<spot_sync_freshness::RejectNote>>>);

impl HubAudit for Notes {
    fn stamp_rejected(&mut self, note: spot_sync_freshness::RejectNote) {
        self.0.borrow_mut().push(note);
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

fn relay_over(entries: Vec<StreamEntry>, led: &Rc<RefCell<Ledger>>) -> Relay {
    let mut stream = MemoryStream::new();
    for e in entries {
        stream.append(e);
    }
    let mut r = HubRelay::new(
        cfg(),
        stream,
        SharedQueue::default(),
        Src::default(),
        Box::new(Refs),
    )
    .unwrap();
    r.set_ledger(Box::new(Shared(led.clone())));
    r.add_destination(dest("n1", &["item"])).unwrap();
    r
}

fn three() -> Vec<StreamEntry> {
    (1..=3)
        .map(|p| entry(p, &format!("t{p}"), "item", &format!("I{p}"), 1))
        .collect()
}

fn mandatory(position: u64, published_at: i64) -> MandatoryEvent {
    MandatoryEvent {
        position,
        published_at_millis: published_at,
        effective_from_millis: None,
        record: format!("tax_rate/{position}"),
    }
}

// (a) A node acks u64::MAX.
#[test]
fn an_ack_of_u64_max_is_refused_and_a_later_mandatory_event_is_still_outstanding() {
    let led = ledger();
    led.borrow_mut().register_node("n1", T, "1", 0).unwrap();
    let mut r = relay_over(three(), &led);
    r.tick(NOW).unwrap();

    let err = r.on_ack_at("n1", u64::MAX, NOW).unwrap_err();
    assert!(matches!(err, HubError::PrefixAhead { .. }), "{err:?}");
    assert_eq!(
        led.borrow().store().load("n1", T).unwrap().applied_prefix,
        0
    );
    assert!(
        led.borrow()
            .store()
            .load("n1", T)
            .unwrap()
            .crossings
            .is_empty()
    );

    // A mandatory event is registered at the next position, then the node acks the true head.
    r.stream_mut().append(entry(4, "t4", "item", "I4", 1));
    led.borrow_mut().register_mandatory(T, mandatory(4, NOW));
    r.tick(NOW + 1).unwrap();
    r.on_ack_at("n1", 3, NOW + 2).unwrap();
    {
        let l = led.borrow();
        let rec = l.store().load("n1", T).unwrap();
        assert_eq!(rec.applied_prefix, 3);
        assert!(
            rec.crossings.is_empty(),
            "the event at 4 is still outstanding for the node"
        );
    }
    r.on_ack_at("n1", 4, NOW + 3).unwrap();
    assert_eq!(
        led.borrow().store().load("n1", T).unwrap().crossings.len(),
        1
    );
}

#[test]
fn an_ack_above_the_hub_head_is_refused_with_its_own_code_even_when_the_cursor_is_stale() {
    let led = ledger();
    led.borrow_mut().register_node("n1", T, "1", 0).unwrap();
    let mut r = relay_over(three(), &led);
    // A snapshot restored onto a rebuilt stream: the cursor is above the head.
    r.restore(RelayState {
        cursor: 10,
        destinations: BTreeMap::new(),
        ..RelayState::default()
    })
    .unwrap();
    let err = r.on_ack_at("n1", 8, NOW).unwrap_err();
    assert_eq!(err.code(), "sync_hub.ack_beyond_head");
    assert_eq!(
        err,
        HubError::AckBeyondHead {
            dest: "n1".into(),
            claimed: 8,
            head: 3
        }
    );
    assert_eq!(
        led.borrow().store().load("n1", T).unwrap().applied_prefix,
        0
    );
    // The head itself is accepted.
    r.on_ack_at("n1", 3, NOW).unwrap();
    assert_eq!(
        led.borrow().store().load("n1", T).unwrap().applied_prefix,
        3
    );
}

#[test]
fn a_refused_ack_marks_the_destination_suspect_in_the_next_report() {
    let led = ledger();
    led.borrow_mut().register_node("n1", T, "1", 0).unwrap();
    let mut r = relay_over(three(), &led);
    r.tick(NOW).unwrap();
    r.on_ack_at("n1", 99, NOW).unwrap_err();
    r.on_ack_at("n1", 100, NOW).unwrap_err();
    let rep = r.tick(NOW + 1).unwrap();
    assert_eq!(rep.suspect, vec!["n1".to_owned()]);
    assert_eq!(rep.suspect_events, 2);
    let again = r.tick(NOW + 2).unwrap();
    assert!(again.suspect.is_empty());
    assert_eq!(again.suspect_events, 0);
}

fn stamp(stamped_at: i64, offset: i64) -> SyncStamp {
    SyncStamp {
        node: "someone-else".into(),
        stamped_at_millis: stamped_at,
        hub_offset_ms: offset,
        applied_prefix: 0,
        class_horizons_confirmed: BTreeMap::new(),
        stale: vec![],
        metadata_version: 1,
        software_version: "1.0.0".into(),
        override_: None,
        warning_confirmed: None,
    }
}

fn stamped_relay() -> (Relay, Rc<RefCell<Ledger>>, Notes) {
    let led = ledger();
    led.borrow_mut().register_node("n1", T, "1", 0).unwrap();
    // A mandatory tax change published 10 days before the document reaches the hub.
    led.borrow_mut()
        .register_mandatory(T, mandatory(4, NOW - 10 * DAY));
    let mut r = relay_over(three(), &led);
    let notes = Notes::default();
    r.set_audit(Box::new(notes.clone()));
    // The longest offline period the hub accepts: 5 days.
    r.set_bounds(Bounds::with_max_age(5 * DAY).unwrap());
    (r, led, notes)
}

// (b) A stamp with extreme values, and an offset that backdates the document.
#[test]
fn a_stamp_at_i64_min_is_rejected_and_audited() {
    let (mut r, _led, notes) = stamped_relay();
    let err = r
        .on_stamped_document("n1", "INV-1", &stamp(i64::MIN, 0), NOW)
        .unwrap_err();
    assert_eq!(err.code(), "sync_hub.stamp_rejected");
    assert_eq!(notes.0.borrow().len(), 1);
    let note = notes.0.borrow()[0].clone();
    assert_eq!(note.node, "n1", "the authenticated node, not the stamp's");
    assert_eq!(note.claimed_stamped_at_millis, i64::MIN);

    let err = r
        .on_stamped_document("n1", "INV-2", &stamp(i64::MAX, i64::MAX), NOW)
        .unwrap_err();
    assert_eq!(err.code(), "sync_hub.stamp_rejected");
    assert_eq!(notes.0.borrow().len(), 2);
}

#[test]
fn a_thirty_day_backdating_offset_cannot_hide_a_ten_day_old_mandatory_event() {
    let (mut r, _led, notes) = stamped_relay();
    let d = r
        .on_stamped_document("n1", "INV-3", &stamp(NOW, -30 * DAY), NOW)
        .unwrap();
    assert_eq!(d.findings.len(), 1, "{d:?}");
    let f = &d.findings[0];
    assert!(f.clamped);
    assert_eq!(f.node, "n1", "the finding names the authenticated node");
    assert_eq!(f.mandatory_record, "tax_rate/4");
    assert_eq!(f.effective_stamped_at_millis, NOW - 5 * DAY);
    assert!(notes.0.borrow().is_empty());
}

#[test]
fn an_honest_stamp_is_not_clamped_and_one_that_applied_the_event_has_no_finding() {
    let (mut r, _led, _n) = stamped_relay();
    let mut s = stamp(NOW - DAY, 0);
    s.applied_prefix = 4;
    let d = r.on_stamped_document("n1", "INV-4", &s, NOW).unwrap();
    assert!(d.findings.is_empty());
    let d = r
        .on_stamped_document("n1", "INV-5", &stamp(NOW - DAY, 0), NOW)
        .unwrap();
    assert_eq!(d.findings.len(), 1);
    assert!(!d.findings[0].clamped);
}

#[test]
fn stamped_documents_fail_closed_without_bounds_a_ledger_or_a_known_node() {
    let (mut r, _led, _n) = stamped_relay();
    let err = r
        .on_stamped_document("ghost", "D", &stamp(NOW, 0), NOW)
        .unwrap_err();
    assert!(matches!(err, HubError::UnknownDestination { .. }));

    let led = ledger();
    led.borrow_mut().register_node("n1", T, "1", 0).unwrap();
    let mut r = relay_over(three(), &led);
    let err = r
        .on_stamped_document("n1", "D", &stamp(NOW, 0), NOW)
        .unwrap_err();
    assert_eq!(err.code(), "sync_hub.invalid_config", "no bounds set");
}

// (c) A gap holding an audit event for node B.
struct Gap(Vec<Event>);

impl GapSource for Gap {
    fn events_between(&self, after: u64, through: u64) -> Result<Vec<Event>, HubError> {
        Ok(self
            .0
            .iter()
            .filter(|e| e.position > after && e.position <= through)
            .cloned()
            .collect())
    }
}

struct NoRefs;

impl spot_sync_catchup::References for NoRefs {
    fn references(
        &self,
        _record: &RecordId,
        _image: &Payload,
    ) -> Result<Vec<RecordId>, spot_sync_catchup::CatchUpError> {
        Ok(Vec::new())
    }
}

fn gap_events() -> Vec<Event> {
    vec![
        Event::new(1, "doc_state", Payload::new("{}")).with_subject(RecordId::new("doc", "a")),
        Event::new(2, "sys_adopt", Payload::new("{}")).with_addressee("B"),
        Event::new(3, "doc_state", Payload::new("{}")).with_subject(RecordId::new("doc", "b")),
    ]
}

fn summary(stream_end: u64) -> GapSummary {
    GapSummary {
        applied_prefix: 0,
        stream_end,
        hot_retention_floor: 0,
        in_scope_event_count: 3,
        class_counts: ClassCounts {
            state: 2,
            domain: 0,
            audit: 1,
        },
        estimated_coalesced_records: 2,
        estimated_coalesced_bytes: 10,
        estimated_snapshot_bytes: 1_000_000,
        node_is_new_or_resync: false,
    }
}

fn catchup_relay() -> Relay {
    let led = ledger();
    let mut r = relay_over(three(), &led);
    r.add_destination(dest("A", &["item"])).unwrap();
    r.add_destination(dest("B", &["item"])).unwrap();
    r
}

fn topics() -> StaticTopics {
    StaticTopics::new()
        .with("doc_state", EventClass::State)
        .with("sys_adopt", EventClass::Audit)
}

fn bulk_topics(plan: &spot_sync_catchup::CatchUpPlan) -> Vec<String> {
    plan.bulk
        .iter()
        .flat_map(|b| b.events.iter().map(|e| e.topic.clone()))
        .collect()
}

#[test]
fn a_gap_with_an_audit_event_for_b_is_planned_for_a_without_it_and_for_b_with_it() {
    let mut r = catchup_relay();
    let gap = Gap(gap_events());
    let a = r
        .catch_up(
            "A",
            &summary(3),
            &gap,
            &Settings::default(),
            &topics(),
            &NoRefs,
        )
        .unwrap();
    assert_eq!(bulk_topics(&a.plan), vec!["doc_state", "doc_state"]);
    assert_eq!(a.audit_dropped, 1);

    let b = r
        .catch_up(
            "B",
            &summary(3),
            &gap,
            &Settings::default(),
            &topics(),
            &NoRefs,
        )
        .unwrap();
    assert_eq!(
        bulk_topics(&b.plan),
        vec!["doc_state", "sys_adopt", "doc_state"]
    );
    assert_eq!(b.audit_dropped, 0);

    // The count lands in the next tick's report.
    let rep = r.tick(NOW).unwrap();
    assert_eq!(rep.audit_dropped, 1);
    assert_eq!(r.tick(NOW + 1).unwrap().audit_dropped, 0);
}

#[test]
fn a_summary_beyond_the_hub_head_is_refused_and_marks_the_node_suspect() {
    let mut r = catchup_relay();
    let gap = Gap(gap_events());
    let err = r
        .catch_up(
            "A",
            &summary(99),
            &gap,
            &Settings::default(),
            &topics(),
            &NoRefs,
        )
        .unwrap_err();
    assert_eq!(err.code(), "sync_hub.ack_beyond_head");
    let rep = r.tick(NOW).unwrap();
    assert_eq!(rep.suspect, vec!["A".to_owned()]);
    let err = r
        .catch_up(
            "ghost",
            &summary(3),
            &gap,
            &Settings::default(),
            &topics(),
            &NoRefs,
        )
        .unwrap_err();
    assert!(matches!(err, HubError::UnknownDestination { .. }));
}
