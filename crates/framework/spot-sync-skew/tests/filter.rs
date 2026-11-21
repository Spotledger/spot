//! Tests for the hub-side hold filter.
mod common;

use std::collections::BTreeMap;

use common::{decodes, report};
use proptest::prelude::*;
use spot_sync_skew::{
    HOLD_RETIRED, HOLDS_FULL, HoldReason, HoldSet, NodeState, NodeStatus, Outgoing, RecordRef,
    filter_outgoing, filter_outgoing_with_fleet, release_for_upgrade,
};

fn out(position: u64, id: &str, version: u64, major: u32) -> Outgoing {
    Outgoing {
        position,
        record: RecordRef {
            entity: "Invoice".into(),
            id: id.into(),
            version,
        },
        topic: "invoice".into(),
        major,
    }
}

fn versions(entries: &[Outgoing]) -> Vec<(u64, u64)> {
    entries
        .iter()
        .map(|e| (e.position, e.record.version))
        .collect()
}

#[test]
fn decodable_entries_go_out_in_input_order() {
    let node = decodes(report(5), "invoice", &[1]);
    let mut holds = HoldSet::new();
    let batch = [out(1, "a", 1, 1), out(2, "b", 1, 1), out(3, "a", 2, 1)];
    let r = filter_outgoing(&batch, &node, &mut holds);
    assert_eq!(r.send, batch.to_vec());
    assert!(r.held.is_empty() && holds.is_empty());
    assert_eq!(r.node_status, None);
}

#[test]
fn version_two_held_then_three_withheld_and_release_gives_two_then_three() {
    let old = decodes(report(5), "invoice", &[1]);
    let mut holds = HoldSet::new();
    let batch = [out(1, "a", 2, 2), out(2, "a", 3, 1)];
    let r = filter_outgoing(&batch, &old, &mut holds);
    assert!(r.send.is_empty());
    assert_eq!(r.held.len(), 2);
    assert!(matches!(
        r.held[0].reason,
        HoldReason::UnsupportedMajor { major: 2, .. }
    ));
    assert_eq!(
        r.held[1].reason,
        HoldReason::BehindHeldVersion { blocker: 1 }
    );
    assert!(
        r.held
            .iter()
            .all(|h| !h.blocks_applied_prefix && !h.obligation_satisfied)
    );
    assert_eq!(holds.pending_len(), 2);
    let new = decodes(report(6), "invoice", &[1, 2]);
    let released = release_for_upgrade(&new, &mut holds);
    assert_eq!(versions(&released), vec![(1, 2), (2, 3)]);
    assert!(holds.is_empty());
}

#[test]
fn later_call_for_a_held_key_is_withheld_too() {
    let old = decodes(report(5), "invoice", &[1]);
    let mut holds = HoldSet::new();
    let _ = filter_outgoing(&[out(1, "a", 2, 2)], &old, &mut holds);
    let r = filter_outgoing(&[out(2, "a", 3, 1), out(3, "b", 1, 1)], &old, &mut holds);
    assert_eq!(versions(&r.send), vec![(3, 1)]);
    assert_eq!(
        r.held[0].reason,
        HoldReason::BehindHeldVersion { blocker: 1 }
    );
}

#[test]
fn resubmitted_entry_is_not_queued_twice() {
    let old = decodes(report(5), "invoice", &[1]);
    let mut holds = HoldSet::new();
    let batch = [out(1, "a", 2, 2)];
    let _ = filter_outgoing(&batch, &old, &mut holds);
    let r = filter_outgoing(&batch, &old, &mut holds);
    assert_eq!(r.held.len(), 1);
    assert_eq!(holds.pending_len(), 1);
}

#[test]
fn interleaved_keys_keep_independent_order() {
    let old = decodes(report(5), "invoice", &[1]);
    let mut holds = HoldSet::new();
    let batch = [
        out(1, "a", 2, 2),
        out(2, "b", 1, 1),
        out(3, "a", 3, 1),
        out(4, "b", 2, 1),
    ];
    let r = filter_outgoing(&batch, &old, &mut holds);
    assert_eq!(versions(&r.send), vec![(2, 1), (4, 2)]);
    let released = release_for_upgrade(&decodes(report(6), "invoice", &[1, 2]), &mut holds);
    assert_eq!(versions(&released), vec![(1, 2), (3, 3)]);
}

#[test]
fn upgrade_releases_only_what_the_new_report_decodes() {
    let old = decodes(report(5), "invoice", &[1]);
    let mut holds = HoldSet::new();
    // v2 needs major 2, v3 needs major 3: an upgrade to major 2 releases only v2.
    let _ = filter_outgoing(
        &[out(1, "a", 2, 2), out(2, "a", 3, 3), out(3, "a", 4, 3)],
        &old,
        &mut holds,
    );
    let mid = decodes(report(6), "invoice", &[1, 2]);
    assert_eq!(
        versions(&release_for_upgrade(&mid, &mut holds)),
        vec![(1, 2)]
    );
    assert_eq!(holds.pending_len(), 2);
    assert!(matches!(
        holds.pending_holds()[0].reason,
        HoldReason::UnsupportedMajor { major: 3, .. }
    ));
    assert_eq!(
        holds.pending_holds()[1].reason,
        HoldReason::BehindHeldVersion { blocker: 2 }
    );
    // No change in report: nothing released.
    assert!(release_for_upgrade(&mid, &mut holds).is_empty());
    let top = decodes(report(7), "invoice", &[1, 2, 3]);
    assert_eq!(
        versions(&release_for_upgrade(&top, &mut holds)),
        vec![(2, 3), (3, 4)]
    );
}

#[test]
fn released_result_is_globally_position_ordered() {
    let old = decodes(report(5), "invoice", &[1]);
    let mut holds = HoldSet::new();
    let batch = [
        out(1, "z", 2, 2),
        out(2, "a", 2, 2),
        out(3, "z", 3, 2),
        out(4, "a", 3, 2),
    ];
    let _ = filter_outgoing(&batch, &old, &mut holds);
    let released = release_for_upgrade(&decodes(report(6), "invoice", &[2]), &mut holds);
    assert_eq!(
        released.iter().map(|e| e.position).collect::<Vec<_>>(),
        vec![1, 2, 3, 4]
    );
}

fn fleet_of(majors: &[(&str, &[u32])]) -> Vec<NodeState> {
    majors
        .iter()
        .map(|(node, m)| NodeState {
            node: (*node).into(),
            status: NodeStatus::Active,
            report: decodes(report(6), "invoice", m),
        })
        .collect()
}

#[test]
fn retired_major_is_a_permanent_hold_and_the_node_is_too_old() {
    // The node dropped major 1; the whole fleet decodes major 2, so major 1 is retired.
    let node = decodes(report(6), "invoice", &[2]);
    let fleet = fleet_of(&[("n1", &[2]), ("n2", &[2])]);
    let mut holds = HoldSet::new();
    let r = filter_outgoing_with_fleet(&[out(1, "a", 1, 1)], &node, &mut holds, &fleet);
    assert!(matches!(
        r.held[0].reason,
        HoldReason::Retired { major: 1, .. }
    ));
    assert_eq!(r.node_status, Some(NodeStatus::TooOld));
    assert_eq!(r.condition, Some(HOLD_RETIRED));
    assert!(r.send.is_empty() && holds.is_empty());
}

#[test]
fn major_not_retired_while_an_active_node_cannot_decode_the_next() {
    let node = decodes(report(5), "invoice", &[1]);
    let fleet = fleet_of(&[("n1", &[1]), ("n2", &[1, 2])]);
    let mut holds = HoldSet::new();
    let r = filter_outgoing_with_fleet(&[out(1, "a", 2, 2)], &node, &mut holds, &fleet);
    assert!(matches!(
        r.held[0].reason,
        HoldReason::UnsupportedMajor { major: 2, .. }
    ));
    assert_eq!(r.node_status, None);
    assert_eq!(holds.pending_len(), 1);
}

#[test]
fn full_hold_set_reports_too_old_and_stores_no_more() {
    let old = decodes(report(5), "invoice", &[1]);
    let mut holds = HoldSet::with_limit(2);
    let batch = [
        out(1, "a", 2, 2),
        out(2, "b", 2, 2),
        out(3, "c", 2, 2),
        out(4, "d", 1, 1),
    ];
    let r = filter_outgoing(&batch, &old, &mut holds);
    assert_eq!(r.held.len(), 2);
    assert_eq!(versions(&r.unstored), vec![(3, 2)]);
    assert_eq!(versions(&r.send), vec![(4, 1)]);
    assert_eq!(r.node_status, Some(NodeStatus::TooOld));
    assert_eq!(r.condition, Some(HOLDS_FULL));
    assert_eq!(holds.pending_len(), 2);
    assert_eq!(holds.limit(), 2);
}

#[test]
fn default_limit_is_one_hundred_thousand() {
    assert_eq!(HoldSet::new().limit(), 100_000);
}

proptest! {
    /// For any input and any sequence of upgrades, per key the delivered versions strictly increase.
    #[test]
    fn per_key_delivery_is_strictly_increasing(
        raw in proptest::collection::vec((0usize..4, 1u32..4), 1..60),
        upgrades in proptest::collection::vec(1u32..4, 0..6),
    ) {
        let mut next_version: BTreeMap<usize, u64> = BTreeMap::new();
        let batch: Vec<Outgoing> = raw
            .iter()
            .enumerate()
            .map(|(i, (k, major))| {
                let v = next_version.entry(*k).or_insert(0);
                *v += 1;
                out(i as u64 + 1, &format!("k{k}"), *v, *major)
            })
            .collect();
        let mut holds = HoldSet::new();
        let mut delivered: Vec<Outgoing> = Vec::new();
        // Capability only grows: the node decodes majors 1..=top.
        let mut top = 1u32;
        let node = |top: u32| decodes(report(5), "invoice", &(1..=top).collect::<Vec<_>>());
        // Feed in chunks, upgrading between them.
        for (i, chunk) in batch.chunks(7).enumerate() {
            let r = filter_outgoing(chunk, &node(top), &mut holds);
            delivered.extend(r.send);
            if let Some(up) = upgrades.get(i) {
                top = top.max(*up);
                delivered.extend(release_for_upgrade(&node(top), &mut holds));
            }
        }
        delivered.extend(release_for_upgrade(&node(3), &mut holds));
        prop_assert!(holds.is_empty());
        let mut last: BTreeMap<String, u64> = BTreeMap::new();
        for e in &delivered {
            let prev = last.insert(e.record.id.clone(), e.record.version).unwrap_or(0);
            prop_assert!(e.record.version > prev);
        }
        prop_assert_eq!(delivered.len(), batch.len());
    }
}
