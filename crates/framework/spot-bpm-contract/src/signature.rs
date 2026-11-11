//! Signatures on a contract version, and the verification policy that is still
//! open (ADR-080 decision 4).
//!
//! The architecture owner decided that **who signs a process contract** is an
//! implementation detail, settled when the first cross-organisation process
//! needs it. This crate therefore:
//!
//! - **stores** every signature a document carries, as data;
//! - **verifies every signature that is present** ([`verify_signatures`]), with
//!   the verifier the host supplies — no cryptography is baked in, and this
//!   crate reads no key material;
//! - **requires none**: a contract with no signatures is valid, which is what
//!   "one author and participants accept a version" looks like today.
//!
//! A signature covers the [content hash](crate::ProcessContract::content_hash),
//! never the file: a whitespace change or a re-ordered participant list does
//! not invalidate it, a changed message does.

use crate::check::{Problem, codes};
use crate::model::{ProcessContract, Signature};

/// The identity a verifier needs to check one signature.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignatureKey<'a> {
    /// The role that signed.
    pub role: &'a str,
    /// The algorithm token the signer used.
    pub algorithm: &'a str,
    /// The identifier of the public key in the signer's vault.
    pub key_id: &'a str,
}

/// Verifies a signature over a contract's content hash.
///
/// A host implements this over its key store (the vault, an agent enrolment
/// key, a bundle signing key). It returns `false` for a signature it cannot
/// verify; it must not read or log the key material.
pub trait SignatureVerifier {
    /// Whether `signature` over `content_hash` is valid for `key`.
    fn verify(&self, key: &SignatureKey<'_>, content_hash: &str, signature: &str) -> bool;
}

/// A verifier that accepts every structurally valid signature, for a host
/// whose signing policy is not settled yet and for tests.
#[derive(Debug, Clone, Copy, Default)]
pub struct AcceptAnySignature;

impl SignatureVerifier for AcceptAnySignature {
    fn verify(&self, _key: &SignatureKey<'_>, _content_hash: &str, _signature: &str) -> bool {
        true
    }
}

/// A verifier that refuses every signature, for tests and for a host in a
/// signing-required mode where the key is not enrolled.
#[derive(Debug, Clone, Copy, Default)]
pub struct RefuseEverySignature;

impl SignatureVerifier for RefuseEverySignature {
    fn verify(&self, _key: &SignatureKey<'_>, _content_hash: &str, _signature: &str) -> bool {
        false
    }
}

/// Verifies every signature the contract carries: structure, coverage and the
/// verifier's answer.
///
/// A contract with no signatures yields no problems: requiring a signature is a
/// policy the caller applies by checking `contract.signatures.is_empty()`
/// itself, not something this crate decides (ADR-080 decision 4).
#[must_use]
pub fn verify_signatures(
    contract: &ProcessContract,
    verifier: &dyn SignatureVerifier,
) -> Vec<Problem> {
    let hash = contract.content_hash().as_hex().to_owned();
    let mut problems = Vec::new();
    for signature in &contract.signatures {
        if signature.signed_hash != hash {
            problems.push(Problem::new(
                codes::SIGNATURE,
                signature.role.clone(),
                format!(
                    "the signature of `{}` covers a different version of the contract",
                    signature.role
                ),
            ));
            continue;
        }
        if !verify_one(signature, &hash, verifier) {
            problems.push(Problem::new(
                codes::SIGNATURE,
                signature.role.clone(),
                format!(
                    "the signature of `{}` did not verify with key `{}`",
                    signature.role, signature.key_id
                ),
            ));
        }
    }
    problems.sort();
    problems.dedup();
    problems
}

/// The verifier's answer for one signature, guarded against an empty signature
/// the verifier might treat as valid.
fn verify_one(signature: &Signature, hash: &str, verifier: &dyn SignatureVerifier) -> bool {
    if signature.signature.is_empty() || signature.key_id.is_empty() {
        return false;
    }
    let key = SignatureKey {
        role: &signature.role,
        algorithm: &signature.algorithm,
        key_id: &signature.key_id,
    };
    verifier.verify(&key, hash, &signature.signature)
}
