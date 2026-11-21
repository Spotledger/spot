//! The hub-side pieces together (WP-SYN-02.T10): a real `HubRelay`, the master-data
//! `MasterScope`, the policy `PolicyRedactor`, the real closure builder and the tenant-keyed
//! ack ledger. Every leak check searches the serialised bytes of the queued bundles for the
//! excluded names and values, not only the structure, so a leak through a field the bundle gains
//! later shows up too.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use common::*;
use serde_json::{Value, json};
use spot_sync_closure::{RecordKey, Role};
use spot_sync_hub::{HubError, HubStream};

const NOW: i64 = 1_000;

fn ids_of(relay: &Relay, dest: &str) -> Vec<String> {
    records(relay, dest)
        .into_iter()
        .map(|(_, id, _)| id)
        .collect()
}

/// Two destinations: `edge-a` (company C1, price lists) and `edge-b` (company C1 but only
/// items, so it does not subscribe to the entity).
fn two_destinations(relay: &mut Relay, book: &'static spot_sync_hub_adapters::PolicyBook) {
    let reg = registry(&[("P1", "C1"), ("P2", "C2")]);
    let (a, _) = destination(
        "edge-a",
        subscription("edge-a", &["C1"], &["price_list"]),
        reg,
        book,
    );
    let (b, _) = destination(
        "edge-b",
        subscription("edge-b", &["C1"], &["item"]),
        reg,
        book,
    );
    relay.add_destination(a).unwrap();
    relay.add_destination(b).unwrap();
}

fn leaked_book() -> &'static spot_sync_hub_adapters::PolicyBook {
    Box::leak(Box::new(book()))
}

// (a) scope: only the subscribed destination is sent anything, and what it is sent has no
// excluded name at any level.
#[test]
fn only_the_subscribed_destination_receives_bundles_and_they_hold_no_excluded_field() {
    let mut src = Src::default();
    src.put("price_list", "P1", price_list_image(), 1);
    src.put("price_list", "P2", price_list_image(), 2);
    let entries = vec![
        entry(1, TENANT, "price_list", "P1"),
        entry(2, TENANT, "price_list", "P2"),
    ];
    let led = ledger();
    let mut relay = relay(TENANT, entries, src, &led);
    two_destinations(&mut relay, leaked_book());
    relay.tick(NOW).unwrap();

    // B is not subscribed to the entity: nothing queued. A is on company C1 only: P1, not P2.
    assert!(queued(&relay, "edge-b").is_empty());
    assert_eq!(ids_of(&relay, "edge-a"), vec!["P1"]);
    for (_, _, image) in records(&relay, "edge-a") {
        assert_eq!(image, price_list_redacted());
    }
    let text = bundle_text(&relay, "edge-a");
    assert_no_leak(&text);
    // What is kept is still there: the redaction removed only what the policy excludes.
    assert!(text.contains("\"rate\":\"1.00\""), "{text}");
    assert!(text.contains("\"qty\":\"10\""), "{text}");
    assert_eq!(bundle_text(&relay, "edge-b"), "");
}

// (b) an entity with no policy is not sent, is named, and the host-side start check refuses the
// destination whose scope has it.
#[test]
fn an_entity_without_a_policy_queues_null_is_named_and_fails_the_start_check() {
    let book = leaked_book();
    let mut src = Src::default();
    src.put("orphan", "O1", json!({"secret": "ORPHAN-SECRET"}), 1);
    let led = ledger();
    let mut relay = relay(TENANT, vec![entry(1, TENANT, "orphan", "O1")], src, &led);
    let reg = registry(&[]);
    let sub = subscription("edge-a", &["C1"], &["price_list", "orphan"]);
    let (dest, redactor) = destination("edge-a", sub, reg, book);
    relay.add_destination(dest).unwrap();
    relay.tick(NOW).unwrap();

    let recs = records(&relay, "edge-a");
    assert_eq!(recs.len(), 1);
    assert_eq!(recs[0].0, "orphan");
    assert_eq!(
        recs[0].2,
        Value::Null,
        "an unpoliced entity must queue Null"
    );
    assert_no_leak(&bundle_text(&relay, "edge-a"));
    assert_eq!(redactor.unredacted_entities(), vec!["orphan"]);

    // The check a deployment writes before it starts a destination.
    assert_eq!(start_check(book, sub), Err(vec!["orphan".to_owned()]));
    assert_eq!(book.missing_for(&scope_entities(sub)), vec!["orphan"]);
    // A destination whose scope is fully covered starts.
    let covered = subscription("edge-c", &["C1"], &["price_list", "item"]);
    assert_eq!(start_check(book, covered), Ok(()));
}

// (c) a reference target outside the scope: the merged closure sends it as an extra (the rule for
// doubt is to include), redacted like any record, and keeps routing its later versions; a record
// outside the scope that is not an extra is not routed at all.
#[test]
fn an_out_of_scope_reference_is_redacted_and_an_unreferenced_one_is_not_sent() {
    let book = leaked_book();
    let mut src = Src::default();
    let mut p3 = price_list_image();
    p3["ref_item"] = json!("I9");
    let mut p4 = price_list_image();
    p4["ref_orphan"] = json!("Z1");
    src.put("price_list", "P3", p3, 1);
    src.put("price_list", "P4", p4, 2);
    src.put(
        "item",
        "I9",
        json!({"sku": "S9", "supplier_cost": "SUPCOST-5-SECRET"}),
        1,
    );
    src.put(
        "item",
        "I8",
        json!({"sku": "S8", "supplier_cost": "SUPCOST-5-SECRET"}),
        3,
    );
    src.put("orphan", "Z1", json!({"secret": "ORPHAN-SECRET"}), 1);
    let entries = vec![
        entry(1, TENANT, "price_list", "P3"),
        entry(2, TENANT, "price_list", "P4"),
        // An item the destination neither subscribes to nor was sent as an extra.
        entry(3, TENANT, "item", "I8"),
    ];
    let led = ledger();
    let mut relay = relay(TENANT, entries, src, &led);
    let reg = registry(&[("P3", "C1"), ("P4", "C1")]);
    let (dest, redactor) = destination(
        "edge-a",
        subscription("edge-a", &["C1"], &["price_list"]),
        reg,
        book,
    );
    relay.add_destination(dest).unwrap();
    relay.tick(NOW).unwrap();

    let recs = records(&relay, "edge-a");
    // The extra item I9 is there, redacted; the unrelated item I8 is not routed.
    let i9 = find(&recs, "item", "I9");
    assert_eq!(i9.len(), 1, "{recs:?}");
    assert_eq!(i9[0].2, json!({"sku": "S9"}));
    assert!(find(&recs, "item", "I8").is_empty(), "{recs:?}");
    // The extra of an entity with no policy goes out as Null, never as its image.
    let z1 = find(&recs, "orphan", "Z1");
    assert_eq!(z1.len(), 1, "{recs:?}");
    assert_eq!(z1[0].2, Value::Null);
    assert_eq!(redactor.unredacted_entities(), vec!["orphan"]);
    let roles: Vec<Role> = queued(&relay, "edge-a")
        .iter()
        .flat_map(|p| p.item.bundle.records.iter())
        .filter(|r| r.key.entity != "price_list")
        .map(|r| r.role)
        .collect();
    assert!(roles.iter().all(|r| *r == Role::Extra), "{roles:?}");
    let extras = relay.extras("edge-a").unwrap();
    assert!(extras.contains(&RecordKey::new("item", "I9")));
    assert!(!extras.contains(&RecordKey::new("item", "I8")));
    assert_no_leak(&bundle_text(&relay, "edge-a"));

    // A later version of the delivered extra keeps being routed, and is redacted again.
    relay.stream_mut().append(entry(4, TENANT, "item", "I9"));
    relay.tick(NOW + 1).unwrap();
    assert_eq!(find_count(&relay, "I9"), 2);
    assert_no_leak(&bundle_text(&relay, "edge-a"));
}

type Rec = (String, String, Value);

fn find<'a>(recs: &'a [Rec], entity: &str, id: &str) -> Vec<&'a Rec> {
    recs.iter().filter(|r| r.0 == entity && r.1 == id).collect()
}

fn find_count(relay: &Relay, id: &str) -> usize {
    ids_of(relay, "edge-a").iter().filter(|i| *i == id).count()
}

// (d) tenants: a relay refuses what is not its tenant's, and a ledger shared by two relays keeps
// the two tenants' prefixes apart.
#[test]
fn another_tenants_entries_never_reach_the_destination_or_the_ledger() {
    let book = leaked_book();
    let reg = registry(&[("P1", "C1"), ("P1b", "C1"), ("P2", "C1")]);
    let mut img1 = price_list_image();
    img1["rate"] = json!("T1-RATE");
    let mut img2 = price_list_image();
    img2["rate"] = json!("T2-RATE");

    // Relay t1 reads a stream that also holds a t2 entry (position 2). The merged relay drops it
    // before any image is read: the image source has no P2, so a read would fail the tick.
    let mut src1 = Src::default();
    src1.put("price_list", "P1", img1.clone(), 1);
    src1.put("price_list", "P1b", img1, 3);
    let mixed = vec![
        entry(1, "t1", "price_list", "P1"),
        entry(2, "t2", "price_list", "P2"),
        entry(3, "t1", "price_list", "P1b"),
    ];
    let led = ledger();
    let mut r1 = relay("t1", mixed, src1, &led);
    let sub = subscription("edge-a", &["C1"], &["price_list"]);
    r1.add_destination(destination("edge-a", sub, reg, book).0)
        .unwrap();
    assert!(r1.register_node("edge-a", "1", 0).unwrap());

    // Relay t2 has its own stream and the same node id.
    let mut src2 = Src::default();
    src2.put("price_list", "P2", img2, 2);
    let mut r2 = relay("t2", vec![entry(2, "t2", "price_list", "P2")], src2, &led);
    r2.add_destination(destination("edge-a", sub, reg, book).0)
        .unwrap();
    assert!(r2.register_node("edge-a", "1", 0).unwrap());

    let rep1 = r1.tick(NOW).unwrap();
    let rep2 = r2.tick(NOW).unwrap();
    assert_eq!(rep1.foreign_dropped, 1);
    assert_eq!(rep1.failed, None);
    assert_eq!(rep1.destinations["edge-a"].failed, None);
    assert_eq!(rep2.foreign_dropped, 0);
    assert_eq!(ids_of(&r1, "edge-a"), vec!["P1", "P1b"]);
    assert_eq!(ids_of(&r2, "edge-a"), vec!["P2"]);
    assert!(!bundle_text(&r1, "edge-a").contains("T2-RATE"));
    assert!(!bundle_text(&r2, "edge-a").contains("T1-RATE"));

    // The ledger is keyed by (node, tenant): t1's ack moves t1's record only.
    r1.on_ack_at("edge-a", 3, NOW + 1).unwrap();
    assert_eq!(applied(&led, "edge-a", "t1"), Some(3));
    assert_eq!(applied(&led, "edge-a", "t2"), Some(0));
    r2.on_ack_at("edge-a", 2, NOW + 2).unwrap();
    assert_eq!(applied(&led, "edge-a", "t2"), Some(2));
    assert_eq!(applied(&led, "edge-a", "t1"), Some(3));
}

// (e) the wiring of an ack: the true head trims the queue and moves the ledger; a claim beyond
// the hub's head is refused and changes nothing (the full hostile set is WP-SYN-02.T2's).
#[test]
fn the_true_head_trims_the_queue_and_a_claim_beyond_it_is_refused() {
    let mut src = Src::default();
    src.put("price_list", "P1", price_list_image(), 1);
    let led = ledger();
    let mut relay = relay(
        TENANT,
        vec![entry(1, TENANT, "price_list", "P1")],
        src,
        &led,
    );
    two_destinations(&mut relay, leaked_book());
    assert!(relay.register_node("edge-a", "1", 0).unwrap());
    relay.tick(NOW).unwrap();
    assert!(!queued(&relay, "edge-a").is_empty());
    let head = relay.stream_mut().head().unwrap();
    assert_eq!(head, 1);

    // One past the head: refused, nothing trimmed, nothing recorded.
    let err = relay.on_ack_at("edge-a", head + 1, NOW + 1).unwrap_err();
    assert!(matches!(err, HubError::PrefixAhead { .. }), "{err:?}");
    assert!(!queued(&relay, "edge-a").is_empty());
    assert_eq!(applied(&led, "edge-a", TENANT), Some(0));
    let rep = relay.tick(NOW + 2).unwrap();
    assert_eq!(rep.suspect, vec!["edge-a".to_owned()]);

    // The true head: trimmed and recorded.
    let removed = relay.on_ack_at("edge-a", head, NOW + 3).unwrap();
    assert!(removed > 0);
    assert!(queued(&relay, "edge-a").is_empty());
    assert_eq!(applied(&led, "edge-a", TENANT), Some(head));
}
