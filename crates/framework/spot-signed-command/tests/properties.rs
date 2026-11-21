//! Property tests (brief §Tests): random float-free JSON parameters sign and verify;
//! flipping any single byte of the signed message or of a signature breaks
//! verification. Nothing panics on random-but-structural inputs.
//!
//! House pattern: lint-scoped integration tests may `unwrap` what the test itself
//! built (cf. `spot-license` properties).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use proptest::prelude::*;

use ed25519_dalek::SigningKey;
use spot_signed_command::{
    Command, CommandError, CommandVerifier, MemoryNonceStore, Signer, SoftwareSigner, TrustedKeys,
    VerifyPolicy, sign,
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

/// The window of the generated commands: issued at 1_000, expires 15 minutes later.
const ISSUED_AT: i64 = 1_000;
const EXPIRES_AT: i64 = 1_900;
const NOW: i64 = 1_500;

/// Leaf JSON values without floats: nulls, booleans, integers, strings (plain, and one
/// packed with escapes and non-ASCII).
fn leaf_strategy() -> impl Strategy<Value = serde_json::Value> {
    prop_oneof![
        Just(()).prop_map(|_| serde_json::Value::Null),
        any::<bool>().prop_map(serde_json::Value::Bool),
        (-9_000_000_000_i64..9_000_000_000_i64).prop_map(|n| serde_json::json!(n)),
        "[^\u{0000}]{0,12}".prop_map(serde_json::Value::String),
        Just(serde_json::Value::String(
            "\u{0007}\u{001F}\\\"xé\u{1F916}".to_owned(),
        )),
    ]
}

/// A JSON value strategy without floats: nested objects, arrays, strings, integers,
/// nulls and booleans, recursive to depth 3.
fn params_strategy() -> impl Strategy<Value = serde_json::Value> {
    leaf_strategy().prop_recursive(3, 8, 4, |inner| {
        prop_oneof![
            // Objects; single-letter keys, the writer does the sorting.
            prop::collection::vec(
                (
                    prop_oneof![
                        Just("a".to_owned()),
                        Just("b".to_owned()),
                        Just("c".to_owned()),
                        Just("z".to_owned())
                    ],
                    inner.clone()
                ),
                0..4
            )
            .prop_map(|entries| serde_json::Value::Object(
                entries
                    .into_iter()
                    .collect::<serde_json::Map<String, serde_json::Value>>()
            )),
            prop::collection::vec(inner, 0..4).prop_map(serde_json::Value::Array),
        ]
    })
}

/// A deterministic signer/issuer pair.
fn signer() -> SoftwareSigner {
    SoftwareSigner::from_key(SigningKey::from_bytes(&[0x77; 32]))
}

/// A command with the given params and a fresh nonce minted inside `Command::new`.
fn command_with(params: serde_json::Value) -> Command {
    Command::new(
        "apply_config",
        params,
        "agent-node-7",
        signer().key_id(),
        ISSUED_AT,
        EXPIRES_AT,
    )
}

/// A verifier trusting the signer, fresh store per verify.
fn verifier() -> CommandVerifier {
    let mut trusted = TrustedKeys::new();
    trusted
        .insert(signer().key_id(), &signer().public_key_bytes())
        .unwrap();
    CommandVerifier::new(
        trusted,
        VerifyPolicy {
            max_skew: 60,
            max_lifetime: 900,
            min_signatures: 1,
        },
        Box::new(MemoryNonceStore::new()),
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Random float-free parameters: sign and verify round trip.
    #[test]
    fn random_params_sign_and_verify(params in params_strategy()) {
        let command = command_with(params);
        let envelope = sign(&command, &signer()).unwrap();
        let verified = verifier().verify_sync(&envelope, "agent-node-7", NOW).unwrap();
        assert_eq!(verified.command, command);
        assert_eq!(verified.signers, vec![signer().key_id()]);
    }

    /// Flipping any single byte of the signed message breaks verification: the
    /// signature covers every byte of the domain-separated canonical document.
    #[test]
    fn flipping_any_message_byte_breaks_verification(
        params in params_strategy(),
        byte_hint in 0_usize..64,
    ) {
        let command = command_with(params);
        let mut message = command.signed_message().unwrap();
        // Index modulo the real length: every byte reachable, no rejected cases.
        let byte_index = byte_hint % message.len();
        message[byte_index] ^= 0x01;
        // Sign over the flipped message; the forged envelope keeps the original
        // command, so the signature must not verify for it.
        let tampered_signature = Signer::sign(&signer(), &message);
        let forged = spot_signed_command::SignedCommand {
            command: command.clone(),
            signatures: vec![spot_signed_command::CommandSignature {
                key_id: signer().key_id(),
                signature: tampered_signature,
            }],
        };
        let result = verifier().verify_sync(&forged, "agent-node-7", NOW);
        // A flip always changes the message (every byte of the canonical document is
        // significant), so the signature over the flipped message never matches the
        // original command.
        prop_assert!(result.is_err());
    }

    /// Flipping any single byte of the signature itself breaks verification.
    #[test]
    fn flipping_any_signature_byte_breaks_verification(
        params in params_strategy(),
        byte_index in 0_usize..64,
    ) {
        let command = command_with(params);
        let mut envelope = sign(&command, &signer()).unwrap();
        let mut bytes = envelope.signatures[0].signature.to_bytes();
        bytes[byte_index] ^= 0x01;
        envelope.signatures[0].signature = ed25519_dalek::Signature::from_bytes(&bytes);
        let result = verifier().verify_sync(&envelope, "agent-node-7", NOW);
        prop_assert!(result.is_err());
    }
}

/// The signed message of two different commands never collides byte-for-byte at the
/// same length: the canonical encoding is injective on the supported subset (spot
/// checks, not a proof).
#[test]
fn different_commands_differ_in_bytes() {
    let a = command_with(serde_json::json!({"service": "a"}));
    let b = command_with(serde_json::json!({"service": "b"}));
    assert_ne!(a.signed_message().unwrap(), b.signed_message().unwrap());
    // Same params, different op: different bytes.
    let mut c = command_with(serde_json::json!({"service": "a"}));
    let rebuilt = Command::from_parts(spot_signed_command::command::UncheckedCommand {
        version: c.version(),
        id: c.id().to_owned(),
        op: "other_op".to_owned(),
        params: c.params().clone(),
        audience: c.audience().to_owned(),
        issuer: c.issuer().clone(),
        issued_at: c.issued_at(),
        expires_at: c.expires_at(),
        nonce: *c.nonce(),
    })
    .unwrap();
    c = rebuilt;
    assert_ne!(a.signed_message().unwrap(), c.signed_message().unwrap());
}
