//! The durable handshake ports (WP-CTL-09.T6): a restart does not forget a used nonce
//! or a pinned key, a database failure is never read as "new" or "unpinned", and the real
//! handshake refuses a replay across a rebuilt `Handshake`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::Arc;

use common::{Env, consent, env, link_id, linked, nonce, offer, pool, runtime, tenant, tenant_id};
use spot_iam_access::StepUpPolicy;
use spot_tenant_link::{
    Carrier, ConsentPolicy, Direction, Error, Handshake, HandshakePolicy, IdentitySigner,
    LinkDraft, LinkStore, MemoryAuditLog, NonceLog, PeerKey, PeerKeyDirectory, PolicyStepUp,
    SeedSigner, Trust,
};
use spot_tenant_link_pg::PgLinkStore;

fn key_of(seed: u8, who: &spot_tenant_link::Party) -> PeerKey {
    SeedSigner::from_seed([seed; 32], who.clone()).public_key()
}

fn closed_store(e: &Env, n: u8) -> PgLinkStore {
    let broken = pool(&e.db.config(), 2);
    broken.close();
    PgLinkStore::new(broken, Arc::clone(&e.rt), tenant_id(n))
}

#[test]
fn a_used_nonce_is_still_refused_after_a_restart() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let (log, _) = e.store(1).handshake_ports();
    assert!(log.record(&link_id(1), &nonce(7)).unwrap());
    assert!(!log.record(&link_id(1), &nonce(7)).unwrap());
    // Same nonce on another link is its own record.
    assert!(log.record(&link_id(2), &nonce(7)).unwrap());
    // A new pool and store, as after a process restart.
    let (reopened, _) = e.restarted_store(1).handshake_ports();
    assert!(!reopened.record(&link_id(1), &nonce(7)).unwrap());
    assert!(reopened.record(&link_id(1), &nonce(8)).unwrap());
}

#[test]
fn a_released_nonce_can_be_recorded_again() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let (log, _) = e.store(1).handshake_ports();
    assert!(log.record(&link_id(1), &nonce(1)).unwrap());
    log.release(&link_id(1), &nonce(1)).unwrap();
    assert!(log.record(&link_id(1), &nonce(1)).unwrap());
}

#[test]
fn two_concurrent_records_of_one_nonce_give_exactly_one_true() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let (log, _) = e.store(1).handshake_ports();
    let log = Arc::new(log);
    for round in 0..8u8 {
        let wins: usize = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..4)
                .map(|_| {
                    let log = Arc::clone(&log);
                    scope.spawn(move || log.record(&link_id(1), &nonce(round)).unwrap())
                })
                .collect();
            handles
                .into_iter()
                .map(|h| usize::from(h.join().unwrap()))
                .sum()
        });
        assert_eq!(wins, 1);
    }
}

#[test]
fn nonces_of_one_tenant_are_not_seen_by_another() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let (log_a, _) = e.store(1).handshake_ports();
    let (log_b, _) = e.store(2).handshake_ports();
    assert!(log_a.record(&link_id(1), &nonce(1)).unwrap());
    assert!(log_b.record(&link_id(1), &nonce(1)).unwrap());
}

#[test]
fn a_database_failure_makes_record_an_error_not_false() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let (log, _) = closed_store(&e, 1).handshake_ports();
    assert!(matches!(
        log.record(&link_id(1), &nonce(1)),
        Err(Error::StoreFailed { .. })
    ));
    assert!(matches!(
        log.release(&link_id(1), &nonce(1)),
        Err(Error::StoreFailed { .. })
    ));
}

#[test]
fn a_pinned_key_survives_a_restart_and_stays_per_tenant() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let store = e.store(1);
    store.insert(linked().a).unwrap();
    let (_, keys) = store.handshake_ports();
    let expected = key_of(2, &tenant(2));
    assert_eq!(keys.pinned(&tenant(2)), Some(expected));
    assert_eq!(keys.pinned_checked(&tenant(2)).unwrap(), Some(expected));
    assert_eq!(keys.pinned(&tenant(9)), None);
    assert_eq!(keys.last_error(), None);
    let (_, reopened) = e.restarted_store(1).handshake_ports();
    assert_eq!(reopened.pinned(&tenant(2)), Some(expected));
    assert_eq!(reopened.last_error(), None);
    // Another tenant's schema holds no pin.
    let (_, other) = e.store(2).handshake_ports();
    assert_eq!(other.pinned(&tenant(2)), None);
    assert_eq!(other.last_error(), None);
}

#[test]
fn a_failed_read_returns_none_and_stays_visible_until_cleared() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let (_, keys) = closed_store(&e, 1).handshake_ports();
    assert!(matches!(
        keys.pinned_checked(&tenant(2)),
        Err(Error::StoreFailed { .. })
    ));
    assert_eq!(keys.pinned(&tenant(2)), None);
    assert!(keys.last_error().is_some());
    keys.clear_error();
    assert_eq!(keys.last_error(), None);
}

#[test]
fn two_different_keys_for_one_party_are_a_failure_not_a_choice() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let store = e.store(1);
    let link = linked().a;
    let first = link.id;
    store.insert(link).unwrap();
    let second = link_id(9);
    let schema = tenant_id(1).schema_name();
    rt.block_on(async {
        let client = e.db.raw_client().await;
        client
            .batch_execute(&format!("SET search_path TO \"{schema}\""))
            .await
            .unwrap();
        // A second, expired link to the same peer, holding a different key for it.
        client
            .execute(
                "CREATE TEMP TABLE copy AS SELECT * FROM spot_tenant_link WHERE id = $1",
                &[&first.as_uuid()],
            )
            .await
            .unwrap();
        client
            .execute(
                "UPDATE copy SET id = $1, state = 'expired'",
                &[&second.as_uuid()],
            )
            .await
            .unwrap();
        client
            .batch_execute("INSERT INTO spot_tenant_link SELECT * FROM copy")
            .await
            .unwrap();
        client
            .execute(
                "INSERT INTO spot_tenant_link_peer_key (link, party, public_key) VALUES ($1, $2, $3)",
                &[
                    &second.as_uuid(),
                    &tenant(2).canonical(),
                    &key_of(77, &tenant(2)).as_bytes().as_slice(),
                ],
            )
            .await
            .unwrap();
    });
    let (_, keys) = store.handshake_ports();
    assert!(matches!(
        keys.pinned_checked(&tenant(2)),
        Err(Error::StoreFailed { .. })
    ));
    assert_eq!(keys.pinned(&tenant(2)), None);
    assert!(keys.last_error().is_some());
}

#[test]
fn the_handshake_built_on_the_pg_ports_refuses_a_replay_across_a_rebuilt_handshake() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let a = tenant(1);
    let b = tenant(2);
    let key_a = SeedSigner::from_seed([1u8; 32], a.clone());
    let key_b = SeedSigner::from_seed([2u8; 32], b.clone());
    let gate = PolicyStepUp::new(StepUpPolicy::default());
    let audit: MemoryAuditLog = MemoryAuditLog::new();

    // Tenant 1 proposes; its nonce goes to its durable log.
    let (log_a, keys_a) = e.store(1).handshake_ports();
    keys_a_check(&keys_a);
    let policy_a = HandshakePolicy {
        keys: &keys_a,
        trust: Trust::first_use(key_b.public_key()),
        consent: ConsentPolicy::default(),
        gate: &gate,
        nonces: &log_a,
    };
    let draft = LinkDraft::new(
        link_id(1),
        a.clone(),
        b.clone(),
        Carrier::Direct,
        1_000,
        None,
        offer(&a, Direction::Send),
    )
    .unwrap();
    let (_proposer, propose) = Handshake::propose(
        &key_a,
        &audit,
        &policy_a,
        draft,
        consent(&a, "user:a", 1_100),
        1_100,
        nonce(1),
    )
    .unwrap();

    // Tenant 2 receives it: first time accepted.
    let (log_b, keys_b) = e.store(2).handshake_ports();
    let policy_b = HandshakePolicy {
        keys: &keys_b,
        trust: Trust::first_use(key_a.public_key()),
        consent: ConsentPolicy::default(),
        gate: &gate,
        nonces: &log_b,
    };
    Handshake::from_proposal(&key_b, &audit, &policy_b, &propose, 1_200).unwrap();

    // The process restarts: new pool, new store, new ports, a rebuilt handshake. The same
    // captured message is refused.
    let (log_b2, keys_b2) = e.restarted_store(2).handshake_ports();
    let policy_b2 = HandshakePolicy {
        keys: &keys_b2,
        trust: Trust::first_use(key_a.public_key()),
        consent: ConsentPolicy::default(),
        gate: &gate,
        nonces: &log_b2,
    };
    let replay = Handshake::from_proposal(&key_b, &audit, &policy_b2, &propose, 1_250);
    assert!(matches!(replay, Err(Error::Replayed { .. })));

    // With an unreadable log the same message is refused too (fail closed).
    let (dead_log, dead_keys) = closed_store(&e, 2).handshake_ports();
    let policy_dead = HandshakePolicy {
        keys: &dead_keys,
        trust: Trust::first_use(key_a.public_key()),
        consent: ConsentPolicy::default(),
        gate: &gate,
        nonces: &dead_log,
    };
    assert!(Handshake::from_proposal(&key_b, &audit, &policy_dead, &propose, 1_300).is_err());
}

fn keys_a_check(keys: &dyn PeerKeyDirectory) {
    assert!(keys.pinned(&tenant(2)).is_none());
}

#[test]
fn prune_still_removes_old_nonces_the_adapter_wrote() {
    let rt = runtime();
    let Some(e) = rt.block_on(env(rt.clone())) else {
        return;
    };
    let store = e.store(1);
    let (log, _) = store.handshake_ports();
    log.record(&link_id(1), &nonce(1)).unwrap();
    assert_eq!(rt.block_on(store.nonce_count(&link_id(1))).unwrap(), 1);
    // The row carries the database clock; a far-future now removes it, a far-past one keeps it.
    let policy = ConsentPolicy::default();
    assert_eq!(rt.block_on(store.prune_nonces(0, policy)).unwrap(), 0);
    let far = u64::MAX / 4;
    assert_eq!(rt.block_on(store.prune_nonces(far, policy)).unwrap(), 1);
    assert_eq!(rt.block_on(store.nonce_count(&link_id(1))).unwrap(), 0);
}
