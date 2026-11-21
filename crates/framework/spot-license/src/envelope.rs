//! The signed envelope: the bytes on disk and the offline verification against trusted keys.
//!
//! # The bytes
//!
//! A [`SignedLicence`] is `payload` + `signature` + `key_id`:
//!
//! - the **payload** is [`Licence::to_canonical_bytes`](crate::model::Licence::to_canonical_bytes)
//!   — canonical JSON, one exact byte sequence per licence;
//! - the **signature** is a raw, 64-byte Ed25519 signature over those payload bytes;
//! - the **key_id** names the issuer key, and must equal the payload's `issuer_key_id`.
//!
//! The text form of a licence file (the thing a customer double-clicks or pastes into the
//! activation screen) is
//!
//! ```text
//! SPOT-LICENCE-1.<base64url payload>.<base64url signature>
//! ```
//!
//! Base64url without padding, the same alphabet the opaque tokens of this codebase use. The
//! key id does **not** travel in the text form: it is inside the signed payload
//! (`issuer_key_id`), so the verifier reads it from the payload after decoding and the text
//! form has nothing to spoof.
//!
//! # Verification
//!
//! [`verify`] checks, in order: the payload's format version is known, the payload parses and
//! **re-serialises byte-identically** (canonicity), the named issuer key is trusted with a
//! validity window covering the payload's `issued_at`, the envelope's id agrees with the
//! payload's, and the signature verifies. Every failure is a
//! [`LicenceError`] with a stable code; no malformed input can panic (property-tested).

use std::fmt;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::{Engine, engine::general_purpose};
use ed25519_dalek::{Signature, Verifier, VerifyingKey};

use crate::error::LicenceError;
use crate::model::Licence;

/// The fixed prefix of the licence text form. Bumping it is how the format signals a
/// breaking change to parsers and file managers.
pub const LICENCE_TEXT_PREFIX: &str = "SPOT-LICENCE-1";

/// The length of a raw Ed25519 signature in bytes.
pub(crate) const SIGNATURE_BYTES: usize = 64;

/// The length of a raw Ed25519 public key in bytes.
pub(crate) const PUBLIC_KEY_BYTES: usize = 32;

/// A signed licence: the canonical payload bytes, the Ed25519 signature over them and the
/// issuer key id. Build one with the `issue` feature's
/// [`Issuer::sign`](crate::issuing::Issuer::sign), or by parsing the text form with
/// [`SignedLicence::from_text`].
#[derive(Clone, PartialEq, Eq)]
pub struct SignedLicence {
    /// The canonical payload bytes; verification re-serialises the parsed licence and
    /// demands byte-equality against these.
    payload: Vec<u8>,
    /// The raw Ed25519 signature over `payload`, exactly [`SIGNATURE_BYTES`] bytes.
    signature: Vec<u8>,
    /// The issuer key id the signature was made with. Kept in the struct (not only in the
    /// payload) so the text form round trip is lossless and diagnostics name the key
    /// before any payload parsing happens.
    key_id: String,
}

impl SignedLicence {
    /// Builds an envelope from its three parts, with the length and cross-field checks:
    /// the signature must be exactly [`SIGNATURE_BYTES`] bytes and the envelope's `key_id`
    /// must equal the payload's `issuer_key_id`.
    ///
    /// # Errors
    /// [`LicenceError::SignatureMalformed`] for a signature of the wrong length,
    /// [`LicenceError::KeyMismatch`] when the ids disagree. This constructor does **not**
    /// verify the signature; [`verify`] does.
    pub fn from_parts(
        payload: Vec<u8>,
        signature: Vec<u8>,
        key_id: impl Into<String>,
    ) -> Result<Self, LicenceError> {
        if signature.len() != SIGNATURE_BYTES {
            return Err(LicenceError::SignatureMalformed);
        }
        let key_id = key_id.into();
        let parsed = payload_as_licence(&payload)?;
        if parsed.issuer_key_id != key_id {
            return Err(LicenceError::KeyMismatch);
        }
        Ok(Self {
            payload,
            signature,
            key_id,
        })
    }

    /// The canonical payload bytes.
    #[must_use]
    pub fn payload_bytes(&self) -> &[u8] {
        &self.payload
    }

    /// The raw signature bytes, exactly [`SIGNATURE_BYTES`] of them.
    #[must_use]
    pub fn signature_bytes(&self) -> &[u8] {
        &self.signature
    }

    /// The issuer key id the envelope names.
    #[must_use]
    pub fn key_id(&self) -> &str {
        &self.key_id
    }

    /// Encodes the text form `SPOT-LICENCE-1.<base64url payload>.<base64url signature>`,
    /// the string a customer receives and the install accepts.
    ///
    /// # Errors
    /// [`LicenceError::EnvelopeMalformed`] with the serde message when the payload holds
    /// bytes that are not the canonical JSON of a licence, which for a signed licence
    /// built through [`SignedLicence::from_parts`] or [`crate::issuing::Issuer::sign`]
    /// cannot happen.
    pub fn to_text(&self) -> Result<String, LicenceError> {
        // The text form carries only base64url segments; the payload must be the UTF-8
        // canonical JSON the decoder produced it from, which `from_parts` re-checks by
        // parsing.
        Ok(format!(
            "{LICENCE_TEXT_PREFIX}.{}.{}",
            URL_SAFE_NO_PAD.encode(&self.payload),
            URL_SAFE_NO_PAD.encode(&self.signature)
        ))
    }

    /// Parses the text form [`SignedLicence::to_text`] produces.
    ///
    /// Checks the prefix, the two-segment structure and the Base64url shape; **no**
    /// signature check happens here ([`verify`] does that against trusted keys) and no
    /// trust is put in the decoded bytes until then.
    ///
    /// # Errors
    /// [`LicenceError::EnvelopeMalformed`] for everything structurally wrong; the
    /// constructor's [`LicenceError::SignatureMalformed`] and [`LicenceError::KeyMismatch`]
    /// then apply to the decoded parts.
    pub fn from_text(text: &str) -> Result<Self, LicenceError> {
        let mut parts = text.split('.');
        let prefix = parts.next().unwrap_or("");
        let payload_part = parts.next().unwrap_or("");
        let signature_part = parts.next().unwrap_or("");
        if prefix != LICENCE_TEXT_PREFIX
            || parts.next().is_some()
            || payload_part.is_empty()
            || signature_part.is_empty()
        {
            return Err(LicenceError::EnvelopeMalformed);
        }
        let payload = general_purpose::URL_SAFE_NO_PAD
            .decode(payload_part)
            .map_err(|_| LicenceError::EnvelopeMalformed)?;
        let signature = general_purpose::URL_SAFE_NO_PAD
            .decode(signature_part)
            .map_err(|_| LicenceError::EnvelopeMalformed)?;
        let parsed = payload_as_licence(&payload)?;
        Self::from_parts(payload, signature, parsed.issuer_key_id)
    }

    /// Parses the canonical payload as a licence, for callers that want the fields
    /// without a trusted-key check (for example, to display *which* licence a file
    /// claims to be before asking for the key list). The output is untrusted input.
    ///
    /// # Errors
    /// [`LicenceError::Json`] when the payload does not parse as a validated licence.
    pub fn claimed_licence(&self) -> Result<Licence, LicenceError> {
        payload_as_licence(&self.payload)
    }
}

impl fmt::Debug for SignedLicence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The payload and signature are opaque to this type; show only sizes and the key id.
        f.debug_struct("SignedLicence")
            .field("payload_bytes", &self.payload.len())
            .field("signature_bytes", &self.signature.len())
            .field("key_id", &self.key_id)
            .finish()
    }
}

/// Parses canonical payload bytes into a validated [`Licence`], the shared funnel of
/// `from_parts`, `from_text` and `verify`.
fn payload_as_licence(payload: &[u8]) -> Result<Licence, LicenceError> {
    let text = std::str::from_utf8(payload).map_err(|_| LicenceError::EnvelopeMalformed)?;
    Licence::from_json(text)
}

/// One issuer public key in the trusted list, with the lifetime it is trusted for.
///
/// The bytes are a raw Ed25519 public key ([`PUBLIC_KEY_BYTES`] of them). The window is in
/// UTC epoch seconds like every instant in this crate: a key signed licences between
/// `valid_from` and `valid_to`, and a licence whose `issued_at` falls outside that window
/// is refused even though the signature is fine — that is rotation done right, not an
/// accident.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrustedKey {
    /// The issuer's own name for this key; the envelope and the payload's
    /// `issuer_key_id` must name exactly this.
    pub key_id: String,
    /// The raw public key bytes, exactly [`PUBLIC_KEY_BYTES`] of them (checked by
    /// [`TrustedKey::from_bytes`]).
    public_key: [u8; PUBLIC_KEY_BYTES],
    /// First instant the key is trusted for, UTC seconds since the Unix epoch.
    pub valid_from: i64,
    /// First instant the key is **no longer** trusted; `None` for a key with no end.
    pub valid_to: Option<i64>,
}

impl TrustedKey {
    /// Builds a trusted key from raw bytes, checking the length and that the bytes are a
    /// valid curve point — a hostile key list must fail cleanly, not panic.
    ///
    /// # Errors
    /// [`LicenceError::KeyBytesInvalid`] for the wrong length or an invalid point.
    pub fn from_bytes(
        key_id: impl Into<String>,
        public_key: &[u8],
        valid_from: i64,
        valid_to: Option<i64>,
    ) -> Result<Self, LicenceError> {
        let Ok(array) = <&[u8; PUBLIC_KEY_BYTES]>::try_from(public_key) else {
            return Err(LicenceError::KeyBytesInvalid);
        };
        VerifyingKey::from_bytes(array).map_err(|_| LicenceError::KeyBytesInvalid)?;
        Ok(Self {
            key_id: key_id.into(),
            public_key: *array,
            valid_from,
            valid_to,
        })
    }

    /// The parsed Ed25519 public key; the bytes were validated at construction, so only
    /// a broken invariant could error here.
    pub(crate) fn verifying_key(&self) -> Result<VerifyingKey, LicenceError> {
        VerifyingKey::from_bytes(&self.public_key).map_err(|_| LicenceError::KeyBytesInvalid)
    }

    /// Whether this key is trusted at `issued_at` for licensing a payload with this id.
    pub(crate) fn trusted_at(&self, key_id: &str, issued_at: i64) -> bool {
        self.key_id == key_id
            && self.valid_from <= issued_at
            && self.valid_to.is_none_or(|valid_to| issued_at < valid_to)
    }
}

/// The issuer keys this machine trusts, with their rotation windows.
///
/// The list is the whole trust anchor: a licence is verified if **one** trusted key named
/// by the payload verifies the signature. Keys the list has dropped are revoked as far as
/// this machine is concerned — removing a key *is* revocation.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TrustedKeys {
    keys: Vec<TrustedKey>,
}

impl TrustedKeys {
    /// A trust anchor with the given keys.
    #[must_use]
    pub fn new(keys: Vec<TrustedKey>) -> Self {
        Self { keys }
    }

    /// An empty anchor: nothing verifies.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// The trusted keys, in list order.
    #[must_use]
    pub fn keys(&self) -> &[TrustedKey] {
        &self.keys
    }

    /// The key with the given id, when trusted.
    #[must_use]
    pub fn by_id(&self, key_id: &str) -> Option<&TrustedKey> {
        self.keys.iter().find(|key| key.key_id == key_id)
    }
}

/// Verifies a signed licence **offline** against the trusted issuer keys and returns the
/// licence it grants.
///
/// The checks, in order:
///
/// 1. the payload's format version is [`crate::model::PAYLOAD_VERSION`] and the payload
///    parses as a validated licence;
/// 2. the payload is **canonical**: re-serialising the parsed licence reproduces the
///    signed bytes exactly, so no unnormalised variant of the same data was ever signed;
/// 3. the envelope's key id names a trusted key whose window covers the payload's
///    `issued_at` (a key outside its window is [`LicenceError::KeyExpired`], an id the
///    list never had or that was dropped is [`LicenceError::KeyUnknown`]);
/// 4. the Ed25519 signature verifies over the payload bytes.
///
/// # Errors
/// Every variant of [`LicenceError`] that matches one of the checks above, per its docs.
pub fn verify(signed: &SignedLicence, trusted: &TrustedKeys) -> Result<Licence, LicenceError> {
    let licence = payload_as_licence(&signed.payload)?;
    if licence.version != crate::model::PAYLOAD_VERSION {
        return Err(LicenceError::UnknownVersion {
            version: licence.version,
        });
    }
    // Canonicity: the signed bytes must be exactly what this crate would emit. This is
    // what makes step 2 of the tamper model real: a valid signature over a non-canonical
    // byte sequence is refused before any key is looked at.
    let canonical = licence.to_canonical_bytes()?;
    if canonical != signed.payload {
        return Err(LicenceError::NotCanonical);
    }
    // The envelope's id and the payload's must agree (a mismatch is a construction error,
    // caught in `from_parts` too; re-checked here because `verify` accepts any bytes).
    if signed.key_id != licence.issuer_key_id {
        return Err(LicenceError::KeyMismatch);
    }
    // Trust: the named key must be in the list with a window over the issue instant.
    let key = trusted
        .by_id(&signed.key_id)
        .ok_or(LicenceError::KeyUnknown)?;
    if !key.trusted_at(&licence.issuer_key_id, licence.issued_at) {
        return Err(LicenceError::KeyExpired);
    }
    // The signature itself, constant-time inside the crate.
    let signature = signature_of(&signed.signature)?;
    let verifying_key = key.verifying_key()?;
    verifying_key
        .verify(&signed.payload, &signature)
        .map_err(|_| LicenceError::SignatureInvalid)?;
    Ok(licence)
}

/// Builds a `Signature` from raw bytes without panicking on a wrong length.
fn signature_of(bytes: &[u8]) -> Result<Signature, LicenceError> {
    let Ok(array) = <&[u8; SIGNATURE_BYTES]>::try_from(bytes) else {
        return Err(LicenceError::SignatureMalformed);
    };
    // Fallible only through the length, which the conversion above fixed; any 64 bytes
    // are a structural signature (verification decides validity).
    Ok(Signature::from_bytes(array))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{LICENCE_TEXT_PREFIX, SignedLicence, TrustedKey};
    use crate::model::{
        DEFAULT_GRACE, Entitlements, Kind, Licence, Limit, PAYLOAD_VERSION, RawLicence,
    };

    /// The canonical bytes of a valid test licence (this module's constant sample).
    fn sample_bytes() -> Vec<u8> {
        sample_licence()
            .to_canonical_bytes()
            .unwrap_or_else(|error| panic!("{error:?}"))
    }

    /// A plain, valid subscription licence for tests of this module.
    fn sample_licence() -> Licence {
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
            renew_after: Some(1_772_071_200),
            grace: DEFAULT_GRACE.as_secs(),
            serial: 1,
            key_id: "issuer-2026-q3".to_owned(),
        })
        .unwrap_or_else(|error| panic!("{error:?}"))
    }

    #[test]
    fn envelope_checks_parts() {
        let payload = sample_bytes();
        let key_id = sample_licence().issuer_key_id;
        let short_signature = vec![0u8; 63];
        assert_eq!(
            SignedLicence::from_parts(payload.clone(), short_signature, key_id.clone())
                .unwrap_err()
                .code(),
            "licence.signature_malformed"
        );
        assert_eq!(
            SignedLicence::from_parts(payload, vec![0u8; 64], "other-key".to_owned())
                .unwrap_err()
                .code(),
            "licence.key_mismatch"
        );
    }

    #[test]
    fn text_form_round_trips() {
        let payload = sample_bytes();
        let key_id = sample_licence().issuer_key_id;
        let signed = SignedLicence::from_parts(payload, vec![0x42; 64], key_id)
            .unwrap_or_else(|error| panic!("{error:?}"));
        let text = signed.to_text().unwrap_or_else(|error| panic!("{error:?}"));
        assert!(text.starts_with(LICENCE_TEXT_PREFIX));
        let parsed = SignedLicence::from_text(&text).unwrap_or_else(|error| panic!("{error:?}"));
        assert_eq!(parsed, signed);
    }

    #[test]
    fn text_form_rejects_garbage() {
        for text in [
            "",
            "SPOT-LICENCE-1",
            "SPOT-LICENCE-1.",
            "SPOT-LICENCE-1.AAAA",
            "SPOT-LICENCE-1.AAAA.BBBB.CCCC",
            "other-prefix.AAAA.BBBB",
            "SPOT-LICENCE-1.###.BBBB",
            "SPOT-LICENCE-1.AAAA.###",
        ] {
            assert_eq!(
                SignedLicence::from_text(text).unwrap_err().code(),
                "licence.envelope_malformed",
                "{text}"
            );
        }
    }

    #[test]
    fn trusted_key_shape_is_checked() {
        // 31 bytes are the wrong shape outright.
        assert_eq!(
            TrustedKey::from_bytes("k", &[0u8; 31], 0, None)
                .unwrap_err()
                .code(),
            "licence.key_bytes_invalid"
        );
        // All-zero bytes ARE a valid encoding in `ed25519-dalek` 2 (the identity, y = 0
        // decompresses); the shape contract of this crate accepts any well-formed curve
        // encoding, because verification — not construction — decides validity. A key
        // the issuer never signed with simply fails `verify` with `SignatureInvalid`.
        assert!(TrustedKey::from_bytes("k", &[0u8; 32], 0, None).is_ok());
        // 33 bytes are again the wrong shape.
        assert_eq!(
            TrustedKey::from_bytes("k", &[0u8; 33], 0, None)
                .unwrap_err()
                .code(),
            "licence.key_bytes_invalid"
        );
    }
}
