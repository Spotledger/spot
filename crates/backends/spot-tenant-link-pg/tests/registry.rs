//! The control-plane registry: it holds that a link exists and nothing else, stays consistent
//! across both parties, and catches up when the control database was unreachable.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::Arc;

use common::{env, linked, runtime, tenant, tenant_id};
use spot_tenant_link::{Error, LinkStore, RevokeReason};
use spot_tenant_link_pg::{
    PgLinkRegistry, PgLinkStore, REGISTRY_COLUMNS, RegistryEntry, RegistryState,
};

#[test]
fn the_registry_has_no_grant_topic_field_key_or_consent_column() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let _registry = e.registry();
    let columns = e.columns("spot_control", "link_registry");
    assert_eq!(columns, REGISTRY_COLUMNS.to_vec());
    assert_eq!(columns, vec!["tenant_a", "tenant_b", "link_id", "state"]);
}

#[test]
fn a_stored_link_leaves_only_its_existence_and_parties_in_the_registry() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let registry = e.registry();
    e.provision_schema(1);
    let store = PgLinkStore::new(e.pool.clone(), Arc::clone(&e.rt), tenant_id(1))
        .with_registry(Arc::clone(&registry));
    rt.block_on(store.prepare()).unwrap();
    let link = linked().a;
    store.insert(link.clone()).unwrap();

    let entry = rt.block_on(registry.get(&link.id)).unwrap();
    assert_eq!(entry, RegistryEntry::of(&link));
    assert_eq!(entry.state, RegistryState::Live);
    // The raw row, as text, names no topic and no field.
    let dump: String = rt.block_on(async {
        let client = e.db.raw_client().await;
        client
            .query_one("SELECT l::text FROM spot_control.link_registry l", &[])
            .await
            .unwrap()
            .get(0)
    });
    assert!(!dump.contains("bpm.message"), "{dump}");
    assert!(!dump.contains("message.body"), "{dump}");
    assert!(dump.contains("tenant:"), "{dump}");
    assert!(rt.block_on(store.registry_pending()).unwrap().is_empty());
}

#[test]
fn both_parties_share_one_row_and_ended_is_sticky() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let registry = e.registry();
    let both = linked();
    for n in [1u8, 2] {
        e.provision_schema(n);
    }
    let store_a = PgLinkStore::new(e.pool.clone(), Arc::clone(&e.rt), tenant_id(1))
        .with_registry(Arc::clone(&registry));
    let store_b = PgLinkStore::new(e.pool.clone(), Arc::clone(&e.rt), tenant_id(2))
        .with_registry(Arc::clone(&registry));
    rt.block_on(store_a.prepare()).unwrap();
    rt.block_on(store_b.prepare()).unwrap();
    store_a.insert(both.a.clone()).unwrap();
    store_b.insert(both.b.clone()).unwrap();
    assert_eq!(
        rt.block_on(registry.entries_for(&tenant(1))).unwrap().len(),
        1
    );
    assert_eq!(
        rt.block_on(registry.entries_for(&tenant(2))).unwrap().len(),
        1
    );

    // A revokes; B's later, stale write of "live" must not resurrect the entry.
    let a = store_a.get(&both.a.id).unwrap();
    store_a
        .update(
            a.revoked_by(&a.local, RevokeReason::SecurityConcern, 9_000)
                .unwrap(),
            1,
        )
        .unwrap();
    assert_eq!(
        rt.block_on(registry.get(&both.a.id)).unwrap().state,
        RegistryState::Ended
    );
    rt.block_on(registry.upsert(&RegistryEntry::of(&both.b)))
        .unwrap();
    assert_eq!(
        rt.block_on(registry.get(&both.a.id)).unwrap().state,
        RegistryState::Ended
    );
}

#[test]
fn a_registered_link_keeps_its_two_parties() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let registry = e.registry();
    let link = linked().a;
    rt.block_on(registry.upsert(&RegistryEntry::of(&link)))
        .unwrap();
    let mut hijack = RegistryEntry::of(&link);
    hijack.tenant_b = tenant(9);
    assert!(matches!(
        rt.block_on(registry.upsert(&hijack)).unwrap_err(),
        Error::InvalidParty { .. }
    ));
    assert_eq!(
        rt.block_on(registry.get(&link.id)).unwrap().tenant_b,
        link.parties[1]
    );
}

#[test]
fn a_change_made_while_the_control_database_was_down_is_finished_by_reconcile() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    // The registry table does not exist yet: the control database "is down" for the store.
    let registry = Arc::new(PgLinkRegistry::new(e.pool.clone()));
    e.provision_schema(1);
    let store = PgLinkStore::new(e.pool.clone(), Arc::clone(&e.rt), tenant_id(1))
        .with_registry(Arc::clone(&registry));
    rt.block_on(store.prepare()).unwrap();
    let link = linked().a;
    // The insert still commits: the tenant's own row is what gates exchange.
    store.insert(link.clone()).unwrap();
    assert_eq!(
        rt.block_on(store.registry_pending()).unwrap(),
        vec![link.id]
    );
    assert!(matches!(
        rt.block_on(registry.get(&link.id)).unwrap_err(),
        Error::StoreFailed { .. }
    ));
    // The control database comes back; reconcile writes the entry and clears the mark.
    rt.block_on(registry.prepare()).unwrap();
    assert_eq!(rt.block_on(store.reconcile_registry()).unwrap(), 1);
    assert!(rt.block_on(store.registry_pending()).unwrap().is_empty());
    assert_eq!(
        rt.block_on(registry.get(&link.id)).unwrap(),
        RegistryEntry::of(&link)
    );
}
