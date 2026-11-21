//! Tenant isolation: a tenant's store sees only its own schema's rows, refuses another tenant's
//! links on the way in and out, and the control registry is not a way around it.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{env, linked, runtime, tenant};
use spot_tenant_link::{Error, LinkStore};

#[test]
fn tenant_b_cannot_read_tenant_as_link_rows() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let store_a = e.store(1);
    let store_b = e.store(2);
    let link = linked().a;
    store_a.insert(link.clone()).unwrap();

    // Through the store: B asks for A's id, A's pair and A's party and finds nothing.
    assert!(matches!(
        store_b.get(&link.id).unwrap_err(),
        Error::NotFound { .. }
    ));
    assert!(store_b.find(&tenant(1), &tenant(2)).unwrap().is_none());
    assert!(store_b.list_for(&tenant(1)).unwrap().is_empty());
    assert!(store_b.list_for(&tenant(2)).unwrap().is_empty());

    // Underneath: B's schema has no rows in any link table, A's has them.
    for table in [
        "spot_tenant_link",
        "spot_tenant_link_peer_key",
        "spot_tenant_link_consent",
        "spot_tenant_link_grant",
    ] {
        assert_eq!(e.raw_count(2, table), 0, "tenant B sees rows of {table}");
        assert!(e.raw_count(1, table) > 0, "tenant A lost rows of {table}");
    }

    // B cannot write through A's link either: an update of A's id finds nothing in B's schema.
    let mut changed = link.clone();
    changed.version = 1;
    assert!(matches!(
        store_b.update(changed, 1).unwrap_err(),
        Error::InvalidParty { .. } | Error::NotFound { .. }
    ));
}

#[test]
fn a_store_refuses_a_link_it_is_not_a_party_of() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let store_b = e.store(2);
    // Tenant 1's copy cannot be stored in tenant 2's schema.
    assert!(matches!(
        store_b.insert(linked().a).unwrap_err(),
        Error::InvalidParty { .. }
    ));
    assert_eq!(e.raw_count(2, "spot_tenant_link"), 0);
}

#[test]
fn each_tenant_keeps_its_own_copy_under_the_same_link_id() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let store_a = e.store(1);
    let store_b = e.store(2);
    let both = linked();
    store_a.insert(both.a.clone()).unwrap();
    store_b.insert(both.b.clone()).unwrap();
    assert_eq!(store_a.get(&both.a.id).unwrap().local, tenant(1));
    assert_eq!(store_b.get(&both.b.id).unwrap().local, tenant(2));
    // Revoking in one schema leaves the other copy exactly as it was.
    let a = store_a.get(&both.a.id).unwrap();
    let revoked = a
        .revoked_by(&a.local, spot_tenant_link::RevokeReason::TermsEnded, 9_000)
        .unwrap();
    store_a.update(revoked, 1).unwrap();
    assert_eq!(
        store_b.get(&both.b.id).unwrap().state,
        spot_tenant_link::LinkState::Active
    );
}

#[test]
fn a_lookup_by_an_unknown_party_matches_nothing_and_changes_nothing() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let store = e.store(1);
    store.insert(linked().a).unwrap();
    // Party text is a bound parameter and the id types refuse quotes and semicolons anyway;
    // an unknown party matches nothing and breaks nothing.
    assert!(spot_tenant_link::ExternalPeerId::new("x'; DROP TABLE spot_tenant_link; --").is_err());
    let hostile = spot_tenant_link::Party::External(
        spot_tenant_link::ExternalPeerId::new("unknown-peer").unwrap(),
    );
    assert!(store.list_for(&hostile).unwrap().is_empty());
    assert!(store.find(&hostile, &tenant(1)).unwrap().is_none());
    assert_eq!(e.raw_count(1, "spot_tenant_link"), 1);
}
