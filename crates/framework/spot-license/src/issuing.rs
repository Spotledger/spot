//! The issuing helper, behind the `issue` cargo feature: how the control plane mints
//! licences.
//!
//! Never compiled into the product binary: the product only ever verifies
//! ([`crate::envelope::verify`]). Keys are **always passed in** to
//! [`Issuer::sign`] — nothing here generates, stores or embeds a private key, and the
//! tests that use this module create a throwaway [`SigningKey`] from a fixed seed.
//!
//! The feature keeps the signing code out of the runtime's dependency graph in spirit
//! too: the code is inert without `--features issue`, and the product's binary never
//! enables it.

use ed25519_dalek::{Signature, Signer, SigningKey};

use crate::envelope::SignedLicence;
use crate::error::LicenceError;
use crate::model::Licence;

/// The signing side of the issuer: a key wrapper that turns a validated licence into a
/// [`SignedLicence`].
///
/// Constructed from the [`SigningKey`] the caller holds; this crate neither generates
/// keys nor keeps them. The key id travels with the key so envelopes can be minted
/// without the caller re-stating it in every call.
pub struct Issuer {
    signing_key: SigningKey,
    key_id: String,
}

impl Issuer {
    /// An issuer over `signing_key`, minting envelopes under `key_id`. The id must
    /// match the `issuer_key_id` the licence payloads it signs carry (checked in
    /// [`Issuer::sign`]).
    #[must_use]
    pub fn new(signing_key: SigningKey, key_id: impl Into<String>) -> Self {
        Self {
            signing_key,
            key_id: key_id.into(),
        }
    }

    /// Signs a validated licence: canonical JSON bytes, Ed25519 over those exact bytes,
    /// and the issuer key id in both the envelope and the payload.
    ///
    /// # Errors
    /// [`LicenceError::KeyMismatch`] when the licence's `issuer_key_id` differs from the
    /// issuer's own id; [`LicenceError::Json`] when the licence cannot be serialised
    /// (a validated licence always can, so this names a bug, not bad input).
    pub fn sign(&self, licence: &Licence) -> Result<SignedLicence, LicenceError> {
        if licence.issuer_key_id != self.key_id {
            return Err(LicenceError::KeyMismatch);
        }
        let payload = licence.to_canonical_bytes()?;
        let signature: Signature = self.signing_key.sign(&payload);
        SignedLicence::from_parts(payload, signature.to_bytes().to_vec(), self.key_id.clone())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::Issuer;
    use crate::envelope::{TrustedKey, TrustedKeys, verify};
    use crate::model::{
        DEFAULT_GRACE, Entitlements, Kind, Licence, Limit, PAYLOAD_VERSION, RawLicence,
    };

    /// A throwaway signing key from a fixed seed: 32 arbitrary bytes, here the
    /// well-known all-`7` seed. No key material of any real issuer ever touches tests.
    fn throwaway_key(seed_byte: u8) -> ed25519_dalek::SigningKey {
        let mut seed = [seed_byte; 32];
        seed[0] = 1; // not all-equal, exercise a bit of entropy
        ed25519_dalek::SigningKey::from_bytes(&seed)
    }

    fn licence(key_id: &str, serial: u64) -> Licence {
        Licence::new(RawLicence {
            v: PAYLOAD_VERSION,
            licence_id: "0198c7a2-license-id".to_owned(),
            tenant: Some("acme".to_owned()),
            installation_id: None,
            edition: crate::model::test_support::edition(),
            ent: Entitlements::from_lists(Some(Limit::UpTo(25)), vec![], vec![]),
            kind: Kind::Subscription,
            issued_at: 1_770_000_000,
            not_before: 1_770_000_000,
            expires_at: Some(1_772_683_200),
            renew_after: None,
            grace: DEFAULT_GRACE.as_secs(),
            serial,
            key_id: key_id.to_owned(),
        })
        .unwrap_or_else(|error| panic!("{error:?}"))
    }

    #[test]
    fn sign_and_verify_round_trip() {
        let signing_key = throwaway_key(7);
        let issuer = Issuer::new(signing_key, "issuer-2026-q3");
        let licence = licence("issuer-2026-q3", 1);
        let signed = issuer
            .sign(&licence)
            .unwrap_or_else(|error| panic!("{error:?}"));
        let trusted = TrustedKeys::new(vec![
            TrustedKey::from_bytes(
                "issuer-2026-q3",
                &issuer.signing_key.verifying_key().to_bytes(),
                0,
                None,
            )
            .unwrap_or_else(|error| panic!("{error:?}")),
        ]);
        let verified = verify(&signed, &trusted).unwrap_or_else(|error| panic!("{error:?}"));
        assert_eq!(verified, licence);
    }

    #[test]
    fn key_id_mismatch_is_refused() {
        let signing_key = throwaway_key(9);
        let issuer = Issuer::new(signing_key, "issuer-2026-q3");
        assert_eq!(
            issuer.sign(&licence("other-issuer", 1)).unwrap_err().code(),
            "licence.key_mismatch"
        );
    }
}
