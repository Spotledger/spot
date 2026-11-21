//! The nonce-store contract suite (feature `testing`).
//!
//! Every [`NonceStore`] implementation runs these checks against a fresh, empty store.
//! They are plain `async fn`s over the port, so they need no executor of their own: the
//! store crate's test drives them on its own runtime. The concurrency check is split in
//! two so the caller spawns the racing claims on whatever runtime it has:
//! [`claim_args`] gives the one shared `(key, nonce)` and [`assert_one_winner`] judges
//! the collected results.
//!
//! The suite never prints or compares nonce bytes in a message.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use crate::command::{Command, KeyId};
use crate::envelope::{CommandVerifier, Signer, SoftwareSigner, TrustedKeys, VerifyPolicy, sign};
use crate::error::{CommandError, StoreError};
use crate::nonce::{NonceFuture, NonceStore};

/// The skew the suite assumes the store under test was built with.
pub const SUITE_SKEW: i64 = 60;

/// A key id for suite claims, distinct per `seed`.
#[must_use]
pub fn suite_key(seed: u8) -> KeyId {
    KeyId::from_public_key(&[seed; 32])
}

/// The `(key, nonce, expires_at, now)` every racing claim of the concurrency check uses.
#[must_use]
pub fn claim_args() -> (KeyId, [u8; 16], i64, i64) {
    (suite_key(9), [0xC1; 16], 2_000, 1_500)
}

/// Judges the results of concurrent claims of one `(key, nonce)`: exactly one `Ok(true)`,
/// every other result `Ok(false)`, never an error.
///
/// # Panics
/// When the results are not exactly one winner and the rest replays.
pub fn assert_one_winner(results: &[Result<bool, StoreError>]) {
    let winners = results.iter().filter(|r| matches!(r, Ok(true))).count();
    let losers = results.iter().filter(|r| matches!(r, Ok(false))).count();
    assert_eq!(winners, 1, "exactly one concurrent claim must win");
    assert_eq!(
        winners + losers,
        results.len(),
        "every other claim must be a replay, not an error"
    );
}

/// Fresh, then replayed; a different nonce and a different key are independent.
///
/// # Panics
/// When the store breaks the contract.
pub async fn fresh_then_replayed<S: NonceStore + ?Sized>(store: &S) {
    let key = suite_key(1);
    let other = suite_key(2);
    assert_eq!(
        store.check_and_insert(&key, [1; 16], 2_000, 1_500).await,
        Ok(true)
    );
    assert_eq!(
        store.check_and_insert(&key, [1; 16], 2_000, 1_500).await,
        Ok(false)
    );
    assert_eq!(
        store.check_and_insert(&key, [2; 16], 2_000, 1_500).await,
        Ok(true)
    );
    assert_eq!(
        store.check_and_insert(&other, [1; 16], 2_000, 1_500).await,
        Ok(true)
    );
}

/// A pair is live until `expires_at + skew`, and may be claimed again strictly after.
///
/// # Panics
/// When the store breaks the contract.
pub async fn reintroduced_after_skew<S: NonceStore + ?Sized>(store: &S) {
    let key = suite_key(3);
    assert_eq!(
        store.check_and_insert(&key, [7; 16], 1_000, 900).await,
        Ok(true)
    );
    // At the end of the skew window the entry is still live.
    let edge = 1_000 + SUITE_SKEW;
    assert_eq!(
        store.check_and_insert(&key, [7; 16], 1_000, edge).await,
        Ok(false)
    );
    // Strictly past it, the pair is fresh again.
    assert_eq!(
        store.check_and_insert(&key, [7; 16], 5_000, edge + 1).await,
        Ok(true)
    );
    assert_eq!(
        store.check_and_insert(&key, [7; 16], 5_000, edge + 2).await,
        Ok(false)
    );
}

/// A store that always fails with [`StoreError::Unavailable`].
#[derive(Debug, Default, Clone, Copy)]
pub struct FailingStore;

impl NonceStore for FailingStore {
    fn check_and_insert<'a>(
        &'a self,
        _key: &'a KeyId,
        _nonce: [u8; 16],
        _expires_at: i64,
        _now: i64,
    ) -> NonceFuture<'a, Result<bool, StoreError>> {
        Box::pin(std::future::ready(Err(StoreError::Unavailable)))
    }
}

/// A verifier whose store fails refuses a fully valid command with `cmd.nonce_store`;
/// it never accepts it.
///
/// # Panics
/// When the verifier accepts the command or answers with another code.
pub async fn failing_store_refuses() {
    let signer = SoftwareSigner::from_seed(&[5u8; 32]).expect("seed");
    let mut trusted = TrustedKeys::new();
    trusted
        .insert(signer.key_id(), &signer.public_key_bytes())
        .expect("key");
    let command = Command::new(
        concat!("server", ".", "service", ".", "restart"),
        serde_json::json!({"service": "x"}),
        "agent-1",
        signer.key_id(),
        2_000,
        2_500,
    );
    let signed = sign(&command, &signer).expect("sign");
    let verifier = CommandVerifier::new(
        trusted,
        VerifyPolicy::default_policy(),
        Box::new(FailingStore),
    );
    let result = verifier.verify(&signed, "agent-1", 2_100).await;
    match result {
        Err(error @ CommandError::Store(_)) => assert_eq!(
            error.code(),
            CommandError::Store(StoreError::Unavailable).code()
        ),
        Err(other) => panic!("wrong refusal: {}", other.code()),
        Ok(_) => panic!("a failing store must never accept a command"),
    }
}

/// Runs every sequential check of the suite, each against `fresh()`'s empty store.
///
/// # Panics
/// When any check fails.
pub async fn nonce_contract<S: NonceStore>(fresh: impl Fn() -> S) {
    fresh_then_replayed(&fresh()).await;
    reintroduced_after_skew(&fresh()).await;
    failing_store_refuses().await;
}
