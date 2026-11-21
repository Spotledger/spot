//! Revocation visibility: the next read after a revoke sees it, through the cache too, and a
//! read racing an invalidation never leaves a stale active link behind.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{Chain, consent, env, linked, runtime, tenant_id, topic};
use spot_iam_access::StepUpPolicy;
use spot_permissions::Principal;
use spot_tenant_link::{
    Direction, Error, LinkService, LinkState, LinkStore, PolicyStepUp, RevokeReason,
};
use spot_tenant_link_pg::{
    CacheHandle, CachedLinkStore, Caller, LinkAdmin, LinkInvalidation, LinkPermissions, Operation,
    PgLinkStore,
};

struct AllowAll;
impl LinkPermissions for AllowAll {
    fn may(&self, _: &Principal, _: Operation, _: Option<&spot_tenant_link::LinkId>) -> bool {
        true
    }
}

#[test]
fn a_revocation_is_visible_to_the_next_read_and_the_next_use() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let store = e.store(1);
    let link = store.insert(linked().a).unwrap();
    let chain = Chain::default();
    let service = LinkService::new(&store, &chain);
    // Active: the topic is allowed now.
    service
        .authorize(
            &link.id,
            &topic("bpm.message"),
            Direction::Send,
            10_000,
            "user:svc",
            None,
        )
        .unwrap();

    let started = Instant::now();
    service
        .revoke(
            &link.id,
            RevokeReason::SecurityConcern,
            "user:admin",
            10_001,
        )
        .unwrap();
    // No waiting, no cache to expire: the very next call is refused.
    let refused = service
        .authorize(
            &link.id,
            &topic("bpm.message"),
            Direction::Send,
            10_002,
            "user:svc",
            None,
        )
        .unwrap_err();
    assert!(matches!(refused, Error::Revoked { .. }));
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "revoke to refusal took {:?}",
        started.elapsed()
    );
    assert_eq!(store.get(&link.id).unwrap().state, LinkState::Revoked);
}

#[test]
fn a_cached_reader_drops_the_link_the_moment_the_store_commits_a_revocation() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    e.provision_schema(1);
    let handle = CacheHandle::new();
    let hook: Arc<dyn LinkInvalidation> = Arc::new(handle.clone());
    let pg =
        PgLinkStore::new(e.pool.clone(), Arc::clone(&e.rt), tenant_id(1)).with_invalidation(hook);
    rt.block_on(pg.prepare()).unwrap();
    let cached = CachedLinkStore::new(pg, handle.clone());
    let link = cached.insert(linked().a).unwrap();

    assert_eq!(cached.get(&link.id).unwrap().state, LinkState::Active);
    assert_eq!(handle.len(), 1);

    // A revocation written through the *inner* store (another code path on this node) still
    // reaches the cache, because the store publishes after its own commit.
    let current = cached.inner().get(&link.id).unwrap();
    cached
        .inner()
        .update(
            current
                .revoked_by(&current.local, RevokeReason::TermsEnded, 10_000)
                .unwrap(),
            1,
        )
        .unwrap();
    assert_eq!(handle.len(), 0);
    assert_eq!(cached.get(&link.id).unwrap().state, LinkState::Revoked);
}

#[test]
fn a_push_invalidation_from_another_node_clears_a_cached_active_link() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let store = e.store(1);
    let handle = CacheHandle::new();
    let cached = CachedLinkStore::new(store, handle.clone());
    let link = cached.insert(linked().a).unwrap();
    cached.get(&link.id).unwrap();
    // Another node revoked it directly in the database; this node's cache is stale until the
    // bus notice arrives.
    let other_node = e.restarted_store(1);
    let current = other_node.get(&link.id).unwrap();
    other_node
        .update(
            current
                .revoked_by(&current.local, RevokeReason::TermsEnded, 10_000)
                .unwrap(),
            1,
        )
        .unwrap();
    assert_eq!(
        cached.get(&link.id).unwrap().state,
        LinkState::Active,
        "stale until pushed"
    );
    handle.invalidate(&link.id);
    assert_eq!(cached.get(&link.id).unwrap().state, LinkState::Revoked);
    // A reconnect gap is handled by dropping everything.
    handle.invalidate_all();
    assert!(handle.is_empty());
}

#[test]
fn the_admin_revoke_is_effective_on_return() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let store = e.store(1);
    let link = store.insert(linked().a).unwrap();
    let chain = Chain::default();
    let gate = PolicyStepUp::new(StepUpPolicy::default());
    let admin = LinkAdmin::new(&store, &chain, &gate, &AllowAll);
    let caller = Caller::new(Principal::with_roles("admin", &["Link Admin"]));
    let revoked = admin
        .revoke(&caller, &link.id, RevokeReason::TermsEnded, 10_000)
        .unwrap();
    assert_eq!(revoked.state, LinkState::Revoked);
    assert_eq!(revoked.version, 2);
    assert_eq!(consent_free(&store, &link.id), LinkState::Revoked);
    // A second revoke is an invalid transition, audited as a failure by the link layer.
    assert!(
        admin
            .revoke(&caller, &link.id, RevokeReason::TermsEnded, 10_001)
            .is_err()
    );
}

fn consent_free(store: &PgLinkStore, id: &spot_tenant_link::LinkId) -> LinkState {
    let _ = consent;
    store.get(id).unwrap().state
}
