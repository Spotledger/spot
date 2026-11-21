//! Two nodes on one database (WP-TST-11.T3): a change on node A reaches node B's metadata, plan
//! and engine; a refused change moves neither; a race is serialised and both nodes agree.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use spot_l10_stack::StackHost;

const WITHIN: Duration = Duration::from_secs(10);

fn boot() -> Option<StackHost> {
    let host = StackHost::boot();
    if host.is_none() {
        println!("SKIPPED: set SPOT_TEST_PG_URL to run the L10 stack");
    }
    host
}

#[test]
fn a_change_on_node_a_reaches_node_b_metadata_plan_and_engine() {
    let Some(a) = boot() else { return };
    let b = a.follower();
    let before = b.version();
    let version = a.add_field("custom_seen").unwrap();
    assert!(version > before);
    let took = b.converge_to(version, WITHIN).unwrap();
    println!("node B converged in {took:?}");
    assert_eq!(b.version(), a.version());
    assert_eq!(b.plan_version(), version, "B's plan layer applied it");
    assert_eq!(b.hash(), a.hash(), "same metadata content");
    // B's engine behaves with the new field at once.
    b.writer()
        .write_through("b-code-1", Some("custom_seen"))
        .unwrap();
    a.writer()
        .write_through("a-code-1", Some("custom_seen"))
        .unwrap();
}

#[test]
fn a_refused_change_on_node_a_moves_neither_node() {
    let Some(a) = boot() else { return };
    let b = a.follower();
    let version = a.add_field("custom_ok").unwrap();
    b.converge_to(version, WITHIN).unwrap();
    // `code` is a standard field of `party`: adding it again collides and is refused.
    let refused = a.add_field("code");
    assert!(refused.is_err(), "the collision was accepted");
    assert_eq!(a.version(), version, "A did not move");
    b.recover().unwrap();
    assert_eq!(b.version(), version, "B did not move");
    assert_eq!(a.hash(), b.hash());
}

#[test]
fn a_race_between_two_nodes_is_serialised_and_both_agree_afterwards() {
    let Some(a) = boot() else { return };
    let b = a.follower();
    let (ra, rb) = std::thread::scope(|scope| {
        let ha = scope.spawn(|| a.add_field("custom_from_a"));
        let hb = scope.spawn(|| b.add_field("custom_from_b"));
        (ha.join().unwrap(), hb.join().unwrap())
    });
    println!("A: {ra:?}, B: {rb:?}");
    // At least one wins; the loser gets a refusal (a conflict) and may retry.
    assert!(
        ra.is_ok() || rb.is_ok(),
        "both changes were refused: {ra:?} {rb:?}"
    );
    let wanted = ra
        .as_ref()
        .ok()
        .copied()
        .max(rb.as_ref().ok().copied())
        .unwrap();
    a.converge_to(wanted, WITHIN).unwrap();
    b.converge_to(wanted, WITHIN).unwrap();
    // The loser applies its change on top, now that it has converged.
    if ra.is_err() {
        a.add_field("custom_from_a").unwrap();
    }
    if rb.is_err() {
        b.add_field("custom_from_b").unwrap();
    }
    let head = a.version().max(b.version());
    a.converge_to(head, WITHIN).unwrap();
    b.converge_to(head, WITHIN).unwrap();
    assert_eq!(a.version(), b.version());
    assert_eq!(a.hash(), b.hash());
    a.writer()
        .write_through("race-a", Some("custom_from_b"))
        .unwrap();
    b.writer()
        .write_through("race-b", Some("custom_from_a"))
        .unwrap();
}
