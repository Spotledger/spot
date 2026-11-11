//! The update trust anchor, the signed revocation list, and the detached signature document.
//!
//! The update signing keys are their own anchor (never the store's and never the licence's):
//! [`UpdateTrust`] holds the keys the build trusts, each with a validity window so a key can
//! rotate. A key that is compromised or retired is placed on the signed revocation list
//! ([`UpdateRevocation`]), whose `generation` strictly increases; a verifier refuses a list older
//! than the one it already holds.
//!
//! The shapes mirror `spot-bundle-signing`: a key id is a SHA-256 prefix in hex, signatures are
//! raw 64-byte Ed25519, and each document signs under its own domain prefix.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;

use crate::canonical::{self, UPDATE_REVOCATION_DOMAIN};
use crate::error::UpdaterError;
use crate::keys::{KeyId, PublicKey, SIGNATURE_BYTES};

/// One trusted update key and its window. `valid_to` of `None` means no end.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrustedUpdateKey {
    key_id: KeyId,
    public: PublicKey,
    /// First instant (UTC seconds) the key is trusted for.
    pub valid_from: i64,
    /// First instant the key is no longer trusted; `None` for no end.
    pub valid_to: Option<i64>,
}

impl TrustedUpdateKey {
    /// A trusted key for `public`, valid over the given window.
    #[must_use]
    pub fn new(public: PublicKey, valid_from: i64, valid_to: Option<i64>) -> Self {
        Self {
            key_id: public.key_id(),
            public,
            valid_from,
            valid_to,
        }
    }

    /// The key's id.
    #[must_use]
    pub fn key_id(&self) -> &KeyId {
        &self.key_id
    }

    /// The key's public half.
    #[must_use]
    pub fn public(&self) -> PublicKey {
        self.public
    }

    /// Whether the key is trusted at `now`.
    #[must_use]
    pub fn valid_at(&self, now: i64) -> bool {
        self.valid_from <= now && self.valid_to.is_none_or(|end| now < end)
    }
}

/// The update keys a build trusts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UpdateTrust {
    keys: Vec<TrustedUpdateKey>,
}

impl UpdateTrust {
    /// An anchor over `keys`.
    #[must_use]
    pub fn new(keys: Vec<TrustedUpdateKey>) -> Self {
        Self { keys }
    }

    /// The keys, in the order they were given.
    #[must_use]
    pub fn keys(&self) -> &[TrustedUpdateKey] {
        &self.keys
    }

    /// The key with this id, if the anchor trusts it at all.
    #[must_use]
    pub fn key(&self, id: &KeyId) -> Option<&TrustedUpdateKey> {
        self.keys.iter().find(|key| key.key_id() == id)
    }
}

/// A revoked update key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevokedUpdateKey {
    /// The revoked key.
    pub key_id: KeyId,
    /// Why, for the administrator.
    pub reason: String,
}

/// The unsigned body of a revocation list. Entries are kept sorted, so the signed bytes do not
/// depend on the order the caller gave them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateRevocationBody {
    /// Strictly increasing per issued list.
    pub generation: u64,
    /// When the list was issued (UTC seconds).
    pub issued_at: i64,
    /// The anchor key that signs it.
    pub issued_by: KeyId,
    /// Revoked keys, sorted by key id.
    pub keys: Vec<RevokedUpdateKey>,
}

impl UpdateRevocationBody {
    /// The domain-separated canonical bytes the issuer signs.
    #[must_use]
    pub fn signed_bytes(&self) -> Vec<u8> {
        let keys = self
            .keys
            .iter()
            .map(|key| {
                let mut fields = BTreeMap::new();
                fields.insert("key_id", Value::from(key.key_id.as_str()));
                fields.insert("reason", Value::from(key.reason.as_str()));
                canonical::object(fields)
            })
            .collect::<Vec<_>>();
        let mut fields = BTreeMap::new();
        fields.insert("generation", Value::from(self.generation));
        fields.insert("issued_at", Value::from(self.issued_at));
        fields.insert("issued_by", Value::from(self.issued_by.as_str()));
        fields.insert("keys", Value::from(keys));
        canonical::encode(UPDATE_REVOCATION_DOMAIN, &fields)
    }
}

/// A revocation list with its issuer's signature.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateRevocation {
    /// The body.
    pub body: UpdateRevocationBody,
    signature: [u8; SIGNATURE_BYTES],
}

impl UpdateRevocation {
    /// The signature bytes.
    #[must_use]
    pub fn signature(&self) -> &[u8; SIGNATURE_BYTES] {
        &self.signature
    }

    /// Assembles a list from its parts without checking the signature. Only verification checks
    /// it.
    #[must_use]
    pub fn from_parts(body: UpdateRevocationBody, signature: [u8; SIGNATURE_BYTES]) -> Self {
        Self { body, signature }
    }

    /// Issues a list: sorts the entries, then signs the canonical bytes with `issuer`'s secret.
    /// The caller names the issuer in `body.issued_by`, and must pass the key that matches.
    #[must_use]
    pub fn issue(mut body: UpdateRevocationBody, issuer: &SigningKey) -> Self {
        body.keys.sort_by(|a, b| a.key_id.cmp(&b.key_id));
        let signature = issuer.sign(&body.signed_bytes());
        Self { body, signature }
    }
}

/// The `format` field of every `update.sig` document.
pub const SIGNATURE_FORMAT: &str = "spot-update-signature";

/// The detached update signature (`update.sig`): the id of the key that signed, and the raw
/// Ed25519 signature over [`UpdateManifest::signing_message`](crate::UpdateManifest::signing_message).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateSignature {
    key_id: KeyId,
    signature: [u8; SIGNATURE_BYTES],
}

/// The raw TOML shape of an `update.sig` document.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSignature {
    format: String,
    key_id: String,
    signature: String,
}

impl UpdateSignature {
    /// Assembles a signature from its parts. Verification checks it.
    #[must_use]
    pub fn from_parts(key_id: KeyId, signature: [u8; SIGNATURE_BYTES]) -> Self {
        Self { key_id, signature }
    }

    /// The id of the key that signed.
    #[must_use]
    pub fn key_id(&self) -> &KeyId {
        &self.key_id
    }

    /// The raw signature bytes.
    #[must_use]
    pub fn signature(&self) -> &[u8; SIGNATURE_BYTES] {
        &self.signature
    }

    /// Parses an `update.sig` document.
    ///
    /// # Errors
    /// [`UpdaterError::SignatureMalformed`] when the TOML does not parse, an unknown field is
    /// present, the `format` is not [`SIGNATURE_FORMAT`], the key id is malformed, or the
    /// signature is not 64 bytes of standard base64.
    pub fn from_toml(text: &str) -> Result<Self, UpdaterError> {
        let raw: RawSignature =
            toml::from_str(text).map_err(|error| UpdaterError::SignatureMalformed {
                detail: error.to_string(),
            })?;
        if raw.format != SIGNATURE_FORMAT {
            return Err(UpdaterError::SignatureMalformed {
                detail: format!("the format is `{}`, not `{SIGNATURE_FORMAT}`", raw.format),
            });
        }
        let key_id = KeyId::parse(&raw.key_id).map_err(|_| UpdaterError::SignatureMalformed {
            detail: "the key id is malformed".to_owned(),
        })?;
        let bytes =
            canonical::unb64(&raw.signature).ok_or_else(|| UpdaterError::SignatureMalformed {
                detail: "the signature is not standard base64".to_owned(),
            })?;
        let signature = <[u8; SIGNATURE_BYTES]>::try_from(bytes.as_slice()).map_err(|_| {
            UpdaterError::SignatureMalformed {
                detail: format!("a signature is {SIGNATURE_BYTES} bytes"),
            }
        })?;
        Ok(Self { key_id, signature })
    }
}

/// A minimal Ed25519 signing key, for the control plane and tests. The seed is drawn by the
/// caller; this crate reads no randomness.
///
/// The secret never leaves the control plane: only the public half travels in an anchor.
pub struct SigningKey(ed25519_dalek::SigningKey);

impl SigningKey {
    /// A key from 32 seed bytes the caller drew from a cryptographic source.
    #[must_use]
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        Self(ed25519_dalek::SigningKey::from_bytes(seed))
    }

    /// The public half.
    #[must_use]
    pub fn public(&self) -> PublicKey {
        PublicKey::from_bytes(&self.0.verifying_key().to_bytes())
            .unwrap_or_else(|_| unreachable!("a derived verifying key is always a point"))
    }

    /// Signs `message`, returning the raw 64 bytes.
    #[must_use]
    pub fn sign(&self, message: &[u8]) -> [u8; SIGNATURE_BYTES] {
        use ed25519_dalek::Signer as _;
        self.0.sign(message).to_bytes()
    }
}

impl std::fmt::Debug for SigningKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "SigningKey({}, secret: <redacted>)",
            self.public().key_id()
        )
    }
}
