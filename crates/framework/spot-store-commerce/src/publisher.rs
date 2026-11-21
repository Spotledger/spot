//! Publishers: onboarding, verification and signing-key registration.
//!
//! A publisher is an organisation that ships apps into the store. Onboarding creates it in
//! [`VerificationState::Pending`]; a human verification step moves it to
//! [`VerificationState::Verified`], and only a verified, non-[`PublisherStatus::Suspended`]
//! publisher may register signing keys, submit versions or set pricing.
//!
//! Signing keys are registered **through `spot-bundle-signing`**: a key is a
//! [`PublicKey`] validated there (32 bytes that are a curve point) and named by the same
//! [`KeyId`] the bundle verifier derives, so one key id means one key across the store.
//! The private half never reaches this crate.

use std::collections::BTreeMap;
use std::sync::Mutex;

use spot_bundle_signing::{KeyId, PublicKey};

use crate::error::{Result, StoreError, codes};

/// Where a publisher is in onboarding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerificationState {
    /// Onboarded but not yet verified; may not publish.
    Pending,
    /// A human verified the publisher; publishing is allowed.
    Verified,
    /// Verification was refused; publishing stays blocked.
    Rejected,
}

/// Administrative state, independent of verification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublisherStatus {
    /// Publishing is allowed (subject to verification).
    Active,
    /// Administratively suspended; existing installs are not silently removed.
    Suspended,
}

/// One registered signing key: a `spot-bundle-signing` key id and its public key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SigningKeyRef {
    /// The key id `spot-bundle-signing` derives (first 16 bytes of SHA-256, hex).
    pub key_id: KeyId,
    /// The validated Ed25519 public key.
    pub public_key: PublicKey,
    /// When the key was registered (UTC seconds).
    pub registered_at: i64,
    /// When the key was revoked, if it was (UTC seconds).
    pub revoked_at: Option<i64>,
}

impl SigningKeyRef {
    /// Whether the key is still usable at `now`.
    #[must_use]
    pub fn usable_at(&self, now: i64) -> bool {
        self.revoked_at.is_none_or(|at| now < at)
    }
}

/// A publisher account: identity, onboarding state and registered signing keys.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Publisher {
    /// The stable publisher id.
    pub id: String,
    /// The organisation's display name.
    pub org: String,
    /// Administrative state.
    pub status: PublisherStatus,
    /// Onboarding state.
    pub verification: VerificationState,
    /// The store's reference to the publisher's payout account; opaque to this crate.
    pub payout_ref: String,
    /// The publisher's registered signing keys, in registration order.
    pub signing_keys: Vec<SigningKeyRef>,
    /// When the publisher was onboarded (UTC seconds).
    pub created_at: i64,
    /// When the publisher was verified, if it was (UTC seconds).
    pub verified_at: Option<i64>,
}

impl Publisher {
    /// Whether the publisher may publish at `now`: verified, active and holding at least
    /// one usable signing key.
    #[must_use]
    pub fn may_publish(&self, now: i64) -> bool {
        self.verification == VerificationState::Verified
            && self.status == PublisherStatus::Active
            && self.signing_keys.iter().any(|key| key.usable_at(now))
    }

    /// The registered key with the given id, usable or not.
    #[must_use]
    pub fn signing_key(&self, key_id: &KeyId) -> Option<&SigningKeyRef> {
        self.signing_keys.iter().find(|key| &key.key_id == key_id)
    }
}

/// The store's publisher accounts.
#[derive(Debug, Default)]
pub struct PublisherRegistry {
    publishers: Mutex<BTreeMap<String, Publisher>>,
}

impl PublisherRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Publisher>> {
        self.publishers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Onboards a publisher in [`VerificationState::Pending`].
    ///
    /// # Errors
    /// [`codes::INVALID_INPUT`] for a blank id or org, or an id already onboarded.
    pub fn onboard(&self, id: &str, org: &str, payout_ref: &str, now: i64) -> Result<Publisher> {
        check_identifier(id, "publisher id")?;
        if org.trim().is_empty() {
            return Err(StoreError::invalid("publisher org"));
        }
        let publisher = Publisher {
            id: id.to_owned(),
            org: org.to_owned(),
            status: PublisherStatus::Active,
            verification: VerificationState::Pending,
            payout_ref: payout_ref.to_owned(),
            signing_keys: Vec::new(),
            created_at: now,
            verified_at: None,
        };
        let mut publishers = self.lock();
        if publishers.contains_key(id) {
            return Err(StoreError::new(
                codes::INVALID_INPUT,
                "the publisher id is already onboarded",
            ));
        }
        publishers.insert(id.to_owned(), publisher.clone());
        Ok(publisher)
    }

    /// The publisher with the given id.
    ///
    /// # Errors
    /// [`codes::PUBLISHER_UNKNOWN`] when there is no such publisher.
    pub fn get(&self, id: &str) -> Result<Publisher> {
        self.lock()
            .get(id)
            .cloned()
            .ok_or_else(|| StoreError::missing(codes::PUBLISHER_UNKNOWN, "the publisher"))
    }

    /// Verifies a pending publisher.
    ///
    /// # Errors
    /// [`codes::PUBLISHER_UNKNOWN`], or [`codes::INVALID_INPUT`] when the publisher is not
    /// pending.
    pub fn verify(&self, id: &str, now: i64) -> Result<Publisher> {
        self.transition(id, |publisher| {
            if publisher.verification != VerificationState::Pending {
                return Err(StoreError::new(
                    codes::INVALID_INPUT,
                    "the publisher is not pending verification",
                ));
            }
            publisher.verification = VerificationState::Verified;
            publisher.verified_at = Some(now);
            Ok(())
        })
    }

    /// Refuses a pending publisher's verification.
    ///
    /// # Errors
    /// [`codes::PUBLISHER_UNKNOWN`], or [`codes::INVALID_INPUT`] when not pending.
    pub fn reject(&self, id: &str) -> Result<Publisher> {
        self.transition(id, |publisher| {
            if publisher.verification != VerificationState::Pending {
                return Err(StoreError::new(
                    codes::INVALID_INPUT,
                    "the publisher is not pending verification",
                ));
            }
            publisher.verification = VerificationState::Rejected;
            Ok(())
        })
    }

    /// Suspends a publisher.
    ///
    /// # Errors
    /// [`codes::PUBLISHER_UNKNOWN`].
    pub fn suspend(&self, id: &str) -> Result<Publisher> {
        self.transition(id, |publisher| {
            publisher.status = PublisherStatus::Suspended;
            Ok(())
        })
    }

    /// Reinstates a suspended publisher.
    ///
    /// # Errors
    /// [`codes::PUBLISHER_UNKNOWN`].
    pub fn reinstate(&self, id: &str) -> Result<Publisher> {
        self.transition(id, |publisher| {
            publisher.status = PublisherStatus::Active;
            Ok(())
        })
    }

    fn transition(
        &self,
        id: &str,
        apply: impl FnOnce(&mut Publisher) -> Result<()>,
    ) -> Result<Publisher> {
        let mut publishers = self.lock();
        let publisher = publishers
            .get_mut(id)
            .ok_or_else(|| StoreError::missing(codes::PUBLISHER_UNKNOWN, "the publisher"))?;
        apply(publisher)?;
        Ok(publisher.clone())
    }

    /// Registers a signing key for a verified publisher.
    ///
    /// The key bytes are validated by `spot-bundle-signing` and named by the same derived
    /// key id the bundle verifier uses.
    ///
    /// # Errors
    /// [`codes::PUBLISHER_UNKNOWN`], [`codes::PUBLISHER_NOT_VERIFIED`],
    /// [`codes::PUBLISHER_SUSPENDED`], [`codes::SIGNING_KEY_INVALID`] for bytes that are
    /// not a valid Ed25519 public key, or [`codes::SIGNING_KEY_DUPLICATE`].
    pub fn register_signing_key(
        &self,
        id: &str,
        public_key: &[u8],
        now: i64,
    ) -> Result<SigningKeyRef> {
        let public_key = PublicKey::from_bytes(public_key).map_err(|_| {
            StoreError::new(
                codes::SIGNING_KEY_INVALID,
                "the signing key is not a valid Ed25519 public key",
            )
        })?;
        let key_id = public_key.key_id();
        let mut publishers = self.lock();
        let publisher = publishers
            .get_mut(id)
            .ok_or_else(|| StoreError::missing(codes::PUBLISHER_UNKNOWN, "the publisher"))?;
        if publisher.verification != VerificationState::Verified {
            return Err(StoreError::new(
                codes::PUBLISHER_NOT_VERIFIED,
                "the publisher has not been verified",
            ));
        }
        if publisher.status != PublisherStatus::Active {
            return Err(StoreError::new(
                codes::PUBLISHER_SUSPENDED,
                "the publisher is suspended",
            ));
        }
        if publisher.signing_key(&key_id).is_some() {
            return Err(StoreError::new(
                codes::SIGNING_KEY_DUPLICATE,
                "the signing key is already registered",
            ));
        }
        let reference = SigningKeyRef {
            key_id,
            public_key,
            registered_at: now,
            revoked_at: None,
        };
        publisher.signing_keys.push(reference.clone());
        Ok(reference)
    }

    /// Revokes a registered signing key from `now` on.
    ///
    /// # Errors
    /// [`codes::PUBLISHER_UNKNOWN`], or [`codes::NOT_FOUND`] when the key is unknown.
    pub fn revoke_signing_key(&self, id: &str, key_id: &KeyId, now: i64) -> Result<SigningKeyRef> {
        let mut publishers = self.lock();
        let publisher = publishers
            .get_mut(id)
            .ok_or_else(|| StoreError::missing(codes::PUBLISHER_UNKNOWN, "the publisher"))?;
        let key = publisher
            .signing_keys
            .iter_mut()
            .find(|key| &key.key_id == key_id)
            .ok_or_else(|| StoreError::missing(codes::NOT_FOUND, "the signing key"))?;
        key.revoked_at = Some(now);
        Ok(key.clone())
    }

    /// The verified, active publisher, or the refusal that stops publishing.
    ///
    /// # Errors
    /// [`codes::PUBLISHER_UNKNOWN`], [`codes::PUBLISHER_NOT_VERIFIED`] or
    /// [`codes::PUBLISHER_SUSPENDED`].
    pub fn require_active(&self, id: &str) -> Result<Publisher> {
        let publisher = self.get(id)?;
        if publisher.verification != VerificationState::Verified {
            return Err(StoreError::new(
                codes::PUBLISHER_NOT_VERIFIED,
                "the publisher has not been verified",
            ));
        }
        if publisher.status != PublisherStatus::Active {
            return Err(StoreError::new(
                codes::PUBLISHER_SUSPENDED,
                "the publisher is suspended",
            ));
        }
        Ok(publisher)
    }

    /// Every publisher, in id order.
    #[must_use]
    pub fn all(&self) -> Vec<Publisher> {
        self.lock().values().cloned().collect()
    }
}

/// Validates a store identifier: 1..=128 bytes of ASCII letters, digits, `-`, `_` and `.`,
/// not starting with punctuation.
pub(crate) fn check_identifier(text: &str, what: &str) -> Result<()> {
    let well_formed = !text.is_empty()
        && text.len() <= 128
        && !text.starts_with(['-', '_', '.'])
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.');
    if well_formed {
        Ok(())
    } else {
        Err(StoreError::invalid(what))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{PublisherRegistry, PublisherStatus, VerificationState};
    use crate::error::codes;

    /// The 32 bytes of the well-known Ed25519 all-`7` test key; the public key of a fixed
    /// seed, so it is a valid curve point and the same id everywhere.
    fn key_bytes(seed_byte: u8) -> [u8; 32] {
        let mut seed = [seed_byte; 32];
        seed[0] = 1;
        ed25519_dalek::SigningKey::from_bytes(&seed)
            .verifying_key()
            .to_bytes()
    }

    #[test]
    fn onboarding_moves_through_verification() {
        let registry = PublisherRegistry::new();
        let publisher = registry
            .onboard("acme", "Acme Ltd", "payout-1", 100)
            .unwrap();
        assert_eq!(publisher.verification, VerificationState::Pending);
        assert_eq!(
            registry.require_active("acme").unwrap_err().code(),
            codes::PUBLISHER_NOT_VERIFIED
        );
        registry.verify("acme", 200).unwrap();
        assert!(registry.require_active("acme").is_ok());
        assert_eq!(registry.get("acme").unwrap().verified_at, Some(200));
    }

    #[test]
    fn only_a_verified_active_publisher_registers_keys() {
        let registry = PublisherRegistry::new();
        registry
            .onboard("acme", "Acme Ltd", "payout-1", 100)
            .unwrap();
        assert_eq!(
            registry
                .register_signing_key("acme", &key_bytes(7), 150)
                .unwrap_err()
                .code(),
            codes::PUBLISHER_NOT_VERIFIED
        );
        registry.verify("acme", 200).unwrap();
        let key = registry
            .register_signing_key("acme", &key_bytes(7), 250)
            .unwrap();
        assert_eq!(
            registry
                .register_signing_key("acme", &key_bytes(7), 251)
                .unwrap_err()
                .code(),
            codes::SIGNING_KEY_DUPLICATE
        );
        registry.suspend("acme").unwrap();
        assert_eq!(
            registry.get("acme").unwrap().status,
            PublisherStatus::Suspended
        );
        assert_eq!(
            registry
                .register_signing_key("acme", &key_bytes(9), 260)
                .unwrap_err()
                .code(),
            codes::PUBLISHER_SUSPENDED
        );
        registry.reinstate("acme").unwrap();
        assert!(registry.get("acme").unwrap().may_publish(300));
        assert!(key.usable_at(300));
    }

    #[test]
    fn invalid_key_bytes_are_refused() {
        let registry = PublisherRegistry::new();
        registry
            .onboard("acme", "Acme Ltd", "payout-1", 100)
            .unwrap();
        registry.verify("acme", 200).unwrap();
        assert_eq!(
            registry
                .register_signing_key("acme", &[0u8; 31], 250)
                .unwrap_err()
                .code(),
            codes::SIGNING_KEY_INVALID
        );
    }

    #[test]
    fn a_revoked_key_is_no_longer_usable() {
        let registry = PublisherRegistry::new();
        registry
            .onboard("acme", "Acme Ltd", "payout-1", 100)
            .unwrap();
        registry.verify("acme", 200).unwrap();
        let key = registry
            .register_signing_key("acme", &key_bytes(7), 250)
            .unwrap();
        registry
            .revoke_signing_key("acme", &key.key_id, 400)
            .unwrap();
        let publisher = registry.get("acme").unwrap();
        assert!(!publisher.may_publish(500));
        assert!(!publisher.signing_key(&key.key_id).unwrap().usable_at(500));
    }
}
