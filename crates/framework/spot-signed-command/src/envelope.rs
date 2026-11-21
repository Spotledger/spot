//! The signed envelope, the signing key and the verifier (`WP-ADM-01.T2`).
//!
//! # What is signed
//!
//! [`DOMAIN_PREFIX`](crate::command::DOMAIN_PREFIX)` || canonical bytes` — the domain
//! separator first, then the command's canonical JSON (see [`crate::canonical`]). The
//! prefix
//! makes a signature made for this envelope useless anywhere else (and vice versa): a
//! signature over bare canonical bytes does not verify (the verifier re-checks), and neither does one over a different protocol's payload.
//!
//! # The pieces
//!
//! - [`Signer`]: the abstraction a hardware key implements later (WP-ADM-03); the only
//!   built-in implementation is [`SoftwareSigner`], an Ed25519 key whose `Debug` shows
//!   the key id, never bytes.
//! - [`sign`]: signs into a one-signature [`SignedCommand`].
//! - [`SignedCommand`] / [`CommandSignature`]: the envelope; [`SignedCommand::add_signature`]
//!   co-signs with further keys.
//! - [`CommandVerifier`]: the fixed order of checks with [`TrustedKeys`],
//!   [`VerifyPolicy`] and a [`NonceStore`](crate::nonce::NonceStore).
//!
//! # The verifier's order (first failure wins, documented once here)
//!
//! 1. the command's version is supported (`cmd.unknown_version`);
//! 2. the command parses and its bytes re-canonicalise identically (`cmd.not_canonical`);
//! 3. the audience is this verifier's agent (`cmd.wrong_audience`);
//! 4. `now` is inside `[issued_at − max_skew, expires_at)` (`cmd.not_yet_valid`,
//!    `cmd.expired`);
//! 5. the window length is within the policy's `max_lifetime` (`cmd.lifetime_exceeded`);
//! 6. at least `min_signatures` valid signatures from distinct trusted keys verify
//!    (`cmd.unknown_key`, `cmd.key_revoked`, `cmd.invalid_signature`,
//!    `cmd.insufficient_signatures`);
//! 7. the nonce store accepts the command (the store is checked, and the nonce
//!    recorded, only after all other checks passed — `cmd.replay`,
//!    `cmd.nonce_store`).
//!
//! Steps 1–6 run on the parsed command; step 7's store call decides acceptance and
//! recording **atomically** ([`crate::nonce::NonceStore::check_and_insert`]), so a
//! rejected command cannot burn a nonce and an accepted one cannot be replayed.

use std::collections::BTreeMap;
use std::fmt;

use ed25519_dalek::{Signature, Verifier as _, VerifyingKey};

use crate::command::{Command, KeyId};
use crate::error::CommandError;

/// The length of a raw Ed25519 public key in bytes.
pub(crate) const PUBLIC_KEY_BYTES: usize = 32;

/// The abstraction over the key that signs a command.
///
/// A hardware-backed key (TPM, YubiKey, WP-ADM-03) implements this trait; the signature
/// algorithm is fixed to Ed25519 here, so a hardware implementation only has to route
/// the hash-less Ed25519 operation to the device. [`SoftwareSigner`] is the reference
/// implementation.
pub trait Signer {
    /// The id of the key this signer signs with: the same derivation
    /// [`KeyId::from_public_key`] uses, which the verifier's trust list must name.
    fn key_id(&self) -> KeyId;

    /// The raw 64-byte Ed25519 signature over `msg`.
    ///
    /// # Errors
    /// Implementations that can fail (hardware unavailable, user declined) report it
    /// here in the error type they share with this crate. The built-in
    /// [`SoftwareSigner`] cannot fail.
    fn sign(&self, msg: &[u8]) -> ed25519_dalek::Signature;
}

/// An Ed25519 signing key held in memory, for tests, the CLI and the control plane.
///
/// The key material never appears in `Debug` (the impl shows the key id only) and
/// `ed25519-dalek` zeroizes its secret on drop internally, so a dropped signer leaves
/// no seed bytes behind.
#[derive(Clone)]
pub struct SoftwareSigner {
    /// The signing key.
    key: ed25519_dalek::SigningKey,
    /// The derived key id, fixed at construction (named `id` so it cannot shadow the
    /// trait's `key_id` method).
    id: KeyId,
}

impl SoftwareSigner {
    /// Builds a signer from raw secret-key bytes (exactly 32 of them, the Ed25519
    /// seed).
    ///
    /// # Errors
    /// [`CommandError::Malformed`] when `seed` is not 32 bytes, or the seed does not
    /// expand to a usable key (cannot happen for 32 bytes, but the API cannot promise
    /// that for a caller's byte array).
    pub fn from_seed(seed: &[u8]) -> Result<Self, CommandError> {
        let Ok(array) = <&[u8; 32]>::try_from(seed) else {
            return Err(CommandError::Malformed(
                "the signing seed must be exactly 32 bytes",
            ));
        };
        let key = ed25519_dalek::SigningKey::from_bytes(array);
        Ok(Self::from_key(key))
    }

    /// Builds a signer from a parsed key.
    #[must_use]
    pub fn from_key(key: ed25519_dalek::SigningKey) -> Self {
        Self {
            id: KeyId::from_public_key(&key.verifying_key().to_bytes()),
            key,
        }
    }

    /// The signer's public key bytes, for setting up the verifier's trust list.
    #[must_use]
    pub fn public_key_bytes(&self) -> [u8; PUBLIC_KEY_BYTES] {
        self.key.verifying_key().to_bytes()
    }
}

impl Signer for SoftwareSigner {
    fn key_id(&self) -> KeyId {
        self.id.clone()
    }

    fn sign(&self, msg: &[u8]) -> Signature {
        // Explicit trait call: `ed25519_dalek::SigningKey` has an inherent `sign`, and
        // this impl must route through the `Signer` trait's contract.
        ed25519_dalek::Signer::sign(&self.key, msg)
    }
}

impl fmt::Debug for SoftwareSigner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The key id is a hash of the public key; lengths are sizes. Neither leaks key
        // material.
        f.debug_struct("SoftwareSigner")
            .field("key_id", &self.id.as_str())
            .finish()
    }
}

/// Signs a command with one signer and returns the one-signature envelope.
///
/// The signed bytes are [`DOMAIN_PREFIX`](crate::command::DOMAIN_PREFIX)` ||`
/// [`Command::canonical_bytes`]. The envelope's [`CommandSignature::key_id`] comes from
/// the signer, so envelope and trust list always use the same derivation.
///
/// # Errors
/// [`CommandError::NotCanonical`] when the command contains a float and has no
/// canonical bytes to sign.
pub fn sign(command: &Command, signer: &dyn Signer) -> Result<SignedCommand, CommandError> {
    let message = command.signed_message()?;
    let signature = signer.sign(&message);
    Ok(SignedCommand {
        command: command.clone(),
        signatures: vec![CommandSignature {
            key_id: signer.key_id(),
            signature,
        }],
    })
}

/// One signature over the signed message, named by the key that made it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandSignature {
    /// The id of the key that made this signature.
    pub key_id: KeyId,
    /// The Ed25519 signature bytes.
    pub signature: Signature,
}

/// A command and the signatures over its signed message.
///
/// Built by [`sign`] (one signature) and extended with
/// [`SignedCommand::add_signature`] (co-signing, which WP-ADM-03's two-person rule
/// uses). The verifier demands valid signatures from distinct trusted keys.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedCommand {
    /// The signed content.
    pub command: Command,
    /// The signatures, one per signer; order carries no meaning for verification.
    pub signatures: Vec<CommandSignature>,
}

impl SignedCommand {
    /// Adds a second (or further) signature over the same command. WP-ADM-03's
    /// two-person rule uses this with `min_signatures = 2`; the same key signing twice
    /// adds no verification value (the verifier counts distinct keys).
    ///
    /// # Errors
    /// [`CommandError::NotCanonical`] when the command has no canonical bytes (the
    /// message cannot be built), or [`CommandError::InvalidSignature`] when the signer
    /// refuses to sign.
    pub fn add_signature(&mut self, signer: &dyn Signer) -> Result<(), CommandError> {
        let message = self.command.signed_message()?;
        let signature = signer.sign(&message);
        self.signatures.push(CommandSignature {
            key_id: signer.key_id(),
            signature,
        });
        Ok(())
    }

    /// The full signed message the signatures cover (prefix || canonical bytes).
    ///
    /// # Errors
    /// [`CommandError::NotCanonical`] when the command contains a float.
    pub fn signed_message(&self) -> Result<Vec<u8>, CommandError> {
        self.command.signed_message()
    }
}

/// The verification policy: how fresh, how long, how many signatures.
#[derive(Clone, Debug)]
pub struct VerifyPolicy {
    /// Tolerance for clock disagreement between issuer and verifier, in seconds.
    /// Applied on the `issued_at` side only (the future side); expiry is exact.
    pub max_skew: i64,
    /// The longest validity window accepted, in seconds; the default is 15 minutes.
    pub max_lifetime: i64,
    /// How many valid signatures from **distinct** trusted keys a command needs.
    pub min_signatures: usize,
}

impl VerifyPolicy {
    /// The default policy: 60 seconds of clock skew, a 15-minute lifetime, one
    /// signature.
    #[must_use]
    pub fn default_policy() -> Self {
        Self {
            max_skew: DEFAULT_MAX_SKEW,
            max_lifetime: DEFAULT_MAX_LIFETIME,
            min_signatures: DEFAULT_MIN_SIGNATURES,
        }
    }
}

impl Default for VerifyPolicy {
    fn default() -> Self {
        Self::default_policy()
    }
}

/// The default clock-skew tolerance, 60 seconds.
pub const DEFAULT_MAX_SKEW: i64 = 60;
/// The default maximum command lifetime, 15 minutes (900 seconds).
pub const DEFAULT_MAX_LIFETIME: i64 = 900;
/// The default minimum number of distinct trusted signatures, one.
pub const DEFAULT_MIN_SIGNATURES: usize = 1;

/// The trust anchor: the public keys whose signatures the verifier accepts, with the
/// moment each was revoked (if it was).
///
/// Nothing here knows about hardware, roles or two-person approval; the API to load
/// keys is plain `insert`/`revoke`. A key revoked **before a command's `issued_at`**
/// fails for that command (`cmd.key_revoked`), so a window of commands signed before
/// the revocation instant stay verifiable — the operator picks the cut-off instant.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TrustedKeys {
    /// The keys, by id. The `None` value means "not revoked".
    keys: BTreeMap<KeyId, TrustedKeyEntry>,
}

/// One trusted public key with its optional revocation instant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrustedKeyEntry {
    /// The parsed Ed25519 public key; validated at insertion.
    pub(crate) verifying_key: VerifyingKey,
    /// First instant the key is **no longer** trusted (`None` until revoked).
    pub(crate) revoked_at: Option<i64>,
}

impl TrustedKeys {
    /// An empty set: nothing verifies.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a public key under the given id, deriving nothing: the id is stated, and
    /// the usual convention is [`KeyId::from_public_key`]'s derivation.
    ///
    /// # Errors
    /// [`CommandError::InvalidKey`] when `public_key` is not a valid Ed25519 public
    /// key (wrong length or not a curve point).
    pub fn insert(
        &mut self,
        key_id: KeyId,
        public_key: &[u8; PUBLIC_KEY_BYTES],
    ) -> Result<(), CommandError> {
        let verifying_key =
            VerifyingKey::from_bytes(public_key).map_err(|_| CommandError::InvalidKey)?;
        self.keys.insert(
            key_id,
            TrustedKeyEntry {
                verifying_key,
                revoked_at: None,
            },
        );
        Ok(())
    }

    /// Marks the key revoked from `revoked_at` on (exclusive: at `revoked_at` the key
    /// is already untrusted). The key stays in the map — signatures naming it report
    /// `cmd.key_revoked`, never `cmd.unknown_key`.
    pub fn revoke(&mut self, key_id: &KeyId, revoked_at: i64) {
        if let Some(entry) = self.keys.get_mut(key_id) {
            entry.revoked_at = Some(revoked_at);
        }
    }

    /// The entry for a key id, when present (revoked or not).
    pub(crate) fn entry(&self, key_id: &KeyId) -> Option<&TrustedKeyEntry> {
        self.keys.get(key_id)
    }

    /// How many keys are trusted, revoked ones included.
    #[must_use]
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Whether the set is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

/// What a successful verification returns: the command and the **distinct** key ids of
/// the signatures that verified.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verified {
    /// The verified command.
    pub command: Command,
    /// The ids of the distinct trusted keys that verified, one per signer.
    pub signers: Vec<KeyId>,
}

/// The verifier: trusted keys, policy and the replay window's store, run against
/// commands handed to [`CommandVerifier::verify`].
pub struct CommandVerifier {
    /// The trust anchor.
    trusted: TrustedKeys,
    /// The freshness and signature-count policy.
    policy: VerifyPolicy,
    /// The replay window; `check_and_insert` decides acceptance.
    nonces: Box<dyn crate::nonce::NonceStore>,
}

impl CommandVerifier {
    /// Builds a verifier from its three parts.
    #[must_use]
    pub fn new(
        trusted: TrustedKeys,
        policy: VerifyPolicy,
        nonces: Box<dyn crate::nonce::NonceStore>,
    ) -> Self {
        Self {
            trusted,
            policy,
            nonces,
        }
    }

    /// Verifies a signed command for the agent `expected_audience`, at `now`.
    ///
    /// The checks run in the documented fixed order; the first failure is the error.
    /// The nonce is recorded only when all other checks passed, atomically with the
    /// check, so a rejected command can be corrected and sent again, and an accepted
    /// one can never verify again.
    ///
    /// # Errors
    /// Every [`CommandError`] variant except `InvalidKey` and the constructor-side
    /// `Malformed`s, per the order table in the module docs. A store failure surfaces
    /// the store's own error (`cmd.nonce_store`).
    pub async fn verify(
        &self,
        signed: &SignedCommand,
        expected_audience: &str,
        now: i64,
    ) -> Result<Verified, CommandError> {
        let command = &signed.command;

        // 1. Version.
        if command.version() != crate::command::COMMAND_VERSION {
            return Err(CommandError::UnknownVersion {
                version: command.version(),
            });
        }

        // 2. Canonicity: re-serialize the parsed command and demand the same bytes the
        //    envelope claims were signed. A non-canonical variant is refused before any
        //    key is looked at. `signed_message` builds prefix || canonical; the
        //    signatures must verify over exactly these bytes.
        let message = signed.signed_message()?;

        // 3. Audience: the command must name *this* agent.
        if command.audience() != expected_audience {
            return Err(CommandError::WrongAudience);
        }

        // 4. The validity window, with skew on the issue side.
        if now < command.issued_at() - self.policy.max_skew {
            return Err(CommandError::NotYetValid);
        }
        if now >= command.expires_at() {
            return Err(CommandError::Expired);
        }

        // 5. Lifetime bound: expiry minus issue, at most the policy's maximum. The
        //    constructor forbids inverted windows, so this is `expires_at - issued_at`.
        if command.expires_at() - command.issued_at() > self.policy.max_lifetime {
            return Err(CommandError::LifetimeExceeded);
        }

        // 6. Signature count from distinct trusted keys.
        let mut verified_keys: Vec<KeyId> = Vec::new();
        for signature_entry in &signed.signatures {
            let entry = self.trusted.entry(&signature_entry.key_id);
            let Some(entry) = entry else {
                return Err(CommandError::UnknownKey);
            };
            if let Some(revoked_at) = entry.revoked_at
                && command.issued_at() >= revoked_at
            {
                return Err(CommandError::KeyRevoked);
            }
            let verifies = entry
                .verifying_key
                .verify(&message, &signature_entry.signature)
                .is_ok();
            if !verifies {
                return Err(CommandError::InvalidSignature);
            }
            if !verified_keys.contains(&signature_entry.key_id) {
                verified_keys.push(signature_entry.key_id.clone());
            }
        }
        let required = self.policy.min_signatures;
        if verified_keys.len() < required {
            return Err(CommandError::InsufficientSignatures {
                valid: verified_keys.len(),
                required,
            });
        }

        // 7. Replay: the store is consulted only now. `check_and_insert` records
        //    atomically, so a true return is acceptance, and the nonce is burned for
        //    any later command.
        let accepted = self
            .nonces
            .check_and_insert(
                command.issuer(),
                *command.nonce(),
                command.expires_at(),
                now,
            )
            .await
            .map_err(CommandError::Store)?;
        if !accepted {
            return Err(CommandError::Replay);
        }

        Ok(Verified {
            command: command.clone(),
            signers: verified_keys,
        })
    }
}

impl fmt::Debug for CommandVerifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The trust list and policy are configuration; the store is opaque.
        f.debug_struct("CommandVerifier")
            .field("trusted_keys", &self.trusted.len())
            .field("policy", &self.policy)
            .finish()
    }
}
