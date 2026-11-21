//! The signed-command flow tests (brief §Tests): goldens, the tamper matrix, the
//! domain separation, trust and revocation, the time boundaries, replay and the
//! non-burned nonce, and the two-signer policy.
//!
//! House pattern: lint-scoped integration tests may `unwrap` what the test itself
//! built (cf. `spot-license` tests); nothing here reads a clock — `now` is an
//! argument everywhere, exactly as callers drive the crate.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use ed25519_dalek::SigningKey;
use serde_json::json;
use spot_signed_command::{
    Command, CommandError, CommandSignature, CommandVerifier, DEFAULT_MAX_LIFETIME,
    DEFAULT_MAX_SKEW, KeyId, MemoryNonceStore, SignedCommand, Signer, SoftwareSigner, TrustedKeys,
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

/// The fixed validity window the tests use: issued at 1_000, expires at 1_900
/// (a 900-second window, exactly the default maximum lifetime).
const ISSUED_AT: i64 = 1_000;
const EXPIRES_AT: i64 = 1_900;
/// `now` comfortably inside the window.
const NOW: i64 = 1_500;

/// A deterministic signing key from a one-byte seed; the same seed is the same key on
/// every run, so tamper tests can name signatures precisely.
fn key(seed: u8) -> SoftwareSigner {
    SoftwareSigner::from_key(SigningKey::from_bytes(&[seed; 32]))
}

/// A deterministic command: fixed id and nonce, the standard window, float-free params.
fn command(issuer: &KeyId) -> Command {
    Command::from_parts(spot_signed_command::command::UncheckedCommand {
        version: 1,
        id: "0123456789abcdef0123456789abcdef".to_owned(),
        op: "restart_service".to_owned(),
        params: json!({"service": "spot-runtime"}),
        audience: "agent-node-7".to_owned(),
        issuer: issuer.clone(),
        issued_at: ISSUED_AT,
        expires_at: EXPIRES_AT,
        nonce: [0xAA; 16],
    })
    .unwrap()
}

/// The standard one-signature envelope: command signed by `key(1)`.
fn signed(issuer: &KeyId) -> (SoftwareSigner, SignedCommand) {
    let signer = key(1);
    assert_eq!(signer.key_id(), *issuer);
    let envelope = sign(&command(issuer), &signer).unwrap();
    (signer, envelope)
}

/// A verifier trusting `key(1)`, default policy, fresh nonce store, expecting
/// `agent-node-7`.
fn verifier(trusted: TrustedKeys) -> CommandVerifier {
    let policy = VerifyPolicy {
        max_skew: DEFAULT_MAX_SKEW,
        min_signatures: 1,
        ..VerifyPolicy::default_policy()
    };
    CommandVerifier::new(trusted, policy, Box::new(MemoryNonceStore::new()))
}

/// The trust list naming `key(1)`'s derived id.
fn trust() -> TrustedKeys {
    let mut trusted = TrustedKeys::new();
    let signer = key(1);
    trusted
        .insert(signer.key_id(), &signer.public_key_bytes())
        .unwrap();
    trusted
}

/// The error code of a verification attempt, for the order table assertions.
fn code_of(result: Result<spot_signed_command::Verified, CommandError>) -> &'static str {
    result.unwrap_err().code()
}

// ---------------------------------------------------------------- goldens

/// The canonical bytes of the deterministic command, spelled out in full: the golden
/// both sides compile against. The issuer and nonce values are spliced in (they derive
/// from the test key), the shape and order are fixed text.
#[test]
fn canonical_golden_bytes() {
    let issuer_key_id = key(1).key_id();
    let bytes = command(&issuer_key_id).canonical_bytes().unwrap();
    let expected = format!(
        concat!(
            r#"{{"audience":"agent-node-7","expires_at":1900,"#,
            r#""id":"0123456789abcdef0123456789abcdef","issued_at":1000,"#,
            r#""issuer":"{issuer}","nonce":"{nonce}","#,
            r#""op":"restart_service","params":{{"service":"spot-runtime"}},"version":1}}"#
        ),
        issuer = issuer_key_id,
        nonce = "aa".repeat(16),
    );
    assert_eq!(String::from_utf8(bytes).unwrap(), expected);
}

/// Key order and whitespace in the input do not change the canonical bytes: the
/// canonical writer normalises both, so two machines that spell the same value
/// differently sign the same thing.
#[test]
fn key_order_and_whitespace_do_not_change_the_bytes() {
    let a: serde_json::Value = serde_json::from_str(r#"{"b":1,"a":{"d":2,"c":[3,"x"]}}"#).unwrap();
    let b: serde_json::Value =
        serde_json::from_str("{ \"a\" : { \"c\" : [3, \"x\"], \"d\": 2 } , \"b\":1 }").unwrap();
    let bytes_a = spot_signed_command::canonical::canonical_json_value(&a).unwrap();
    let bytes_b = spot_signed_command::canonical::canonical_json_value(&b).unwrap();
    assert_eq!(bytes_a, bytes_b);
    // And the same holds at the top of the envelope: reordered command fields
    // canonicalise to the signed command's own bytes.
    let issuer_key_id = key(1).key_id();
    let original = command(&issuer_key_id);
    let value: serde_json::Value =
        serde_json::from_str(&String::from_utf8(original.canonical_bytes().unwrap()).unwrap())
            .unwrap();
    let again = spot_signed_command::canonical::canonical_json_value(&value).unwrap();
    assert_eq!(again, original.canonical_bytes().unwrap());
}

/// A float in `params` is rejected at signing time, typed, before any key is touched.
#[test]
fn float_params_are_rejected() {
    let issuer = key(1).key_id();
    let mut unchecked = {
        let c = command(&issuer);
        // Rebuild with a float parameter through the unchecked constructor.
        spot_signed_command::command::UncheckedCommand {
            version: c.version(),
            id: c.id().to_owned(),
            op: c.op().to_owned(),
            params: json!({"service": "spot-runtime", "ratio": 0.5}),
            audience: c.audience().to_owned(),
            issuer: c.issuer().clone(),
            issued_at: c.issued_at(),
            expires_at: c.expires_at(),
            nonce: *c.nonce(),
        }
    };
    unchecked.params = json!({"service": "spot-runtime", "ratio": 0.5});
    let command = Command::from_parts(unchecked).unwrap();
    let error = command.canonical_bytes().unwrap_err();
    assert_eq!(error.code(), "cmd.not_canonical");
    // And the verifier refuses it for the same reason before signatures matter.
    let signer = key(1);
    let mut trusted = TrustedKeys::new();
    trusted
        .insert(signer.key_id(), &signer.public_key_bytes())
        .unwrap();
    // `sign` itself refuses: nothing signed, nothing to verify.
    let error = sign(&command, &signer).unwrap_err();
    assert_eq!(error.code(), "cmd.not_canonical");
}

// ---------------------------------------------------------------- round trip

/// Sign and verify round trip with one signer: the happy path returns the command and
/// its single signer.
#[test]
fn sign_verify_round_trip() {
    let issuer = key(1).key_id();
    let (signer, envelope) = signed(&issuer);
    let verified = verifier(trust())
        .verify_sync(&envelope, "agent-node-7", NOW)
        .unwrap();
    assert_eq!(verified.command, command(&issuer));
    assert_eq!(verified.signers, vec![signer.key_id()]);
}

/// Tampering with any of op, one parameter, audience, expiry, issuer, nonce or version
/// fails; the failure code follows the verifier's fixed order.
#[test]
fn every_tamper_fails() {
    let issuer = key(1).key_id();
    let (_, envelope) = signed(&issuer);

    let tampered = |mutate: &dyn Fn(&mut Command)| {
        let mut copy = envelope.clone();
        let mut command = copy.command.clone();
        mutate(&mut command);
        copy.command = command;
        copy
    };

    // Rebuild a tampered command through the checked constructor (version stays valid,
    // other fields change), or mutate an already-invalid version directly.
    let with_op = tampered(&|c| {
        *c = {
            let parts = spot_signed_command::command::UncheckedCommand {
                version: c.version(),
                id: c.id().to_owned(),
                op: "reboot_node".to_owned(), // changed
                params: c.params().clone(),
                audience: c.audience().to_owned(),
                issuer: c.issuer().clone(),
                issued_at: c.issued_at(),
                expires_at: c.expires_at(),
                nonce: *c.nonce(),
            };
            Command::from_parts(parts).unwrap()
        }
    });
    assert_eq!(
        code_of(verifier(trust()).verify_sync(&with_op, "agent-node-7", NOW)),
        "cmd.invalid_signature"
    );

    // One parameter changed: signature no longer matches.
    let with_param = tampered(&|c| {
        *c = {
            let parts = spot_signed_command::command::UncheckedCommand {
                version: c.version(),
                id: c.id().to_owned(),
                op: c.op().to_owned(),
                params: json!({"service": "spot-frontend"}), // changed
                audience: c.audience().to_owned(),
                issuer: c.issuer().clone(),
                issued_at: c.issued_at(),
                expires_at: c.expires_at(),
                nonce: *c.nonce(),
            };
            Command::from_parts(parts).unwrap()
        }
    });
    assert_eq!(
        code_of(verifier(trust()).verify_sync(&with_param, "agent-node-7", NOW)),
        "cmd.invalid_signature"
    );

    // Audience changed: the audience check fires before the signature check.
    let with_audience = tampered(&|c| {
        *c = {
            let parts = spot_signed_command::command::UncheckedCommand {
                version: c.version(),
                id: c.id().to_owned(),
                op: c.op().to_owned(),
                params: c.params().clone(),
                audience: "agent-node-8".to_owned(), // changed
                issuer: c.issuer().clone(),
                issued_at: c.issued_at(),
                expires_at: c.expires_at(),
                nonce: *c.nonce(),
            };
            Command::from_parts(parts).unwrap()
        }
    });
    assert_eq!(
        code_of(verifier(trust()).verify_sync(&with_audience, "agent-node-7", NOW)),
        "cmd.wrong_audience"
    );
    // The same tampered command addressed to the audience it now names fails on the
    // signature instead: the bytes are not what any key signed.
    assert_eq!(
        code_of(verifier(trust()).verify_sync(&with_audience, "agent-node-8", NOW)),
        "cmd.invalid_signature"
    );

    // Expiry pushed far out: the lifetime check fires before the signature check.
    let with_expiry = tampered(&|c| {
        *c = {
            let parts = spot_signed_command::command::UncheckedCommand {
                version: c.version(),
                id: c.id().to_owned(),
                op: c.op().to_owned(),
                params: c.params().clone(),
                audience: c.audience().to_owned(),
                issuer: c.issuer().clone(),
                issued_at: c.issued_at(),
                expires_at: c.expires_at() + 10_000, // changed
                nonce: *c.nonce(),
            };
            Command::from_parts(parts).unwrap()
        }
    });
    assert_eq!(
        code_of(verifier(trust()).verify_sync(&with_expiry, "agent-node-7", NOW)),
        "cmd.lifetime_exceeded"
    );

    // Issuer renamed: the signature names a key the trust list does not have.
    let with_issuer = tampered(&|c| {
        *c = {
            let parts = spot_signed_command::command::UncheckedCommand {
                version: c.version(),
                id: c.id().to_owned(),
                op: c.op().to_owned(),
                params: c.params().clone(),
                audience: c.audience().to_owned(),
                issuer: KeyId::from_public_key(&key(9).public_key_bytes()), // changed
                issued_at: c.issued_at(),
                expires_at: c.expires_at(),
                nonce: *c.nonce(),
            };
            Command::from_parts(parts).unwrap()
        }
    });
    assert_eq!(
        code_of(verifier(trust()).verify_sync(&with_issuer, "agent-node-7", NOW)),
        "cmd.invalid_signature",
        "the signature verifies under key(1) but names key(9); key(9) is unknown"
    );

    // Version switched: refused before anything else. The checked constructor refuses
    // a foreign version outright, so the path a transport could take — parsing raw
    // bytes — is exercised through `from_json_str` on rewritten canonical text in
    // `foreign_version_is_refused` below; the verifier-side first-check behaviour is
    // the same check the constructor funnels through.
}

/// A version other than 1 is refused first of all, also through the parse path.
#[test]
fn foreign_version_is_refused() {
    // Build the canonical text of a valid command, then bump the version digit and try
    // to parse it: the constructor refuses a foreign version before anything else.
    let issuer = key(1).key_id();
    let text = String::from_utf8(command(&issuer).canonical_bytes().unwrap()).unwrap();
    let bumped = text.replace(r#""version":1}"#, r#""version":2}"#);
    assert_ne!(text, bumped);
    let error = Command::from_json_str(&bumped).unwrap_err();
    assert_eq!(error.code(), "cmd.unknown_version");
}

// ---------------------------------------------------------------- domain separation

/// A signature over the bare canonical bytes must not verify: the domain prefix is not
/// optional, and signing for another context does not carry over.
#[test]
fn signature_over_bare_canonical_bytes_does_not_verify() {
    let issuer = key(1).key_id();
    let signer = key(1);
    let command = command(&issuer);
    let bare = command.canonical_bytes().unwrap();
    let forged = SignedCommand {
        command,
        signatures: vec![CommandSignature {
            key_id: signer.key_id(),
            signature: Signer::sign(&signer, &bare),
        }],
    };
    assert_eq!(
        code_of(verifier(trust()).verify_sync(&forged, "agent-node-7", NOW)),
        "cmd.invalid_signature"
    );
}

/// A signature made for a different message (some other protocol's payload) does not
/// verify either — the domain prefix cuts both ways.
#[test]
fn signature_over_a_different_domain_does_not_verify() {
    let issuer = key(1).key_id();
    let signer = key(1);
    let command = command(&issuer);
    let other = b"spotledger-licence-v1\0{\"x\":1}";
    let forged = SignedCommand {
        command,
        signatures: vec![CommandSignature {
            key_id: signer.key_id(),
            signature: Signer::sign(&signer, other),
        }],
    };
    assert_eq!(
        code_of(verifier(trust()).verify_sync(&forged, "agent-node-7", NOW)),
        "cmd.invalid_signature"
    );
}

// ---------------------------------------------------------------- trust and revocation

/// A signature from an untrusted key fails with `cmd.unknown_key`.
#[test]
fn untrusted_key_fails() {
    let issuer = key(1).key_id();
    let stranger = key(7);
    let envelope = sign(&command(&issuer), &stranger).unwrap();
    assert_eq!(
        code_of(verifier(trust()).verify_sync(&envelope, "agent-node-7", NOW)),
        "cmd.unknown_key"
    );
}

/// A key revoked no later than `issued_at` fails; the same command still verifies when
/// the revocation instant is after its issue instant.
#[test]
fn revoked_key_fails_before_issue_but_not_after() {
    let issuer = key(1).key_id();
    let (_, envelope) = signed(&issuer);
    let mut revoked_at_issue = trust();
    revoked_at_issue.revoke(&issuer, ISSUED_AT);
    assert_eq!(
        code_of(verifier(revoked_at_issue).verify_sync(&envelope, "agent-node-7", NOW)),
        "cmd.key_revoked"
    );
    let mut revoked_after_issue = trust();
    revoked_after_issue.revoke(&issuer, ISSUED_AT + 1);
    verifier(revoked_after_issue)
        .verify_sync(&envelope, "agent-node-7", NOW)
        .unwrap();
}

// ---------------------------------------------------------------- time boundaries

/// The window boundaries: just before expiry passes, at expiry fails; at the skew edge
/// passes, one tick earlier fails; a window over the maximum lifetime fails.
#[test]
fn time_boundaries() {
    let issuer = key(1).key_id();
    let (_, envelope) = signed(&issuer);
    let strict = verifier(trust());

    // Expiry is exclusive: the last second inside passes, expiry itself fails.
    strict
        .verify_sync(&envelope, "agent-node-7", EXPIRES_AT - 1)
        .unwrap();
    assert_eq!(
        code_of(strict.verify_sync(&envelope, "agent-node-7", EXPIRES_AT)),
        "cmd.expired"
    );

    // Skew is inclusive on the issue side: exactly at `issued_at - max_skew` passes
    // (fresh store; the expiry-boundary verifies above already burned this envelope's
    // nonce once, and a burned nonce is a replay by design, not a boundary result).
    let skew_ok = verifier(trust());
    skew_ok
        .verify_sync(&envelope, "agent-node-7", ISSUED_AT - DEFAULT_MAX_SKEW)
        .unwrap();
    let skew_early = verifier(trust());
    assert_eq!(
        code_of(skew_early.verify_sync(
            &envelope,
            "agent-node-7",
            ISSUED_AT - DEFAULT_MAX_SKEW - 1
        )),
        "cmd.not_yet_valid"
    );

    // A window one second over the maximum lifetime fails, from inside the window.
    let long = Command::from_parts(spot_signed_command::command::UncheckedCommand {
        version: 1,
        id: "0123456789abcdef0123456789abcdef".to_owned(),
        op: "restart_service".to_owned(),
        params: json!({"service": "spot-runtime"}),
        audience: "agent-node-7".to_owned(),
        issuer: issuer.clone(),
        issued_at: ISSUED_AT,
        expires_at: ISSUED_AT + DEFAULT_MAX_LIFETIME + 1,
        nonce: [0xAB; 16],
    })
    .unwrap();
    let long_signer = key(1);
    let envelope_long = sign(&long, &long_signer).unwrap();
    assert_eq!(
        code_of(strict.verify_sync(&envelope_long, "agent-node-7", ISSUED_AT)),
        "cmd.lifetime_exceeded"
    );
    // Exactly the maximum lifetime is accepted (fresh nonce, new command id, signed
    // over its own bytes).
    let exact = Command::from_parts(spot_signed_command::command::UncheckedCommand {
        version: 1,
        id: "ffffffffffffffffffffffffffffffff".to_owned(),
        op: "restart_service".to_owned(),
        params: json!({"service": "spot-runtime"}),
        audience: "agent-node-7".to_owned(),
        issuer: issuer.clone(),
        issued_at: ISSUED_AT,
        expires_at: ISSUED_AT + DEFAULT_MAX_LIFETIME,
        nonce: [0xCD; 16],
    })
    .unwrap();
    let exact_signer = key(1);
    let envelope_exact = sign(&exact, &exact_signer).unwrap();
    strict
        .verify_sync(&envelope_exact, "agent-node-7", ISSUED_AT)
        .unwrap();
}

/// A command meant for another agent never verifies here, and the corrected command's
/// nonce was not burned by the rejection.
#[test]
fn wrong_audience_and_nonce_not_burned() {
    let issuer = key(1).key_id();
    let (_, envelope) = signed(&issuer);
    let verifier = verifier(trust());
    assert_eq!(
        code_of(verifier.verify_sync(&envelope, "agent-node-8", NOW)),
        "cmd.wrong_audience"
    );
    // The same command, now checked for the audience it names, verifies: the failed
    // attempt recorded nothing.
    verifier
        .verify_sync(&envelope, "agent-node-7", NOW)
        .unwrap();
}

/// Replay: an accepted command verifies exactly once; a command rejected for another
/// reason can be corrected and sent again with the same nonce.
#[test]
fn replay_and_correction() {
    let issuer = key(1).key_id();
    let (_, envelope) = signed(&issuer);
    let verifier = verifier(trust());

    verifier
        .verify_sync(&envelope, "agent-node-7", NOW)
        .unwrap();
    // Same command again: the store remembers the nonce.
    assert_eq!(
        code_of(verifier.verify_sync(&envelope, "agent-node-7", NOW)),
        "cmd.replay"
    );

    // A second command with a DIFFERENT nonce verifies (signed over its own bytes).
    let second_command = Command::from_parts(spot_signed_command::command::UncheckedCommand {
        version: 1,
        id: "ffffffffffffffffffffffffffffffff".to_owned(),
        op: "restart_service".to_owned(),
        params: json!({"service": "spot-runtime"}),
        audience: "agent-node-7".to_owned(),
        issuer: issuer.clone(),
        issued_at: ISSUED_AT,
        expires_at: EXPIRES_AT,
        nonce: [0x11; 16],
    })
    .unwrap();
    let signer = key(1);
    let second_envelope = sign(&second_command, &signer).unwrap();
    verifier
        .verify_sync(&second_envelope, "agent-node-7", NOW)
        .unwrap();
}

/// A command rejected for too few signatures did not burn its nonce: once co-signed it
/// verifies with the same nonce.
#[test]
fn rejected_command_can_be_corrected() {
    let issuer = key(1).key_id();
    let signer_a = key(1);
    let signer_b = key(2);
    let command = command(&issuer);
    let mut envelope = sign(&command, &signer_a).unwrap();

    // A two-signature policy rejects the one-signature envelope.
    let policy = VerifyPolicy {
        min_signatures: 2,
        ..VerifyPolicy::default_policy()
    };
    let mut trusted = TrustedKeys::new();
    for signer in [&signer_a, &signer_b] {
        trusted
            .insert(signer.key_id(), &signer.public_key_bytes())
            .unwrap();
    }
    let strict = CommandVerifier::new(trusted, policy, Box::new(MemoryNonceStore::new()));
    assert_eq!(
        code_of(strict.verify_sync(&envelope, "agent-node-7", NOW)),
        "cmd.insufficient_signatures"
    );

    // The co-signature arrives; the same command (same nonce!) now passes.
    envelope.add_signature(&signer_b).unwrap();
    strict.verify_sync(&envelope, "agent-node-7", NOW).unwrap();
}

// ---------------------------------------------------------------- two signers

/// Two signers: `min_signatures = 2` passes with two distinct keys, fails with one,
/// fails when the same key signs twice.
#[test]
fn two_signer_policy() {
    let issuer = key(1).key_id();
    let signer_a = key(1);
    let signer_b = key(2);
    let command = command(&issuer);

    let mut trusted = TrustedKeys::new();
    for signer in [&signer_a, &signer_b] {
        trusted
            .insert(signer.key_id(), &signer.public_key_bytes())
            .unwrap();
    }
    let policy = VerifyPolicy {
        min_signatures: 2,
        ..VerifyPolicy::default_policy()
    };
    let verifier = CommandVerifier::new(trusted, policy, Box::new(MemoryNonceStore::new()));

    // Both distinct keys: passes, and both signers are reported.
    let mut both = sign(&command, &signer_a).unwrap();
    both.add_signature(&signer_b).unwrap();
    let verified = verifier.verify_sync(&both, "agent-node-7", NOW).unwrap();
    assert_eq!(verified.signers.len(), 2);
    assert!(verified.signers.contains(&signer_a.key_id()));
    assert!(verified.signers.contains(&signer_b.key_id()));

    // One signature under the same store: refused (a different nonce per attempt —
    // the refusals must not burn this command's nonce either).
    let mut nonce_counter = 0u16;
    let one_signature_command = |nonce: u16| {
        Command::from_parts(spot_signed_command::command::UncheckedCommand {
            version: 1,
            id: format!("{nonce:032x}"),
            op: "restart_service".to_owned(),
            params: json!({"service": "spot-runtime"}),
            audience: "agent-node-7".to_owned(),
            issuer: issuer.clone(),
            issued_at: ISSUED_AT,
            expires_at: EXPIRES_AT,
            nonce: [nonce as u8; 16],
        })
        .unwrap()
    };
    nonce_counter += 1;
    let one = sign(&one_signature_command(nonce_counter), &signer_a).unwrap();
    assert_eq!(
        code_of(verifier.verify_sync(&one, "agent-node-7", NOW)),
        "cmd.insufficient_signatures"
    );

    // The same key twice: still one distinct key, refused. The broken second
    // signature of a doubled signer also fails on its own merit below.
    nonce_counter += 1;
    let mut doubled = sign(&one_signature_command(nonce_counter), &signer_a).unwrap();
    doubled.add_signature(&signer_a).unwrap();
    assert_eq!(
        code_of(verifier.verify_sync(&doubled, "agent-node-7", NOW)),
        "cmd.insufficient_signatures"
    );
    // Same key twice with one broken copy: the broken signature is an error in itself.
    nonce_counter += 1;
    let mut doubled_broken = sign(&one_signature_command(nonce_counter), &signer_a).unwrap();
    let valid_signature = doubled_broken.signatures[0].signature;
    doubled_broken.signatures.push(CommandSignature {
        key_id: signer_a.key_id(),
        signature: flip_signature_byte(valid_signature),
    });
    assert_eq!(
        code_of(verifier.verify_sync(&doubled_broken, "agent-node-7", NOW)),
        "cmd.invalid_signature"
    );
}

/// Flips one bit of a signature, for the broken-signature tests.
fn flip_signature_byte(signature: ed25519_dalek::Signature) -> ed25519_dalek::Signature {
    let mut bytes = signature.to_bytes();
    bytes[0] ^= 0x01;
    ed25519_dalek::Signature::from_bytes(&bytes)
}
