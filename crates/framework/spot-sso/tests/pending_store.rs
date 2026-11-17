//! The pending-authorisation store in the sign-in flow: begin stores, finish takes once, and a
//! store that cannot answer refuses the sign-in (WP-OSS-06.T3).
//!
//! The same cases run against PostgreSQL in `spot-sso-pg` (`tests/pending.rs`); here they run
//! against the in-memory store so the fail-closed handling of a caller is checked without a
//! database.

mod support;

use std::sync::Arc;
use std::thread;

use spot_sso::contract::block_on;
use spot_sso::contract_stores::FailingPendingStore;
use spot_sso::{
    CallbackParams, Jwa, MemoryPendingStore, PENDING_TTL_SECONDS, PendingAuth, PendingStore,
    SsoError, begin, callback, codes, complete,
};

use support::*;

const NOW: i64 = 1_700_000_000;

/// A finished sign-in as a host would write it: take the entry (once), then verify the callback.
/// A store error is a refusal, never a pass.
fn finish(
    store: &impl PendingStore,
    idp: &TestIdp,
    binding: &str,
    state: &str,
    now: i64,
) -> Result<PendingAuth, SsoError> {
    let taken = block_on(store.take(binding, now)).map_err(SsoError::from)?;
    let Some(pending) = taken else {
        return Err(SsoError::new(
            codes::STATE_EXPIRED,
            "This sign-in attempt is not known.",
            "Start the sign-in again from the beginning.",
        ));
    };
    let params = CallbackParams {
        state: Some(state.to_owned()),
        code: Some("the-code".to_owned()),
        error: None,
        error_description: None,
    };
    callback(&idp.config(), &idp.discovery(), &pending, &params, now)?;
    Ok(pending)
}

fn started(store: &impl PendingStore, idp: &TestIdp, binding: &str, seed: u8) -> PendingAuth {
    let (_, pending) = ok(
        begin(
            &idp.config(),
            &idp.discovery(),
            &rng_bytes(seed),
            NOW,
            "/app/invoices",
        ),
        "begin",
    );
    ok(block_on(store.put(binding, &pending)), "put");
    pending
}

#[test]
fn a_sign_in_survives_a_rebuilt_store_and_completes() {
    let idp = TestIdp::new();
    let backend = MemoryPendingStore::new();
    let pending = started(&backend, &idp, "binding-1", 1);
    // The handle is rebuilt over the same backend (the PostgreSQL case rebuilds it over the
    // same database in `spot-sso-pg`).
    let rebuilt = backend.for_tenant("default");

    let finished = ok(
        finish(&rebuilt, &idp, "binding-1", &pending.state, NOW + 5),
        "finish",
    );
    let token = idp.id_token(Jwa::Rs256, &finished.nonce, NOW, |_| {});
    let identity = ok(
        complete(
            &idp.config(),
            &idp.discovery(),
            &keys(&idp),
            &finished,
            &token_response(&token),
            NOW + 5,
        ),
        "complete",
    );
    assert_eq!(identity.subject, "subject-1");
}

#[test]
fn a_second_finish_with_the_same_state_is_refused() {
    let idp = TestIdp::new();
    let store = MemoryPendingStore::new();
    let pending = started(&store, &idp, "binding-2", 2);
    ok(
        finish(&store, &idp, "binding-2", &pending.state, NOW + 1),
        "first finish",
    );
    let second = finish(&store, &idp, "binding-2", &pending.state, NOW + 2);
    assert_eq!(second.map_err(|e| e.code()), Err(codes::STATE_EXPIRED));
}

#[test]
fn a_tampered_state_is_refused_and_the_entry_is_spent() {
    let idp = TestIdp::new();
    let store = MemoryPendingStore::new();
    started(&store, &idp, "binding-3", 3);
    let refused = finish(&store, &idp, "binding-3", "not-the-state", NOW + 1);
    assert_eq!(refused.map_err(|e| e.code()), Err(codes::STATE_MISMATCH));
    // The entry was taken, so a retry of the genuine state cannot succeed either.
    assert!(block_on(store.take("binding-3", NOW + 2)).is_ok_and(|taken| taken.is_none()));
}

#[test]
fn two_concurrent_finishes_give_exactly_one_success() {
    let idp = Arc::new(TestIdp::new());
    let store = Arc::new(MemoryPendingStore::new());
    let pending = started(&*store, &idp, "binding-4", 4);
    let handles: Vec<_> = (0..16)
        .map(|_| {
            let (store, idp, state) = (Arc::clone(&store), Arc::clone(&idp), pending.state.clone());
            thread::spawn(move || finish(&*store, &idp, "binding-4", &state, NOW + 1).is_ok())
        })
        .collect();
    let successes = handles
        .into_iter()
        .map(|handle| handle.join().is_ok_and(|won| won))
        .filter(|won| *won)
        .count();
    assert_eq!(successes, 1);
}

#[test]
fn an_expired_entry_is_refused() {
    let idp = TestIdp::new();
    let store = MemoryPendingStore::new();
    let pending = started(&store, &idp, "binding-5", 5);
    let late = NOW + PENDING_TTL_SECONDS + 1;
    let refused = finish(&store, &idp, "binding-5", &pending.state, late);
    assert_eq!(refused.map_err(|e| e.code()), Err(codes::STATE_EXPIRED));
}

#[test]
fn a_store_error_refuses_the_sign_in_with_the_stable_code() {
    let idp = TestIdp::new();
    let refused = finish(&FailingPendingStore, &idp, "binding-6", "any", NOW);
    assert_eq!(
        refused.map_err(|e| e.code()),
        Err(codes::PENDING_STORE_UNAVAILABLE)
    );
    // Starting a sign-in is refused too: nothing is stored, so nothing can be finished.
    let (_, pending) = ok(
        begin(&idp.config(), &idp.discovery(), &rng_bytes(6), NOW, "/app"),
        "begin",
    );
    assert!(block_on(FailingPendingStore.put("binding-6", &pending)).is_err());
}

#[test]
fn the_refusal_never_carries_the_secrets() {
    let idp = TestIdp::new();
    let pending = {
        let store = MemoryPendingStore::new();
        started(&store, &idp, "binding-7", 7)
    };
    let refused = finish(&FailingPendingStore, &idp, "binding-7", &pending.state, NOW);
    let rendered = format!("{refused:?}");
    assert!(!rendered.contains(&pending.state));
    assert!(!rendered.contains(&pending.nonce));
    assert!(!rendered.contains(&pending.code_verifier));
}
