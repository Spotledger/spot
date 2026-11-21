//! The envelope: its JSON shape, its associated data, and the AEAD calls.
//!
//! ```text
//! envelope = { "$pii": 1, "subject": <id>, "ct": <base64url>, "nonce": <base64url>,
//!              "aad_hash": <base64url> }
//! aad      = "spot.journal.pii:aad:v1"
//!            u32be(len) entity  entity
//!            u32be(len) id      id
//!            u32be(len) path    path
//!            u32be(len) subject subject
//!            u64be      version
//! ct       = XChaCha20-Poly1305(aad, serde_json::to_vec(value))   (ciphertext || tag)
//! nonce    = 24 bytes from the caller's NonceSource
//! aad_hash = SHA-256(aad)
//! ```
//!
//! The AEAD is the one `spot-secrets` already uses (XChaCha20-Poly1305, 24-byte nonce,
//! base64url without padding), so the platform has one sealing primitive and not two.
//!
//! `aad_hash` is a stored digest of the associated data, not a second tag: the AEAD tag already
//! binds the associated data, but opening needs the key first, and by then the envelope may sit
//! where its subject's key is gone. The digest lets `reveal` tell "this record's field, subject
//! erased" from "this envelope was moved here" without a key, so a moved envelope fails with
//! `journal_pii.envelope_invalid` instead of being reported as erased.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::error::PiiError;
use crate::key::KeyHandle;
use crate::nonce::NONCE_LEN;

/// The version marker member of an envelope.
pub(crate) const MARKER: &str = "$pii";
/// The subject member of an envelope.
pub(crate) const SUBJECT: &str = "subject";
/// The ciphertext member of an envelope.
pub(crate) const CIPHERTEXT: &str = "ct";
/// The nonce member of an envelope.
pub(crate) const NONCE: &str = "nonce";
/// The associated-data digest member of an envelope.
pub(crate) const AAD_HASH: &str = "aad_hash";
/// The version marker of an erasure placeholder.
pub(crate) const ERASED: &str = "$erased";
/// The purpose member of an erasure placeholder.
pub(crate) const PURPOSE: &str = "purpose";
/// The erasure timestamp member of an erasure placeholder (always null here).
pub(crate) const ERASED_AT: &str = "erased_at";

/// The envelope version this build writes and reads.
pub(crate) const ENVELOPE_VERSION: u64 = 1;
/// The associated-data domain prefix.
const AAD_PREFIX: &[u8] = b"spot.journal.pii:aad:v1";
/// The Poly1305 tag length.
const TAG_LEN: usize = 16;
/// The SHA-256 digest length.
const HASH_LEN: usize = 32;

/// Everything an envelope is bound to: the entity, the record, the record version, the concrete
/// field path and the data subject.
pub(crate) struct Aad<'a> {
    /// The entity the record belongs to.
    pub(crate) entity: &'a str,
    /// The record id.
    pub(crate) id: &'a str,
    /// The record version the image was taken at.
    pub(crate) version: u64,
    /// The concrete field path the envelope sits at.
    pub(crate) path: &'a str,
    /// The data subject whose key sealed the value.
    pub(crate) subject: &'a str,
}

impl Aad<'_> {
    /// The associated data, length-prefixed so no component can be shifted into another.
    fn to_bytes(&self) -> Result<Vec<u8>, PiiError> {
        let mut out = Vec::with_capacity(96);
        out.extend_from_slice(AAD_PREFIX);
        for text in [self.entity, self.id, self.path, self.subject] {
            let len = u32::try_from(text.len())
                .map_err(|_| PiiError::ImageInvalid("an identifier is too long to bind"))?;
            out.extend_from_slice(&len.to_be_bytes());
            out.extend_from_slice(text.as_bytes());
        }
        out.extend_from_slice(&self.version.to_be_bytes());
        Ok(out)
    }

    /// The SHA-256 digest of the associated data, as the envelope stores it.
    fn digest(&self) -> Result<[u8; HASH_LEN], PiiError> {
        let bytes = self.to_bytes()?;
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        Ok(hasher.finalize().into())
    }
}

/// True when `value` is an envelope of this crate.
pub(crate) fn is_envelope(value: &Value) -> bool {
    matches!(value, Value::Object(map) if map.contains_key(MARKER))
}

/// Reads an envelope's version and subject, checking the shape every other member must have.
///
/// # Errors
///
/// `journal_pii.envelope_invalid` when a member is missing or of the wrong type, and
/// `journal_pii.envelope_version` when the marker names a version this build does not read.
pub(crate) fn read_header(value: &Value, path: &str) -> Result<String, PiiError> {
    let Value::Object(map) = value else {
        return Err(PiiError::EnvelopeInvalid("the envelope is not an object"));
    };
    match map.get(MARKER) {
        None => {
            return Err(PiiError::EnvelopeInvalid(
                "the envelope has no version marker",
            ));
        }
        Some(marker) => {
            let version = marker.as_u64().ok_or(PiiError::EnvelopeInvalid(
                "the envelope's version marker is not a number",
            ))?;
            if version != ENVELOPE_VERSION {
                return Err(PiiError::EnvelopeVersion {
                    path: path.to_owned(),
                    version,
                });
            }
        }
    }
    let subject = text(map, SUBJECT, "the envelope has no subject")?;
    text(map, CIPHERTEXT, "the envelope has no ciphertext")?;
    text(map, NONCE, "the envelope has no nonce")?;
    text(map, AAD_HASH, "the envelope has no associated-data hash")?;
    Ok(subject.to_owned())
}

/// True when the two subject ids are equal, comparing every byte of equal-length ids without an
/// early exit (the crate has no tag-comparison helper and no `subtle` dependency: the AEAD
/// verifies its own tag, so this small fold is the only comparison written here).
pub(crate) fn same_subject(left: &str, right: &str) -> bool {
    let (left, right) = (left.as_bytes(), right.as_bytes());
    let mut diff = u8::from(left.len() != right.len());
    for (index, byte) in left.iter().enumerate() {
        diff |= byte ^ right.get(index).copied().unwrap_or(0);
    }
    diff == 0
}

/// Checks that the envelope's stored associated-data digest is the one this position implies.
///
/// # Errors
///
/// `journal_pii.envelope_invalid` when the digest is malformed or names another entity, record,
/// version, field or subject.
pub(crate) fn check_aad(value: &Value, aad: &Aad<'_>) -> Result<(), PiiError> {
    let stored = aad_hash(value)?;
    if stored != aad.digest()? {
        return Err(PiiError::EnvelopeInvalid(
            "the associated data does not match where the envelope sits",
        ));
    }
    Ok(())
}

/// Seals `plain` under `key` with `nonce` and the associated data `aad`.
///
/// # Errors
///
/// `journal_pii.envelope_invalid` when the value cannot be serialised or the AEAD refuses.
pub(crate) fn seal(
    key: &KeyHandle,
    aad: &Aad<'_>,
    plain: &Value,
    nonce: [u8; NONCE_LEN],
) -> Result<Value, PiiError> {
    let message = serde_json::to_vec(plain)
        .map_err(|_| PiiError::EnvelopeInvalid("the value cannot be serialised"))?;
    let bytes = aad.to_bytes()?;
    let cipher = XChaCha20Poly1305::new(key.expose().into());
    let body = cipher
        .encrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: &message,
                aad: &bytes,
            },
        )
        .map_err(|_| PiiError::EnvelopeInvalid("the value could not be sealed"))?;
    let mut envelope = Map::new();
    envelope.insert(MARKER.to_owned(), Value::from(ENVELOPE_VERSION));
    envelope.insert(SUBJECT.to_owned(), Value::from(aad.subject));
    envelope.insert(
        CIPHERTEXT.to_owned(),
        Value::from(URL_SAFE_NO_PAD.encode(body)),
    );
    envelope.insert(NONCE.to_owned(), Value::from(URL_SAFE_NO_PAD.encode(nonce)));
    envelope.insert(
        AAD_HASH.to_owned(),
        Value::from(URL_SAFE_NO_PAD.encode(aad.digest()?)),
    );
    Ok(Value::Object(envelope))
}

/// Opens `value` under `key` with the associated data `aad`.
///
/// # Errors
///
/// `journal_pii.envelope_invalid` when a member is malformed, the tag does not verify (a
/// tampered envelope, or one sealed under another key) or the plaintext is not JSON.
pub(crate) fn open(key: &KeyHandle, aad: &Aad<'_>, value: &Value) -> Result<Value, PiiError> {
    let map = as_map(value)?;
    let nonce = decode(map, NONCE)?;
    if nonce.len() != NONCE_LEN {
        return Err(PiiError::EnvelopeInvalid(
            "the envelope's nonce is not 24 bytes",
        ));
    }
    let body = decode(map, CIPHERTEXT)?;
    if body.len() < TAG_LEN {
        return Err(PiiError::EnvelopeInvalid(
            "the envelope's ciphertext is shorter than a tag",
        ));
    }
    let bytes = aad.to_bytes()?;
    let cipher = XChaCha20Poly1305::new(key.expose().into());
    let plain = cipher
        .decrypt(
            XNonce::from_slice(&nonce),
            Payload {
                msg: &body,
                aad: &bytes,
            },
        )
        .map_err(|_| PiiError::EnvelopeInvalid("the envelope does not open where it sits"))?;
    serde_json::from_slice(&plain)
        .map_err(|_| PiiError::EnvelopeInvalid("the envelope's plaintext is not JSON"))
}

/// The documented placeholder a field becomes once its subject's key is destroyed.
///
/// `erased_at` is always null: time is not read here (ADR-061 s6 leaves the timestamp to the
/// erasure workflow, which knows when the request was decided).
pub(crate) fn placeholder(purpose: &str) -> Value {
    let mut map = Map::new();
    map.insert(ERASED.to_owned(), Value::from(1));
    map.insert(PURPOSE.to_owned(), Value::from(purpose));
    map.insert(ERASED_AT.to_owned(), Value::Null);
    Value::Object(map)
}

fn as_map(value: &Value) -> Result<&Map<String, Value>, PiiError> {
    match value {
        Value::Object(map) => Ok(map),
        _ => Err(PiiError::EnvelopeInvalid("the envelope is not an object")),
    }
}

fn text<'a>(
    map: &'a Map<String, Value>,
    member: &str,
    missing: &'static str,
) -> Result<&'a str, PiiError> {
    map.get(member)
        .and_then(Value::as_str)
        .ok_or(PiiError::EnvelopeInvalid(missing))
}

fn decode(map: &Map<String, Value>, member: &str) -> Result<Vec<u8>, PiiError> {
    let encoded = map
        .get(member)
        .and_then(Value::as_str)
        .ok_or(PiiError::EnvelopeInvalid(
            "the envelope's base64 member is missing",
        ))?;
    URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| PiiError::EnvelopeInvalid("the envelope's base64 is not valid"))
}

fn aad_hash(value: &Value) -> Result<[u8; HASH_LEN], PiiError> {
    let map = as_map(value)?;
    let bytes = decode(map, AAD_HASH)?;
    <[u8; HASH_LEN]>::try_from(bytes.as_slice())
        .map_err(|_| PiiError::EnvelopeInvalid("the associated-data hash is not 32 bytes"))
}
