//! The `LinkStore` on PostgreSQL: round trip, optimistic versions, immutable fields, the state
//! machine, one live link per pair, restart persistence and the public-key-only key column.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{env, link_id, linked, runtime, tenant};
use spot_tenant_link::{Error, LinkState, LinkStore, MemoryPeerKeyDirectory, RevokeReason, Trust};

#[test]
fn a_finished_link_round_trips_with_everything_it_holds() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let store = e.store(1);
    let link = linked().a;
    let stored = store.insert(link.clone()).unwrap();
    assert_eq!(stored.version, 1);
    let read = store.get(&link.id).unwrap();
    assert_eq!(read, stored);
    // The grants, both consents in order, both public keys, the state and the local side.
    assert_eq!(read.state, LinkState::Active);
    assert_eq!(read.grants, link.grants);
    assert_eq!(read.consents, link.consents);
    assert_eq!(read.peer_keys, link.peer_keys);
    assert!(read.peer_keys.iter().all(Option::is_some));
    assert_eq!(read.local, tenant(1));
    assert_eq!(read.parties, link.parties);
}

#[test]
fn the_key_table_holds_only_public_keys() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let store = e.store(1);
    store.insert(linked().a).unwrap();
    let schema = common::tenant_id(1).schema_name();
    assert_eq!(
        e.columns(&schema, "spot_tenant_link_peer_key"),
        vec!["link", "party", "public_key"]
    );
    // Every stored key is exactly the 32 bytes of a public key.
    let widths: Vec<i32> = rt.block_on(async {
        let client = e.db.raw_client().await;
        client
            .batch_execute(&format!("SET search_path TO \"{schema}\""))
            .await
            .unwrap();
        client
            .query(
                "SELECT octet_length(public_key) FROM spot_tenant_link_peer_key",
                &[],
            )
            .await
            .unwrap()
            .iter()
            .map(|row| row.get(0))
            .collect()
    });
    assert_eq!(widths, vec![32, 32]);
}

#[test]
fn a_new_link_must_be_unversioned_and_ids_are_unique() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let store = e.store(1);
    let link = linked().a;
    let mut versioned = link.clone();
    versioned.version = 3;
    assert!(matches!(
        store.insert(versioned).unwrap_err(),
        Error::StoreFailed { .. }
    ));
    store.insert(link.clone()).unwrap();
    assert!(matches!(
        store.insert(link).unwrap_err(),
        Error::DuplicateLink { .. }
    ));
}

#[test]
fn updates_are_optimistic_and_two_revokers_cannot_both_win() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let store = e.store(1);
    let stored = store.insert(linked().a).unwrap();
    let peer = stored.peer().unwrap().clone();
    let by_me = stored
        .revoked_by(&stored.local, RevokeReason::TermsEnded, 5_000)
        .unwrap();
    let by_peer = stored
        .revoked_by(&peer, RevokeReason::SecurityConcern, 5_001)
        .unwrap();
    let first = store.update(by_me, 1).unwrap();
    assert_eq!((first.version, first.state), (2, LinkState::Revoked));
    // The second writer read version 1 and now finds 2.
    match store.update(by_peer, 1).unwrap_err() {
        Error::VersionConflict {
            expected, found, ..
        } => assert_eq!((expected, found), (1, 2)),
        other => panic!("{other:?}"),
    }
    let read = store.get(&stored.id).unwrap();
    assert_eq!(read.revocation.unwrap().reason, RevokeReason::TermsEnded);
}

#[test]
fn the_state_machine_and_immutable_fields_hold_in_the_database_path_too() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let store = e.store(1);
    let stored = store.insert(linked().a).unwrap();
    // Revoked is terminal.
    let revoked = store
        .update(
            stored
                .revoked_by(&stored.local, RevokeReason::TermsEnded, 5_000)
                .unwrap(),
            1,
        )
        .unwrap();
    let mut back = revoked.clone();
    back.state = LinkState::Active;
    assert!(matches!(
        store.update(back, 2).unwrap_err(),
        Error::InvalidTransition { .. }
    ));
    // The creation time never changes.
    let other = store.insert({
        let mut l = linked().a;
        l.id = link_id(9);
        l.parties = linked().a.parties;
        l
    });
    // A second link for the same pair is refused only while the first is live; this one is not.
    let other = other.unwrap();
    let mut moved = other.clone();
    moved.created += 1;
    assert!(matches!(
        store.update(moved, other.version).unwrap_err(),
        Error::InvalidParty { .. }
    ));
    // The stored link was not touched by the refusals.
    assert_eq!(store.get(&other.id).unwrap(), other);
}

#[test]
fn one_live_link_joins_a_pair_and_a_revoked_one_does_not_count() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let store = e.store(1);
    let first = store.insert(linked().a).unwrap();
    let mut second = linked().a;
    second.id = link_id(2);
    assert!(matches!(
        store.insert(second.clone()).unwrap_err(),
        Error::DuplicateLink { .. }
    ));
    assert_eq!(
        store.find(&tenant(1), &tenant(2)).unwrap().unwrap().id,
        first.id
    );
    assert_eq!(
        store.find(&tenant(2), &tenant(1)).unwrap().unwrap().id,
        first.id
    );
    store
        .update(
            first
                .revoked_by(&first.local, RevokeReason::Superseded, 5_000)
                .unwrap(),
            1,
        )
        .unwrap();
    assert!(store.find(&tenant(1), &tenant(2)).unwrap().is_none());
    store.insert(second.clone()).unwrap();
    let ids: Vec<_> = store
        .list_for(&tenant(2))
        .unwrap()
        .iter()
        .map(|l| l.id)
        .collect();
    let mut expected = vec![first.id, second.id];
    expected.sort();
    assert_eq!(ids, expected);
}

#[test]
fn links_and_nonces_survive_a_restart() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let store = e.store(1);
    let handshake = linked();
    let (accept, key_back) = (&handshake.messages[1], &handshake.messages[3]);
    let keys = MemoryPeerKeyDirectory::new();
    let stored = store.insert(handshake.a.clone()).unwrap();
    rt.block_on(store.claim_inbound(accept, &keys, &Trust::PinnedOnly, 2_000))
        .unwrap();
    drop(store);
    // A new pool and store, as after a process restart.
    let again = e.restarted_store(1);
    assert_eq!(again.get(&stored.id).unwrap(), stored);
    let replay = rt.block_on(again.claim_inbound(accept, &keys, &Trust::PinnedOnly, 2_100));
    assert!(matches!(replay.unwrap_err(), Error::Replayed { .. }));
    rt.block_on(again.claim_inbound(key_back, &keys, &Trust::PinnedOnly, 2_100))
        .unwrap();
    assert_eq!(rt.block_on(again.nonce_count(&stored.id)).unwrap(), 2);
}

#[test]
fn a_missing_link_and_a_store_without_tables_fail_closed() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let store = e.store(1);
    assert!(matches!(
        store.get(&link_id(5)).unwrap_err(),
        Error::NotFound { .. }
    ));
    // Tenant 3's schema was never provisioned: every call is a store failure, not an empty list.
    let unprovisioned = e.restarted_store(3);
    assert!(matches!(
        unprovisioned.list_for(&tenant(3)).unwrap_err(),
        Error::StoreFailed { .. }
    ));
    assert!(matches!(
        unprovisioned.get(&link_id(5)).unwrap_err(),
        Error::StoreFailed { .. }
    ));
}

#[test]
fn concurrent_revokers_on_one_version_have_exactly_one_winner() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let store = std::sync::Arc::new(e.store(1));
    let stored = store.insert(linked().a).unwrap();
    let outcomes: Vec<_> = (0..4u64)
        .map(|n| {
            let store = std::sync::Arc::clone(&store);
            let next = stored
                .revoked_by(&stored.local, RevokeReason::TermsEnded, 5_000 + n)
                .unwrap();
            std::thread::spawn(move || store.update(next, 1))
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();
    let wins = outcomes.iter().filter(|r| r.is_ok()).count();
    assert_eq!(wins, 1, "{outcomes:?}");
    assert!(
        outcomes
            .iter()
            .filter_map(|r| r.as_ref().err())
            .all(|error| matches!(
                error,
                Error::VersionConflict { .. } | Error::InvalidTransition { .. }
            ))
    );
    assert_eq!(store.get(&stored.id).unwrap().version, 2);
}
