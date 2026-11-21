//! Nonce-store behaviour (brief §Tests): the cap refuses rather than evicts, expired
//! entries are cleaned, and 8 threads racing the same nonce produce exactly one winner.
//! Also the `Debug` guarantees: no key bytes anywhere.
//!
//! House pattern: lint-scoped integration tests may `unwrap` what the test itself
//! built.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::thread;

use ed25519_dalek::SigningKey;
use spot_signed_command::{
    Command, CommandError, CommandVerifier, DEFAULT_NONCE_CAP, KeyId, MemoryNonceStore, NonceStore,
    Signer, StoreError, TrustedKeys, VerifyPolicy, sign,
};

/// Runs the async verifier to completion (the memory store's future is always ready).
trait VerifySync {
    fn verify_sync(
        &self,
        signed: &spot_signed_command::SignedCommand,
        audience: &str,
        now: i64,
    ) -> Result<spot_signed_command::Verified, CommandError>;
}

impl VerifySync for CommandVerifier {
    fn verify_sync(
        &self,
        signed: &spot_signed_command::SignedCommand,
        audience: &str,
        now: i64,
    ) -> Result<spot_signed_command::Verified, CommandError> {
        futures::executor::block_on(self.verify(signed, audience, now))
    }
}

/// Runs the async port to completion (the memory store's future is always ready).
trait ClaimSync {
    fn claim_sync(
        &self,
        key: &KeyId,
        nonce: [u8; 16],
        expires_at: i64,
        now: i64,
    ) -> Result<bool, StoreError>;
}

impl<T: NonceStore> ClaimSync for T {
    fn claim_sync(
        &self,
        key: &KeyId,
        nonce: [u8; 16],
        expires_at: i64,
        now: i64,
    ) -> Result<bool, StoreError> {
        futures::executor::block_on(self.check_and_insert(key, nonce, expires_at, now))
    }
}

/// A tiny cap for cap tests, so the test builds only a few entries.
const TEST_CAP: usize = 4;
/// The same cap as the u32 the replay loop counts with.
const TEST_CAP_COUNT: u32 = TEST_CAP as u32;

/// A store with a small cap and no skew, for the cap behaviour.
fn capped_store() -> MemoryNonceStore {
    MemoryNonceStore::with_limits(0, TEST_CAP)
}

/// A fresh command nonce, distinct per call.
fn distinct_nonce(counter: &mut u32) -> [u8; 16] {
    *counter += 1;
    let mut nonce = [0u8; 16];
    nonce[..4].copy_from_slice(&counter.to_be_bytes());
    nonce
}

/// The cap refuses the entry that would overflow it, typed, and keeps every live
/// entry: nothing is evicted.
#[test]
fn cap_refuses_instead_of_evicting() {
    let store = capped_store();
    let key = KeyId::from_public_key(&SigningKey::from_bytes(&[1; 32]).verifying_key().to_bytes());
    let mut counter = 0u32;
    // Fill to the cap.
    for _ in 0..TEST_CAP {
        let nonce = distinct_nonce(&mut counter);
        assert!(store.claim_sync(&key, nonce, 1_900, 1_000).unwrap());
    }
    assert_eq!(store.len().unwrap(), TEST_CAP);
    // One more is refused, typed as StoreError::Full.
    let error = store
        .claim_sync(&key, distinct_nonce(&mut counter), 1_900, 1_000)
        .unwrap_err();
    assert!(matches!(error, StoreError::Full));
    // The refusal evicted nothing: all four live entries are still replayed.
    for replay_nonce in 1..=TEST_CAP_COUNT {
        let mut nonce = [0u8; 16];
        let be = replay_nonce.to_be_bytes();
        nonce[..be.len()].copy_from_slice(&be);
        assert!(!store.claim_sync(&key, nonce, 1_900, 1_000).unwrap());
    }
    assert_eq!(store.len().unwrap(), TEST_CAP);
}

/// Entries are dropped once they are past their `expires_at + skew`.
#[test]
fn expired_entries_are_cleaned() {
    let store = MemoryNonceStore::with_limits(60, DEFAULT_NONCE_CAP);
    let key = KeyId::from_public_key(&SigningKey::from_bytes(&[2; 32]).verifying_key().to_bytes());
    // One entry expiring at 1_000 (dropable at 1_061), one at 2_000 (live).
    assert!(store.claim_sync(&key, [1; 16], 1_000, 1_000).unwrap());
    assert!(store.claim_sync(&key, [2; 16], 2_000, 1_000).unwrap());
    assert_eq!(store.len().unwrap(), 2);
    // Before the horizon nothing is dropped.
    assert_eq!(store.sweep(1_060).unwrap(), 0);
    assert_eq!(store.len().unwrap(), 2);
    // After expires_at + skew the entry is gone, and its slot is reusable.
    assert_eq!(store.sweep(1_070).unwrap(), 1);
    assert_eq!(store.len().unwrap(), 1);
    assert!(store.claim_sync(&key, [1; 16], 1_000, 1_070).unwrap());
    assert_eq!(store.len().unwrap(), 2);
    // The same happens implicitly inside check_and_insert: a full store frees the
    // expired entries instead of refusing.
    let small = capped_store();
    let mut counter = 0u32;
    for _ in 0..TEST_CAP {
        assert!(
            small
                .claim_sync(&key, distinct_nonce(&mut counter), 1_000, 1_000)
                .unwrap()
        );
    }
    // All expire at 1_000 + skew 0: at now = 1_001 they are past their horizon.
    assert!(
        small
            .claim_sync(&key, distinct_nonce(&mut counter), 1_000, 1_001)
            .unwrap()
    );
    assert_eq!(small.len().unwrap(), 1);
}

/// 8 threads race the same (issuer, nonce); exactly one call wins, the others see the
/// replay.
#[test]
fn eight_threads_race_one_nonce_exactly_one_wins() {
    let store = Arc::new(MemoryNonceStore::new());
    let key = KeyId::from_public_key(&SigningKey::from_bytes(&[3; 32]).verifying_key().to_bytes());
    let mut handles = Vec::new();
    for _ in 0..8 {
        let store = Arc::clone(&store);
        let key = key.clone();
        handles.push(thread::spawn(move || {
            store.claim_sync(&key, [0x5A; 16], 1_900, 1_000)
        }));
    }
    let results: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap().unwrap())
        .collect();
    let wins = results.iter().filter(|accepted| **accepted).count();
    assert_eq!(wins, 1, "exactly one racer wins: {results:?}");
    assert_eq!(results.iter().filter(|accepted| !**accepted).count(), 7);
    assert_eq!(store.len().unwrap(), 1);
}

/// Distinct nonces race freely: all 8 are accepted.
#[test]
fn eight_threads_distinct_nonces_all_accepted() {
    let store = Arc::new(MemoryNonceStore::new());
    let key = KeyId::from_public_key(&SigningKey::from_bytes(&[4; 32]).verifying_key().to_bytes());
    let mut handles = Vec::new();
    for thread_index in 0..8u8 {
        let store = Arc::clone(&store);
        let key = key.clone();
        handles.push(thread::spawn(move || {
            let mut nonce = [0u8; 16];
            nonce[0] = thread_index;
            store.claim_sync(&key, nonce, 1_900, 1_000)
        }));
    }
    let results: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap().unwrap())
        .collect();
    assert_eq!(results.iter().filter(|accepted| **accepted).count(), 8);
    assert_eq!(store.len().unwrap(), 8);
}

/// A store failure refuses the command with the store's own error: the verifier maps
/// it to `cmd.nonce_store`, and the command's nonce is not burned anywhere visible.
#[test]
fn store_failure_refuses_the_command() {
    // A full store (cap 1, one live entry) behind an otherwise passing verification.
    let signer = spot_signed_command::SoftwareSigner::from_key(SigningKey::from_bytes(&[5; 32]));
    let mut trusted = TrustedKeys::new();
    trusted
        .insert(signer.key_id(), &signer.public_key_bytes())
        .unwrap();
    let command = Command::new(
        "noop",
        serde_json::json!({}),
        "agent-node-7",
        signer.key_id(),
        1_000,
        1_900,
    );
    let envelope = sign(&command, &signer).unwrap();
    // Burn the store's single slot with a foreign issuer key/nonce first.
    let foreign =
        KeyId::from_public_key(&SigningKey::from_bytes(&[6; 32]).verifying_key().to_bytes());
    let full = MemoryNonceStore::with_limits(60, 1);
    assert!(full.claim_sync(&foreign, [9; 16], 2_900, 1_000).unwrap());
    let verifier = CommandVerifier::new(trusted, VerifyPolicy::default_policy(), Box::new(full));
    let error = verifier
        .verify_sync(&envelope, "agent-node-7", 1_500)
        .unwrap_err();
    assert_eq!(error.code(), "cmd.nonce_store");
    assert!(matches!(error, CommandError::Store(StoreError::Full)));
}

/// `Debug` for `SoftwareSigner` shows no key bytes; `MemoryNonceStore` and the
/// verifier's debug show configuration only.
#[test]
fn debug_never_shows_key_bytes() {
    let signer = spot_signed_command::SoftwareSigner::from_seed(&[7u8; 32]).unwrap();
    let text = format!("{signer:?}");
    assert!(!text.contains(&hex_prefix_of(&signer.public_key_bytes())));
    assert!(text.contains("SoftwareSigner"));
    let key_id_text = signer.key_id();
    assert!(text.contains(key_id_text.as_str()));
    // The key id is a hash of the public key, not the key: showing it is safe.
    let store = MemoryNonceStore::new();
    let store_text = format!("{store:?}");
    assert!(store_text.contains("MemoryNonceStore"));
    let verifier = CommandVerifier::new(
        TrustedKeys::new(),
        VerifyPolicy::default_policy(),
        Box::new(MemoryNonceStore::new()),
    );
    let verifier_text = format!("{verifier:?}");
    assert!(verifier_text.contains("CommandVerifier"));
}

/// The hex of the first byte of `bytes`, which any leaked key rendering would start
/// with.
fn hex_prefix_of(bytes: &[u8]) -> String {
    let mut hex = String::new();
    for byte in bytes.iter().take(4) {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// The shared contract suite passes for the memory store.
#[test]
fn the_memory_store_passes_the_contract_suite() {
    futures::executor::block_on(spot_signed_command::testing::nonce_contract(|| {
        MemoryNonceStore::with_limits(spot_signed_command::testing::SUITE_SKEW, 1_000)
    }));
}

/// The contract's concurrency check, through the suite's own judge.
#[test]
fn the_memory_store_gives_one_winner_through_the_suite_judge() {
    let store = Arc::new(MemoryNonceStore::new());
    let (key, nonce, expires_at, now) = spot_signed_command::testing::claim_args();
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let store = Arc::clone(&store);
            let key = key.clone();
            thread::spawn(move || store.claim_sync(&key, nonce, expires_at, now))
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    spot_signed_command::testing::assert_one_winner(&results);
}

/// A failing store never lets a valid command through.
#[test]
fn a_failing_store_refuses_a_valid_command() {
    futures::executor::block_on(spot_signed_command::testing::failing_store_refuses());
}
