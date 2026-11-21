//! [`SignedCommandHook`]: the hardware-signature hook over `spot-signed-command`.
//!
//! An approval's `signature` is the hex of this text:
//!
//! ```text
//! <canonical command JSON>
//! <key id>:<signature hex>          one line per signature
//! ```
//!
//! The command is `APPROVE_OP` with exactly the params `{"approver", "plan_digest"}`
//! ([`approval_params`]), so a signature binds **this plan digest and this approver**: it is
//! useless for another plan, another approver, or any other command. The signed command is
//! verified with `spot-signed-command`'s verifier (audience, validity window, lifetime, trusted
//! and unrevoked keys, valid signatures) at the approval's own time.
//!
//! A trusted key is **bound to one approver** ([`ApproverKey`]): the signature is accepted only
//! when every key that signed is bound to the approval's `approver`, so a key holder cannot sign
//! an approval that names someone else. A key with no subject is not trusted
//! ([`ApprovalError::KeyNotBound`]).
//!
//! Replay protection is deliberately off here: the same approval is re-evaluated on every
//! `status` and `authorise_apply`, and re-presenting it can only repeat the same digest-bound
//! approval, which the service already counts once per approver.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use ed25519_dalek::Signature;
use serde_json::{Value, json};
use spot_signed_command::{
    Command, CommandError, CommandSignature, CommandVerifier, KeyId, NonceFuture, NonceStore,
    SignedCommand, StoreError, TrustedKeys, VerifyPolicy,
};

use crate::approval::{Approval, SignatureHook};
use crate::digest::PlanDigest;
use crate::error::ApprovalError;

/// The operation a plan approval signs.
pub const APPROVE_OP: &str = concat!("migration", ".approve_plan");

/// The params a signed approval must carry, exactly.
#[must_use]
pub fn approval_params(digest: &PlanDigest, approver: &str) -> Value {
    json!({ "approver": approver, "plan_digest": digest.as_hex() })
}

/// Encodes `signed` as an approval's `signature` text (see the module docs).
///
/// # Errors
/// [`ApprovalError::BadCommand`] when the command has no canonical form.
pub fn encode_signature(signed: &SignedCommand) -> Result<String, ApprovalError> {
    let canonical = signed
        .command
        .canonical_bytes()
        .map_err(|e| ApprovalError::BadCommand {
            message: format!("the command cannot be encoded: {}", e.code()),
        })?;
    let mut text = String::from_utf8(canonical).map_err(|_| ApprovalError::BadCommand {
        message: "the command is not text".to_owned(),
    })?;
    for entry in &signed.signatures {
        text.push('\n');
        text.push_str(entry.key_id.as_str());
        text.push(':');
        text.push_str(&to_hex(&entry.signature.to_bytes()));
    }
    Ok(to_hex(text.as_bytes()))
}

/// A nonce store that accepts every nonce (see the module docs for why).
struct Unchecked;

impl NonceStore for Unchecked {
    fn check_and_insert<'a>(
        &'a self,
        _key: &'a KeyId,
        _nonce: [u8; 16],
        _expires_at: i64,
        _now: i64,
    ) -> NonceFuture<'a, Result<bool, StoreError>> {
        // Always immediately ready: nothing is replayed against a window here.
        Box::pin(std::future::ready(Ok(true)))
    }
}

/// Drives `future` with a single poll and **fails closed** when it is not ready.
///
/// The hook is synchronous, and its verifier uses the always-ready [`Unchecked`] store, so
/// the verification future completes on the first poll. A future that is `Pending` is
/// refused with [`StoreError::Unavailable`] and never treated as verified.
///
/// This helper must **never** be used with a durable or PostgreSQL nonce store: such a
/// store can be pending, and this would then refuse every command.
fn poll_once<T>(future: impl Future<Output = Result<T, CommandError>>) -> Result<T, CommandError> {
    // `Waker::noop` is safe std: nothing is ever woken, the future is polled exactly once.
    let mut context = Context::from_waker(Waker::noop());
    let mut future = pin!(future);
    match future.as_mut().poll(&mut context) {
        Poll::Ready(result) => result,
        Poll::Pending => Err(CommandError::Store(StoreError::Unavailable)),
    }
}

/// A trusted approval key and the one person who holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApproverKey {
    /// The Ed25519 public key.
    pub public_key: [u8; 32],
    /// The subject (approver) the key is bound to; a key without one is refused.
    pub subject: Option<String>,
}

impl ApproverKey {
    /// A key bound to `subject`.
    #[must_use]
    pub fn bound(public_key: [u8; 32], subject: impl Into<String>) -> Self {
        Self {
            public_key,
            subject: Some(subject.into()),
        }
    }

    /// A key with no subject. [`SignedCommandHook::new`] refuses it.
    #[must_use]
    pub fn unbound(public_key: [u8; 32]) -> Self {
        Self {
            public_key,
            subject: None,
        }
    }
}

/// Verifies the signature on a destructive plan's approval.
pub struct SignedCommandHook {
    trusted: TrustedKeys,
    ids: BTreeMap<String, KeyId>,
    subjects: BTreeMap<String, String>,
    policy: VerifyPolicy,
    audience: String,
}

impl SignedCommandHook {
    /// A hook that trusts `keys` (each bound to one approver), applies `policy`, and accepts
    /// commands addressed to `audience` (the site).
    ///
    /// # Errors
    /// [`ApprovalError::KeyNotBound`] when a key has no subject (it is not trusted for
    /// approvals), [`ApprovalError::BadPolicy`] when a key is not a valid Ed25519 public key.
    pub fn new(
        keys: &[ApproverKey],
        policy: VerifyPolicy,
        audience: impl Into<String>,
    ) -> Result<Self, ApprovalError> {
        let mut trusted = TrustedKeys::new();
        let mut ids = BTreeMap::new();
        let mut subjects = BTreeMap::new();
        for entry in keys {
            let subject = match entry.subject.as_deref() {
                Some(subject) if !subject.trim().is_empty() => subject,
                _ => return Err(ApprovalError::KeyNotBound),
            };
            let key = &entry.public_key;
            let id = KeyId::from_public_key(key);
            subjects.insert(id.as_str().to_owned(), subject.to_owned());
            trusted
                .insert(id.clone(), key)
                .map_err(|_| ApprovalError::BadPolicy {
                    message: "a trusted signing key is not a valid public key".to_owned(),
                })?;
            ids.insert(id.as_str().to_owned(), id);
        }
        Ok(Self {
            trusted,
            ids,
            subjects,
            policy,
            audience: audience.into(),
        })
    }

    /// Distrusts `public_key` for commands issued at or after `revoked_at` (unix seconds).
    pub fn revoke(&mut self, public_key: &[u8; 32], revoked_at: i64) {
        self.trusted
            .revoke(&KeyId::from_public_key(public_key), revoked_at);
    }

    fn parse(&self, encoded: &str) -> Option<SignedCommand> {
        let text = String::from_utf8(from_hex(encoded)?).ok()?;
        let mut lines = text.split('\n');
        let command = Command::from_json_str(lines.next()?).ok()?;
        let mut signatures = Vec::new();
        for line in lines {
            let (id, sig_hex) = line.split_once(':')?;
            let key_id = self.ids.get(id)?.clone();
            let signature = Signature::from_slice(&from_hex(sig_hex)?).ok()?;
            signatures.push(CommandSignature { key_id, signature });
        }
        Some(SignedCommand {
            command,
            signatures,
        })
    }

    /// Runs the signed-command verifier with `nonces` and drives it with [`poll_once`].
    fn verify_with(
        &self,
        nonces: Box<dyn NonceStore>,
        signed: &SignedCommand,
        at_seconds: i64,
    ) -> Result<(), CommandError> {
        let verifier = CommandVerifier::new(self.trusted.clone(), self.policy.clone(), nonces);
        poll_once(verifier.verify(signed, &self.audience, at_seconds)).map(|_| ())
    }

    fn check(&self, digest: &PlanDigest, approval: &Approval) -> Result<(), CommandError> {
        let encoded = approval
            .signature
            .as_deref()
            .ok_or(CommandError::Malformed("no signature"))?;
        let signed = self
            .parse(encoded)
            .ok_or(CommandError::Malformed("the signature does not decode"))?;
        // The approval is judged at the time it was given.
        let at_seconds = approval.at.div_euclid(1_000_000);
        self.verify_with(Box::new(Unchecked), &signed, at_seconds)?;
        // Every key that signed must be bound to the approver the approval names.
        if signed.signatures.is_empty()
            || signed.signatures.iter().any(|entry| {
                self.subjects.get(entry.key_id.as_str()).map(String::as_str)
                    != Some(approval.approver.as_str())
            })
        {
            return Err(CommandError::Malformed(
                "a signing key is not bound to this approver",
            ));
        }
        if signed.command.op() != APPROVE_OP
            || *signed.command.params() != approval_params(digest, &approval.approver)
        {
            return Err(CommandError::Malformed(
                "the signature binds another command",
            ));
        }
        Ok(())
    }
}

impl SignatureHook for SignedCommandHook {
    fn verify(&self, digest: &PlanDigest, approval: &Approval) -> bool {
        // The approval must be for this digest, whatever its fields say.
        approval.plan_digest == *digest && self.check(digest, approval).is_ok()
    }
}

fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        out.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
    }
    out
}

fn from_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) || !text.is_ascii() {
        return None;
    }
    text.as_bytes()
        .chunks(2)
        .map(|pair| {
            let high = char::from(pair[0]).to_digit(16)?;
            let low = char::from(pair[1]).to_digit(16)?;
            u8::try_from(high * 16 + low).ok()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use spot_signed_command::{Signer, SoftwareSigner, sign};

    use super::*;

    /// A store whose future never completes.
    struct PendingStore;

    impl NonceStore for PendingStore {
        fn check_and_insert<'a>(
            &'a self,
            _key: &'a KeyId,
            _nonce: [u8; 16],
            _expires_at: i64,
            _now: i64,
        ) -> NonceFuture<'a, Result<bool, StoreError>> {
            Box::pin(std::future::pending())
        }
    }

    fn hook_and_command() -> (SignedCommandHook, SignedCommand) {
        let signer = SoftwareSigner::from_seed(&[3u8; 32]).unwrap();
        let hook = SignedCommandHook::new(
            &[ApproverKey::bound(signer.public_key_bytes(), "bob")],
            VerifyPolicy::default(),
            "site-1",
        )
        .unwrap();
        let command = Command::new(
            APPROVE_OP,
            json!({"approver": "bob"}),
            "site-1",
            signer.key_id(),
            1_000,
            1_500,
        );
        (hook, sign(&command, &signer).unwrap())
    }

    /// A verifier whose store future is pending is refused, never treated as verified.
    #[test]
    fn a_pending_store_future_is_refused() {
        let (hook, signed) = hook_and_command();
        let error = hook
            .verify_with(Box::new(PendingStore), &signed, 1_100)
            .unwrap_err();
        assert!(matches!(
            error,
            CommandError::Store(StoreError::Unavailable)
        ));
        assert_eq!(error.code(), "cmd.nonce_store");
        // The ready store still verifies the same command.
        hook.verify_with(Box::new(Unchecked), &signed, 1_100)
            .unwrap();
    }
}
