//! Serving the node's repair pull and scoped re-send (WP-SYN-02.T7).
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use common::*;
use serde_json::json;
use spot_sync_closure::{RecordKey, SourceImage};
use spot_sync_hub::{
    Destination, HubError, HubRelay, MemoryStream, PULL_THROTTLED, PullItem, RESEND_SCOPE_MISMATCH,
    StreamEntry,
};
use spot_sync_repair_pull::{
    Input, Output, PullRequest, RepairConfig, RepairLoop, ScopeCheck, ScopedResend,
};

type Relay = HubRelay<SharedQueue, MemoryStream, Src>;

const SCOPE: &str = "branch:khi";

fn relay_with(cfg: spot_sync_hub::RelayConfig, entries: Vec<StreamEntry>, src: Src) -> Relay {
    HubRelay::new(
        cfg,
        memory_stream(entries),
        SharedQueue::default(),
        src,
        Box::new(Refs),
    )
    .unwrap()
}

fn scoped_dest(id: &str, scope: Scope) -> Destination {
    Destination::new(
        id,
        Box::new(scope),
        Box::new(StripSecret),
        v1(&["item", "price", "order", "master", "tax"]),
    )
    .with_scope_identity(SCOPE)
}

fn pull(keys: Vec<RecordKey>) -> PullRequest {
    PullRequest {
        tenant: TENANT.to_owned(),
        keys,
    }
}

fn json_of(items: &[PullItem]) -> String {
    serde_json::to_string(items).unwrap()
}

fn tombstone(src: &mut Src, entity: &str, id: &str, position: u64) {
    let key = k(entity, id);
    src.images
        .insert(key.clone(), SourceImage::tombstone(key, position));
}

#[test]
fn an_in_scope_key_returns_the_redacted_record_without_prerequisites() {
    let mut src = Src::default();
    src.put("order", "O1", linking(&["M1"]), 3);
    src.put("master", "M1", plain(), 2);
    let mut r = relay_with(cfg(), vec![entry(3, "a", "order", "O1", 1)], src);
    r.add_destination(scoped_dest("d", Scope::of(&["order"])))
        .unwrap();
    let ans = r.serve_pull("d", &pull(vec![k("order", "O1")])).unwrap();
    assert_eq!(ans.items.len(), 1);
    let PullItem::Record(rec) = &ans.items[0] else {
        panic!("expected a record, got {:?}", ans.items[0]);
    };
    assert_eq!(rec.key, k("order", "O1"));
    assert!(
        rec.image.get("secret").is_none(),
        "redacted for the destination"
    );
    assert_eq!(ans.report.served, 1);
    // The master the order refers to is not sent: the node asks again for what it lacks.
    let ans2 = r.serve_pull("d", &pull(vec![k("master", "M1")]));
    assert!(
        matches!(ans2.unwrap().items[0], PullItem::NotAvailable),
        "master is out of scope here"
    );
}

#[test]
fn every_reason_for_refusing_a_key_gives_the_same_answer() {
    let mut src = Src::default();
    src.put("item", "OTHER", plain(), 1); // in another destination's scope only
    src.put("price", "OWNED", plain(), 2); // owned by the destination
    src.put("item", "FOREIGN-TENANT", plain(), 3);
    let mut scope = Scope::of(&["price"]);
    scope.owned.insert(k("price", "OWNED"));
    let mut r = relay_with(cfg(), vec![], src);
    r.add_destination(scoped_dest("d", scope)).unwrap();
    let out_of_scope = r.serve_pull("d", &pull(vec![k("item", "OTHER")])).unwrap();
    let unknown = r.serve_pull("d", &pull(vec![k("price", "NOPE")])).unwrap();
    let owned = r.serve_pull("d", &pull(vec![k("price", "OWNED")])).unwrap();
    let foreign = r
        .serve_pull(
            "d",
            &PullRequest {
                tenant: "other-tenant".to_owned(),
                keys: vec![k("price", "OWNED")],
            },
        )
        .unwrap();
    let baseline = json_of(&out_of_scope.items);
    assert_eq!(baseline, json_of(&unknown.items));
    assert_eq!(baseline, json_of(&owned.items));
    assert_eq!(baseline, json_of(&foreign.items));
    assert_eq!(out_of_scope.report.out_of_scope, 1);
    assert_eq!(unknown.report.unknown, 1);
    assert_eq!(owned.report.owned, 1);
    assert_eq!(foreign.report.foreign_tenant, 1);
    // The counts are not on the wire.
    assert!(!serde_json::to_string(&owned).unwrap().contains("owned"));
}

#[test]
fn a_held_record_is_not_available() {
    let mut src = Src::default();
    src.put("item", "H", plain(), 1);
    let mut held_entry = entry(1, "a", "item", "H", 1);
    held_entry.major = 2; // the node decodes major 1 only
    let mut r = relay_with(cfg(), vec![held_entry], src);
    r.add_destination(
        Destination::new(
            "d",
            Box::new(Scope::of(&["item"])),
            Box::new(StripSecret),
            report(&[("item", &[1][..])]),
        )
        .with_scope_identity(SCOPE),
    )
    .unwrap();
    r.tick(0).unwrap();
    assert_eq!(r.held_entries("d"), vec![(1, k("item", "H"))]);
    let ans = r.serve_pull("d", &pull(vec![k("item", "H")])).unwrap();
    assert_eq!(ans.items, vec![PullItem::NotAvailable]);
    assert_eq!(ans.report.held, 1);
}

#[test]
fn a_deleted_in_scope_record_is_gone_and_an_out_of_scope_one_is_not() {
    let mut src = Src::default();
    tombstone(&mut src, "item", "D1", 4);
    tombstone(&mut src, "order", "D2", 5);
    let mut r = relay_with(cfg(), vec![], src);
    r.add_destination(scoped_dest("d", Scope::of(&["item"])))
        .unwrap();
    let ans = r
        .serve_pull("d", &pull(vec![k("item", "D1"), k("order", "D2")]))
        .unwrap();
    assert_eq!(ans.items, vec![PullItem::Gone, PullItem::NotAvailable]);
    assert_eq!(ans.report.gone, 1);
    assert_eq!(ans.report.out_of_scope, 1);
}

#[test]
fn a_long_request_is_cut_and_counted_and_the_tick_allowance_is_enforced() {
    let mut src = Src::default();
    src.put("item", "A", plain(), 1);
    let mut c = cfg();
    c.max_pull_keys = 2;
    c.max_pull_keys_per_tick = 3;
    let mut r = relay_with(c, vec![], src);
    r.add_destination(scoped_dest("d", Scope::of(&["item"])))
        .unwrap();
    let keys: Vec<RecordKey> = (0..5).map(|i| k("item", &format!("A{i}"))).collect();
    let first = r.serve_pull("d", &pull(keys.clone())).unwrap();
    assert_eq!(first.items.len(), 5, "one item per requested key");
    assert_eq!(first.report.truncated, 3);
    assert!(
        first.items[2..]
            .iter()
            .all(|i| *i == PullItem::NotAvailable)
    );
    // 2 answered; 2 more would make 4 > 3: refused whole, nothing counted.
    let err = r.serve_pull("d", &pull(keys.clone())).unwrap_err();
    assert_eq!(err.code(), PULL_THROTTLED);
    // One key still fits in the allowance.
    assert!(r.serve_pull("d", &pull(vec![k("item", "A")])).is_ok());
    // The next tick starts a fresh allowance.
    r.tick(1).unwrap();
    assert!(r.serve_pull("d", &pull(keys)).is_ok());
}

#[test]
fn a_resend_with_the_wrong_scope_is_refused_and_changes_nothing() {
    let mut r = relay_with(cfg(), vec![entry(1, "a", "item", "A", 1)], Src::default());
    r.add_destination(scoped_dest("d", Scope::of(&["item"])))
        .unwrap();
    let before = r.snapshot();
    for (tenant, scope) in [(TENANT, "branch:other"), ("other-tenant", SCOPE)] {
        let err = r
            .serve_resend(
                "d",
                &ScopedResend {
                    tenant: tenant.to_owned(),
                    scope: scope.to_owned(),
                    from_position: Some(0),
                },
            )
            .unwrap_err();
        assert_eq!(err.code(), RESEND_SCOPE_MISMATCH);
        assert!(
            !err.to_string().contains("branch:other"),
            "the named scope is not echoed"
        );
    }
    assert_eq!(r.snapshot(), before);
    assert!(matches!(
        r.serve_resend(
            "nobody",
            &ScopedResend {
                tenant: TENANT.to_owned(),
                scope: SCOPE.to_owned(),
                from_position: None,
            }
        ),
        Err(HubError::UnknownDestination { .. })
    ));
}

#[test]
fn a_destination_without_a_registered_scope_identity_gets_no_resend() {
    let mut r = relay_with(cfg(), vec![], Src::default());
    r.add_destination(dest("d", &["item"])).unwrap();
    let err = r
        .serve_resend(
            "d",
            &ScopedResend {
                tenant: TENANT.to_owned(),
                scope: SCOPE.to_owned(),
                from_position: None,
            },
        )
        .unwrap_err();
    assert_eq!(err.code(), RESEND_SCOPE_MISMATCH);
}

fn resend(from: Option<u64>) -> ScopedResend {
    ScopedResend {
        tenant: TENANT.to_owned(),
        scope: SCOPE.to_owned(),
        from_position: from,
    }
}

#[test]
fn a_resend_from_zero_sends_everything_again_and_a_huge_position_is_clamped_to_the_head() {
    let mut src = Src::default();
    src.put("item", "A", plain(), 1);
    src.put("item", "B", plain(), 2);
    let entries = vec![entry(1, "a", "item", "A", 1), entry(2, "b", "item", "B", 1)];
    let q = SharedQueue::default();
    let mut r = HubRelay::new(
        cfg(),
        memory_stream(entries),
        q.clone(),
        src,
        Box::new(Refs),
    )
    .unwrap();
    r.add_destination(scoped_dest("d", Scope::of(&["item"])))
        .unwrap();
    r.tick(0).unwrap();
    r.on_ack("d", 2).unwrap();
    assert!(
        q.0.borrow().all("d").is_empty(),
        "the ack trimmed the queue"
    );

    // From the start: the acknowledged prefix is lowered too, so the data is queued again.
    r.serve_resend("d", &resend(Some(0))).unwrap();
    assert_eq!(r.behind_from("d"), Some(0));
    r.tick(1).unwrap();
    let again: Vec<u64> = served(&q, "d").into_iter().map(|(_, p)| p).collect();
    assert_eq!(again, vec![1, 2]);

    // u64::MAX is clamped to the head (2): catch-up never starts beyond the hub's own stream.
    let mut r2 = relay_with(
        cfg(),
        vec![entry(1, "a", "item", "A", 1), entry(2, "b", "item", "B", 1)],
        Src::default(),
    );
    r2.add_destination(scoped_dest("d", Scope::of(&["item"])))
        .unwrap();
    r2.serve_resend("d", &resend(Some(u64::MAX))).unwrap();
    assert_eq!(r2.behind_from("d"), Some(2));
    // Never raised: a later, higher request leaves the lower position alone.
    r2.serve_resend("d", &resend(Some(2))).unwrap();
    r2.serve_resend("d", &resend(Some(1))).unwrap();
    assert_eq!(r2.behind_from("d"), Some(1));
    r2.serve_resend("d", &resend(Some(2))).unwrap();
    assert_eq!(r2.behind_from("d"), Some(1));
}

struct NodeScope;

impl ScopeCheck for NodeScope {
    fn in_scope(&self, _tenant: &str, _key: &RecordKey) -> bool {
        true
    }
    fn scope(&self, _tenant: &str) -> String {
        SCOPE.to_owned()
    }
    fn applied_prefix(&self, _tenant: &str) -> Option<u64> {
        Some(1)
    }
}

#[test]
fn the_real_repair_loop_asks_the_hub_and_resolves_what_it_is_answered() {
    // A parked bundle at position 10 lacks a record that exists and one that was deleted.
    let mut src = Src::default();
    src.put("item", "HERE", json!({ "note": "x", "secret": "s" }), 7);
    tombstone(&mut src, "item", "DELETED", 8);
    let mut r = relay_with(cfg(), vec![], src);
    r.add_destination(scoped_dest("d", Scope::of(&["item"])))
        .unwrap();

    let mut node = RepairLoop::new("d", RepairConfig::default()).unwrap();
    let missing = vec![k("item", "HERE"), k("item", "DELETED")];
    node.step(
        Input::Missed {
            tenant: TENANT.to_owned(),
            keys: missing.clone(),
            bundle_position: 10,
            now_ms: 0,
        },
        &NodeScope,
    );
    let asks: Vec<PullRequest> = node
        .step(Input::Tick(0), &NodeScope)
        .into_iter()
        .filter_map(|o| match o {
            Output::Ask(req) => Some(req),
            _ => None,
        })
        .collect();
    assert_eq!(asks.len(), 1);
    let ans = r.serve_pull("d", &asks[0]).unwrap();

    // Apply the answer as the node would: arrived records and gone keys feed the loop back.
    let mut arrived = Vec::new();
    let mut gone = Vec::new();
    for (key, item) in asks[0].keys.iter().zip(&ans.items) {
        match item {
            PullItem::Record(rec) => {
                assert!(rec.image.get("secret").is_none());
                arrived.push(key.clone());
            }
            PullItem::Gone => gone.push(key.clone()),
            PullItem::NotAvailable => panic!("both keys are answerable"),
        }
    }
    assert_eq!(arrived, vec![k("item", "HERE")]);
    assert_eq!(gone, vec![k("item", "DELETED")]);
    node.step(
        Input::Arrived {
            tenant: TENANT.to_owned(),
            keys: arrived,
        },
        &NodeScope,
    );
    let out = node.step(
        Input::Gone {
            tenant: TENANT.to_owned(),
            keys: gone,
        },
        &NodeScope,
    );
    assert!(out.iter().any(|o| matches!(o, Output::Gone { .. })));
    assert_eq!(
        node.tracked_len(),
        0,
        "nothing is missing any more: the parked bundle is released"
    );
}
