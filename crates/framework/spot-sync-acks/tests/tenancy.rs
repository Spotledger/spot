//! The ledger is keyed by `(node, tenant)`: one tenant's ack, hold or check-in can never satisfy,
//! move or hide another tenant's obligation (WP-SYN-05.T5, ADR-061 s1, ADR-065).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use proptest::prelude::*;
use spot_conditions::MemorySink;
use spot_sync_acks::{AckError, AckLedger, MemoryStore, NodeStateStore, StreamHead};
use spot_sync_obligations::{Level, MandatoryEvent, ObligationSettings, ReportStatus};

const H: i64 = 3_600_000;
const A: &str = "tenant-a";
const B: &str = "tenant-b";

type Ledger = AckLedger<MemoryStore, MemorySink>;

fn event(position: u64) -> MandatoryEvent {
    MandatoryEvent {
        position,
        published_at_millis: 0,
        effective_from_millis: None,
        record: format!("tax_rate/{position}"),
    }
}

fn ledger() -> Ledger {
    AckLedger::with_catalogue(
        MemoryStore::new(),
        MemorySink::new(),
        ObligationSettings::default(),
    )
    .unwrap()
}

/// One node serving two tenants, each with a mandatory event at the same position 40.
fn two_tenants() -> Ledger {
    let mut l = ledger();
    assert!(l.register_node("n1", A, "1", 0).unwrap());
    assert!(l.register_node("n1", B, "1", 0).unwrap());
    assert!(l.register_mandatory(A, event(40)));
    assert!(l.register_mandatory(B, event(40)));
    l
}

/// Everything the ledger holds about tenant `t`, as text, for byte-for-byte comparison.
fn tenant_view(l: &Ledger, t: &str) -> String {
    let suffix = format!("@tenant:{t}");
    let open: Vec<String> = l
        .conditions()
        .open()
        .iter()
        .filter(|o| o.subject.as_str().ends_with(&suffix))
        .map(|o| format!("{o:?}"))
        .collect();
    let audit: Vec<String> = l
        .conditions()
        .sink()
        .events()
        .iter()
        .filter(|e| e.subject().as_str().ends_with(&suffix))
        .map(|e| format!("{e:?}"))
        .collect();
    format!(
        "{:?}|{:?}|{:?}|{open:?}|{audit:?}",
        l.store().load("n1", t),
        l.store().events(t),
        l.level("n1", t),
    )
}

#[test]
fn an_ack_for_one_tenant_never_satisfies_the_other_tenants_event() {
    let mut l = two_tenants();
    l.tick(30 * H).unwrap();
    assert_eq!(l.level("n1", A), Some(Level::Red));
    assert_eq!(l.level("n1", B), Some(Level::Red));
    let b_before = tenant_view(&l, B);

    let out = l
        .on_ack("n1", A, 500, StreamHead::new(500), 31 * H)
        .unwrap();
    assert_eq!(out.satisfied.unwrap().count, 1);

    // Tenant A is satisfied, with evidence.
    assert_eq!(
        l.evidence(A, 40, "n1").unwrap().status,
        ReportStatus::AppliedAt(31 * H)
    );
    assert!(l.who_is_missing(A, 40).unwrap().is_empty());
    assert_eq!(l.level("n1", A), Some(Level::Ok));

    // Tenant B is untouched: still outstanding, same level, same conditions, same audit trail.
    assert!(matches!(
        l.evidence(B, 40, "n1").unwrap().status,
        ReportStatus::OutstandingSince { .. }
    ));
    let missing = l.who_is_missing(B, 40).unwrap();
    assert_eq!(missing.len(), 1);
    assert_eq!(
        (missing[0].node.as_str(), missing[0].tenant.as_str()),
        ("n1", B)
    );
    assert_eq!(missing[0].applied_prefix, 0);
    assert_eq!(l.level("n1", B), Some(Level::Red));
    assert_eq!(tenant_view(&l, B), b_before);
    assert_eq!(l.store().load("n1", B).unwrap().applied_prefix, 0);
}

#[test]
fn conditions_are_one_per_node_and_tenant() {
    let mut l = two_tenants();
    l.tick(30 * H).unwrap();
    let subjects: Vec<String> = l
        .conditions()
        .open()
        .iter()
        .map(|o| o.subject.as_str().to_owned())
        .collect();
    assert_eq!(subjects.len(), 6);
    assert!(subjects.contains(&"node:n1@tenant:tenant-a".to_owned()));
    assert!(subjects.contains(&"node:n1@tenant:tenant-b".to_owned()));

    // Clearing A leaves B's three conditions open.
    l.on_ack("n1", A, 40, StreamHead::new(40), 31 * H).unwrap();
    let open = l.conditions().open();
    assert_eq!(open.len(), 3);
    assert!(
        open.iter()
            .all(|o| o.subject.as_str() == "node:n1@tenant:tenant-b")
    );
}

#[test]
fn notifications_and_actions_name_their_tenant() {
    let mut l = two_tenants();
    let r = l.tick(30 * H).unwrap();
    for t in [A, B] {
        assert!(r.notifications.iter().any(|n| n.tenant == t));
        assert!(r.actions.iter().any(|a| a.tenant == t));
        assert!(
            r.level_changes
                .iter()
                .any(|c| c.tenant == t && c.node == "n1" && c.level == Level::Red)
        );
    }
}

#[test]
fn an_ack_for_an_unregistered_pair_is_unknown_node_and_changes_nothing() {
    let mut l = two_tenants();
    l.tick(30 * H).unwrap();
    let (a, b) = (tenant_view(&l, A), tenant_view(&l, B));
    let audit = l.conditions().sink().events().len();
    let err = l
        .on_ack("n1", "tenant-c", 500, StreamHead::new(500), 31 * H)
        .unwrap_err();
    assert_eq!(
        err,
        AckError::UnknownNode {
            node: "n1".into(),
            tenant: "tenant-c".into()
        }
    );
    assert_eq!(err.code(), "sync_acks.unknown_node");
    // The holds path refuses the same way.
    assert_eq!(
        l.set_holds("n1", "tenant-c", vec![], StreamHead::new(500), 31 * H)
            .unwrap_err()
            .code(),
        "sync_acks.unknown_node"
    );
    assert_eq!((tenant_view(&l, A), tenant_view(&l, B)), (a, b));
    assert_eq!(l.conditions().sink().events().len(), audit);
    // A node that is registered only for tenant A cannot ack for tenant B either.
    let mut one = ledger();
    one.register_node("n9", A, "1", 0).unwrap();
    assert!(one.on_ack("n9", B, 1, StreamHead::new(5), H).is_err());
    assert!(one.who_is_missing(B, 1).is_err());
}

#[test]
fn the_stream_head_and_the_holds_are_per_tenant() {
    let mut l = two_tenants();
    // Tenant A's stream is long, tenant B's is short: a prefix valid for A is beyond B's head.
    l.on_ack("n1", A, 500, StreamHead::new(500), H).unwrap();
    let err = l.on_ack("n1", B, 500, StreamHead::new(45), H).unwrap_err();
    assert_eq!(
        err,
        AckError::PrefixBeyondHead {
            node: "n1".into(),
            tenant: B.into(),
            claimed: 500,
            head: 45
        }
    );
    // A hold at 40 is within B's head of 45 but a hold at 90 is not, whatever A's head is.
    l.register_mandatory(A, event(90));
    l.register_mandatory(B, event(90));
    let err = l
        .set_holds("n1", B, vec![40, 90], StreamHead::new(45), H)
        .unwrap_err();
    assert_eq!(err.code(), "sync_acks.hold_beyond_head");
    // A hold registered only in A's position space is unknown to B.
    l.register_mandatory(A, event(70));
    let err = l
        .set_holds("n1", B, vec![70], StreamHead::new(500), H)
        .unwrap_err();
    assert_eq!(
        err,
        AckError::HoldUnknown {
            node: "n1".into(),
            tenant: B.into(),
            position: 70
        }
    );
    assert_eq!(l.store().load("n1", B).unwrap().applied_prefix, 0);
    assert!(l.store().load("n1", B).unwrap().holds.is_empty());
}

#[test]
fn registered_events_are_per_tenant_and_idempotent_by_tenant_and_position() {
    let mut l = ledger();
    assert!(l.register_mandatory(A, event(40)));
    assert!(!l.register_mandatory(A, event(40)));
    assert!(l.register_mandatory(B, event(40)));
    assert_eq!(l.store().events(A).len(), 1);
    assert_eq!(l.store().events(B).len(), 1);
    assert!(l.store().events("tenant-c").is_empty());
    // An event asked about under the wrong tenant is unknown.
    l.register_mandatory(A, event(41));
    assert_eq!(
        l.who_is_missing(B, 41).unwrap_err(),
        AckError::UnknownEvent {
            tenant: B.into(),
            position: 41
        }
    );
}

#[test]
fn ids_containing_at_are_refused() {
    let mut l = ledger();
    for (node, tenant, bad) in [
        ("n@1", A, "n@1"),
        ("n1", "t@x", "t@x"),
        ("a@tenant:b", "c", "a@tenant:b"),
    ] {
        let err = l.register_node(node, tenant, "1", 0).unwrap_err();
        assert_eq!(err, AckError::IdInvalid { id: bad.into() });
        assert_eq!(err.code(), "sync_acks.id_invalid");
    }
    assert!(l.store().nodes().is_empty());
}

#[test]
fn version_is_node_level_and_checkin_is_per_pair() {
    let mut l = two_tenants();
    l.register_node("n2", A, "1", 0).unwrap();
    l.on_checkin("n1", A, 7 * H).unwrap();
    l.set_version("n1", "2.0").unwrap();
    let a = l.store().load("n1", A).unwrap();
    assert_eq!((a.last_contact_millis, a.version.as_str()), (7 * H, "2.0"));
    // The version reached the other tenant's record; the check-in did not.
    let b = l.store().load("n1", B).unwrap();
    assert_eq!((b.last_contact_millis, b.version.as_str()), (0, "2.0"));
    // Another node is untouched.
    let other = l.store().load("n2", A).unwrap();
    assert_eq!(
        (other.last_contact_millis, other.version.as_str()),
        (0, "1")
    );
    // A node with no record at all is unknown, the tenant filled in for a check-in.
    assert_eq!(
        l.on_checkin("n3", A, H).unwrap_err(),
        AckError::UnknownNode {
            node: "n3".into(),
            tenant: A.into()
        }
    );
    assert_eq!(
        l.set_version("n3", "1").unwrap_err().code(),
        "sync_acks.unknown_node"
    );
}

#[test]
fn a_checkin_for_one_tenant_leaves_the_other_tenants_record_and_overdue_condition_alone() {
    let mut l = two_tenants();
    let before_b = tenant_view(&l, B);
    l.on_checkin("n1", A, 20 * H).unwrap();
    assert_eq!(l.store().load("n1", A).unwrap().last_contact_millis, 20 * H);
    assert_eq!(tenant_view(&l, B), before_b);
    assert_eq!(l.store().load("n1", B).unwrap().last_contact_millis, 0);
    // B still goes overdue on schedule.
    let report = l.tick(25 * H).unwrap();
    assert_eq!(l.level("n1", B), Some(Level::Overdue));
    assert!(
        report.notifications.iter().any(|n| n.tenant == B),
        "B's overdue notification must be raised"
    );
}

#[test]
fn the_mirror_checkin_on_b_leaves_a_unchanged() {
    let mut l = two_tenants();
    let before_a = tenant_view(&l, A);
    l.on_checkin("n1", B, 20 * H).unwrap();
    assert_eq!(tenant_view(&l, A), before_a);
    l.tick(25 * H).unwrap();
    assert_eq!(l.level("n1", A), Some(Level::Overdue));
}

#[test]
fn a_checkin_for_an_unregistered_pair_names_the_tenant_and_changes_nothing() {
    let mut l = two_tenants();
    l.register_node("n2", A, "1", 0).unwrap();
    let before = (tenant_view(&l, A), tenant_view(&l, B), l.store().nodes());
    // n2 exists, but only for A: the pair (n2, B) is what is unknown.
    let err = l.on_checkin("n2", B, 5 * H).unwrap_err();
    assert_eq!(
        err,
        AckError::UnknownNode {
            node: "n2".into(),
            tenant: B.into()
        }
    );
    assert_eq!(err.code(), "sync_acks.unknown_node");
    assert!(l.on_checkin("n9", A, 5 * H).is_err());
    assert!(l.on_checkin("n1", "tenant-c", 5 * H).is_err());
    assert_eq!(
        before,
        (tenant_view(&l, A), tenant_view(&l, B), l.store().nodes())
    );
    assert_eq!(l.store().load("n2", A).unwrap().last_contact_millis, 0);
}

#[test]
fn a_checkin_never_moves_the_contact_backwards_or_touches_prefix_holds_or_crossings() {
    let mut l = two_tenants();
    l.on_ack("n1", A, 40, StreamHead::new(100), 3 * H).unwrap();
    l.on_checkin("n1", A, 9 * H).unwrap();
    let r = l.store().load("n1", A).unwrap();
    let (prefix, holds, crossings) = (r.applied_prefix, r.holds.clone(), r.crossings.clone());
    l.on_checkin("n1", A, 2 * H).unwrap();
    let r = l.store().load("n1", A).unwrap();
    assert_eq!(r.last_contact_millis, 9 * H);
    assert_eq!(
        (r.applied_prefix, r.holds, r.crossings),
        (prefix, holds, crossings)
    );
}

#[test]
fn a_checkin_on_an_overdue_pair_changes_only_that_pair() {
    let mut l = two_tenants();
    l.tick(25 * H).unwrap();
    assert_eq!(l.level("n1", A), Some(Level::Overdue));
    assert_eq!(l.level("n1", B), Some(Level::Overdue));
    let before_b = tenant_view(&l, B);
    l.on_checkin("n1", A, 26 * H).unwrap();
    assert_eq!(l.store().load("n1", A).unwrap().last_contact_millis, 26 * H);
    assert_eq!(tenant_view(&l, B), before_b);
}

proptest! {
    #[test]
    fn operations_addressed_to_a_never_change_b(
        ops in proptest::collection::vec((0u8..2, 0u64..60, 0i64..48), 0..30),
    ) {
        let mut l = two_tenants();
        let before = tenant_view(&l, B);
        for (op, prefix, at) in ops {
            if op == 0 {
                let _ = l.on_checkin("n1", A, at * H);
            } else {
                let _ = l.on_ack("n1", A, prefix, StreamHead::new(50), at * H);
            }
            prop_assert_eq!(tenant_view(&l, B), before.clone());
        }
    }
}

#[test]
fn nodes_are_listed_in_tenant_then_node_order() {
    let mut l = ledger();
    l.register_node("z", A, "1", 0).unwrap();
    l.register_node("a", B, "1", 0).unwrap();
    l.register_node("m", A, "1", 0).unwrap();
    let pairs: Vec<(String, String)> = l.store().nodes();
    let expect = [("m", A), ("z", A), ("a", B)];
    assert_eq!(
        pairs,
        expect
            .iter()
            .map(|(n, t)| ((*n).to_owned(), (*t).to_owned()))
            .collect::<Vec<_>>()
    );
}

#[derive(Clone, Debug)]
enum Op {
    Ack { prefix: u64, head: u64 },
    Holds { positions: Vec<u64>, head: u64 },
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        (0u64..700, 0u64..700).prop_map(|(prefix, head)| Op::Ack { prefix, head }),
        (
            proptest::collection::vec(prop_oneof![Just(5u64), Just(40), Just(90), 0u64..700], 0..4),
            0u64..700
        )
            .prop_map(|(positions, head)| Op::Holds { positions, head }),
    ]
}

proptest! {
    /// Any sequence of acks and holds (accepted or refused) addressed to tenant A leaves every
    /// row, level and condition of tenant B byte-for-byte unchanged.
    #[test]
    fn nothing_addressed_to_tenant_a_changes_tenant_b(
        ops in proptest::collection::vec(op(), 1..40),
    ) {
        let mut l = ledger();
        for t in [A, B] {
            l.register_node("n1", t, "1", 0).unwrap();
            l.register_node("n2", t, "1", 0).unwrap();
            for p in [5, 40, 90] {
                l.register_mandatory(t, event(p));
            }
        }
        l.on_ack("n2", B, 5, StreamHead::new(10), H).unwrap();
        l.tick(30 * H).unwrap();
        let before = tenant_view(&l, B);
        let n2_before = format!("{:?}", l.store().load("n2", B));
        let mut now = 31 * H;
        for o in ops {
            now += H;
            let _ = match o {
                Op::Ack { prefix, head } => l.on_ack("n1", A, prefix, StreamHead::new(head), now),
                Op::Holds { positions, head } => {
                    l.set_holds("n1", A, positions, StreamHead::new(head), now)
                }
            };
            prop_assert_eq!(tenant_view(&l, B), before.clone());
            prop_assert_eq!(format!("{:?}", l.store().load("n2", B)), n2_before.clone());
        }
    }
}
