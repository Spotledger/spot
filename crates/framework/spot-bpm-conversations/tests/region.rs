//! ADR-058 rule 6 at the conversations receiver: a grant that requires the peer in a region is
//! judged by the inbox's oracle, and refused as not granted when there is none, when it does not
//! know the peer, or when it places the peer elsewhere.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::*;
use spot_bpm_conversations::ConformantInbox;
use spot_link_exchange::{Inbox, InboxStore, Message, ReceiveStatus, RejectCode};
use spot_tenant_link::{MemoryRegionOracle, RegionOracle};

fn region(name: &str) -> spot_data_policy::Region {
    spot_data_policy::Region::new(name).unwrap()
}

/// The requester's first message, taken from its outbox, and a world whose approver side
/// optionally requires the peer in `eu`.
fn world_with_request(restricted: bool) -> (World, Message) {
    let w = World::new();
    {
        let a = w.a.boot();
        a.start_request("req1", "REF-1", T0);
        a.settle(T0);
    }
    let message = queued(&w.a, T0 + SECOND, 1).remove(0);
    if restricted {
        let mut link = w.b.link.write().unwrap();
        for grant in &mut link.grants {
            grant.require_peer_region = Some(region("eu"));
        }
    }
    (w, message)
}

fn receive(w: &World, message: &Message, oracle: Option<&dyn RegionOracle>) -> ReceiveStatus {
    let side = &w.b;
    let booted = side.boot();
    let link = side.link();
    let inbox = Inbox::new(&*side.inbox, &*side.audit, &*side.quarantine, &*side.signer);
    let inbox = match oracle {
        Some(oracle) => inbox.with_region_oracle(oracle),
        None => inbox,
    };
    ConformantInbox::new(
        &inbox,
        &*side.inbox,
        &*side.quarantine,
        &*side.signer,
        &booted.runtime,
    )
    .with_ledger(&*side.inbox)
    .receive_from(&link, &connection(&w.a.party), message, T0 + 2 * SECOND)
    .unwrap()
    .status
}

fn stored(w: &World, message: &Message) -> bool {
    w.b.inbox
        .entry(&w.b.link_id(), &message.id)
        .unwrap()
        .is_some()
}

#[test]
fn an_oracle_that_places_the_peer_in_the_required_region_lets_the_message_in() {
    let (w, message) = world_with_request(true);
    let oracle = MemoryRegionOracle::new();
    oracle.insert(w.a.party.clone(), region("eu")).unwrap();
    assert_eq!(receive(&w, &message, Some(&oracle)), ReceiveStatus::Applied);
    assert!(stored(&w, &message));
}

#[test]
fn an_oracle_that_places_the_peer_elsewhere_refuses_and_stores_nothing() {
    let (w, message) = world_with_request(true);
    let oracle = MemoryRegionOracle::new();
    oracle.insert(w.a.party.clone(), region("us")).unwrap();
    assert_eq!(
        receive(&w, &message, Some(&oracle)),
        ReceiveStatus::Rejected(RejectCode::NotGranted)
    );
    assert!(!stored(&w, &message));
}

#[test]
fn an_oracle_that_does_not_know_the_peer_refuses() {
    let (w, message) = world_with_request(true);
    let oracle = MemoryRegionOracle::new();
    assert_eq!(
        receive(&w, &message, Some(&oracle)),
        ReceiveStatus::Rejected(RejectCode::NotGranted)
    );
    assert!(!stored(&w, &message));
}

#[test]
fn no_oracle_and_a_region_restricted_grant_still_refuses() {
    let (w, message) = world_with_request(true);
    assert_eq!(
        receive(&w, &message, None),
        ReceiveStatus::Rejected(RejectCode::NotGranted)
    );
    assert!(!stored(&w, &message));
}

#[test]
fn a_grant_without_a_region_requirement_behaves_as_before_with_and_without_an_oracle() {
    let (w, message) = world_with_request(false);
    let oracle = MemoryRegionOracle::new();
    assert_eq!(receive(&w, &message, Some(&oracle)), ReceiveStatus::Applied);
    let (w, message) = world_with_request(false);
    assert_eq!(receive(&w, &message, None), ReceiveStatus::Applied);
}
