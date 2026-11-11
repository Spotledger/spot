//! The signed revocation list: keys, publishers and bundle digests the store no longer trusts.
//!
//! The list is signed by a store root. Its `generation` strictly increases; a verifier refuses
//! a list older than the one it already holds. A key or publisher revoked with `effective_from`
//! invalidates the signatures made at or after that instant; `compromised` on a key refuses
//! every signature it made, whenever it was made.

use std::collections::BTreeMap;

use spot_bundle::Digest;

use crate::canonical::{self, REVOCATION_DOMAIN};
use crate::keys::{KeyId, KeyPair, SIGNATURE_BYTES};

/// A revoked signing key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevokedKey {
    /// The key.
    pub key_id: KeyId,
    /// Signatures at or after this instant (UTC seconds) are invalid.
    pub effective_from: i64,
    /// Whether the key was compromised: every signature it made is refused.
    pub compromised: bool,
    /// Why, for the admin.
    pub reason: String,
}

/// A revoked publisher.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevokedPublisher {
    /// The publisher.
    pub publisher_id: String,
    /// Signatures at or after this instant (UTC seconds) are invalid.
    pub effective_from: i64,
    /// Why, for the admin.
    pub reason: String,
}

/// A revoked bundle, by its exact digest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevokedBundle {
    /// The digest in lower-case hex.
    pub digest_hex: String,
    /// Why, for the admin.
    pub reason: String,
}

/// The unsigned body of a revocation list. Entries are kept sorted, so the signed bytes do not
/// depend on the order the caller gave them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevocationBody {
    /// Strictly increasing per issued list.
    pub generation: u64,
    /// When the list was issued (UTC seconds).
    pub issued_at: i64,
    /// The root that signs it.
    pub issued_by: KeyId,
    /// Revoked keys, sorted by key id.
    pub keys: Vec<RevokedKey>,
    /// Revoked publishers, sorted by publisher id.
    pub publishers: Vec<RevokedPublisher>,
    /// Revoked bundle digests, sorted.
    pub bundles: Vec<RevokedBundle>,
}

impl RevocationBody {
    /// The domain-separated canonical bytes the root signs.
    #[must_use]
    pub fn signed_bytes(&self) -> Vec<u8> {
        let keys = self
            .keys
            .iter()
            .map(|k| {
                let mut o = BTreeMap::new();
                o.insert("compromised", k.compromised.into());
                o.insert("effective_from", k.effective_from.into());
                o.insert("key_id", k.key_id.as_str().into());
                o.insert("reason", k.reason.as_str().into());
                canonical::object(o)
            })
            .collect::<Vec<_>>();
        let publishers = self
            .publishers
            .iter()
            .map(|p| {
                let mut o = BTreeMap::new();
                o.insert("effective_from", p.effective_from.into());
                o.insert("publisher_id", p.publisher_id.as_str().into());
                o.insert("reason", p.reason.as_str().into());
                canonical::object(o)
            })
            .collect::<Vec<_>>();
        let bundles = self
            .bundles
            .iter()
            .map(|b| {
                let mut o = BTreeMap::new();
                o.insert("digest", b.digest_hex.as_str().into());
                o.insert("reason", b.reason.as_str().into());
                canonical::object(o)
            })
            .collect::<Vec<_>>();
        let mut fields = BTreeMap::new();
        fields.insert("bundles", bundles.into());
        fields.insert("generation", self.generation.into());
        fields.insert("issued_at", self.issued_at.into());
        fields.insert("issued_by", self.issued_by.as_str().into());
        fields.insert("keys", keys.into());
        fields.insert("publishers", publishers.into());
        canonical::encode(REVOCATION_DOMAIN, &fields)
    }
}

/// A revocation list with the root's signature.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevocationList {
    /// The body.
    pub body: RevocationBody,
    pub(crate) signature: [u8; SIGNATURE_BYTES],
}

impl RevocationList {
    /// The signature bytes.
    #[must_use]
    pub fn signature(&self) -> &[u8; SIGNATURE_BYTES] {
        &self.signature
    }

    /// Assembles a list from its parts without checking the signature. Only verification
    /// checks it.
    #[must_use]
    pub fn from_parts(body: RevocationBody, signature: [u8; SIGNATURE_BYTES]) -> Self {
        Self { body, signature }
    }
}

/// Issues a revocation list. `root` signs it; the caller names the root in `body.issued_by`.
/// Entries are sorted here, so equal content always signs the same bytes.
#[must_use]
pub fn issue_revocation_list(mut body: RevocationBody, root: &KeyPair) -> RevocationList {
    body.keys.sort_by(|a, b| a.key_id.cmp(&b.key_id));
    body.publishers
        .sort_by(|a, b| a.publisher_id.cmp(&b.publisher_id));
    body.bundles.sort_by(|a, b| a.digest_hex.cmp(&b.digest_hex));
    let signature = root.sign(&body.signed_bytes());
    RevocationList { body, signature }
}

/// The hex text of a bundle digest, as the revocation list stores it.
#[must_use]
pub fn digest_hex(digest: &Digest) -> String {
    digest.to_hex()
}
