//! Protecting and revealing an image.
//!
//! `protect` replaces each personal field's value with an envelope; `reveal` puts the value
//! back, or puts the documented placeholder there when the subject's key is gone. Neither
//! touches anything else: the image keeps its keys, its links, its child rows, their order and
//! its version.

use serde_json::Value;

use crate::envelope::{self, Aad};
use crate::error::PiiError;
use crate::key::SubjectKeys;
use crate::nonce::NonceSource;
use crate::spec::PersonalFields;
use crate::walk::{NodePath, map_at};

/// The record an image belongs to: its entity from the spec, its id and version from the image.
struct Record {
    entity: String,
    id: String,
    version: u64,
}

impl Record {
    /// Reads the record identity an envelope is bound to.
    ///
    /// The entity comes from the `PersonalFields` description; the id and the version are the
    /// image's own `id` (string) and `version` (integer) members, which every state image
    /// carries and which `protect` leaves byte-identical.
    fn of(image: &Value, spec: &PersonalFields) -> Result<Self, PiiError> {
        let Value::Object(map) = image else {
            return Err(PiiError::ImageInvalid("the image is not an object"));
        };
        let id = map
            .get("id")
            .and_then(Value::as_str)
            .ok_or(PiiError::ImageInvalid("the image has no string `id`"))?;
        let version = map
            .get("version")
            .and_then(Value::as_u64)
            .ok_or(PiiError::ImageInvalid("the image has no integer `version`"))?;
        Ok(Self {
            entity: spec.entity().to_owned(),
            id: id.to_owned(),
            version,
        })
    }
}

/// Replaces every personal field value in `image` with an envelope sealed under its subject's
/// key.
///
/// A field declared `secret` is left exactly as it is (the journal never decrypts a secret),
/// and a field that is absent from this record is skipped. A field declared `erasable = false`
/// is sealed under [`SubjectKeys::retained_key`], so erasing its subject never touches it.
///
/// # Errors
///
/// * `journal_pii.spec_invalid` — the description is unusable.
/// * `journal_pii.image_invalid` — the image is not an object with a string `id` and an
///   integer `version`.
/// * `journal_pii.subject_invalid` — a protected field's subject path does not name a
///   non-empty string in the image.
/// * `journal_pii.already_protected` — the field already holds an envelope.
/// * `journal_pii.key_missing` — the subject has no key (create one before protecting).
/// * `journal_pii.key_store` — the key store failed, including a missing retained key.
/// * `journal_pii.nonce_unavailable` — the nonce source produced nothing.
/// * `journal_pii.envelope_invalid` — the value could not be sealed.
pub fn protect(
    image: &Value,
    spec: &PersonalFields,
    keys: &dyn SubjectKeys,
    nonces: &mut dyn NonceSource,
) -> Result<Value, PiiError> {
    spec.validate()?;
    let record = Record::of(image, spec)?;
    let mut out = image.clone();
    for field in spec.fields() {
        if field.secret() {
            continue;
        }
        let mut path = NodePath::default();
        out = map_at(&out, field.path().steps(), &mut path, &mut |value, node| {
            let subject = subject_of(image, field.subject_path().as_str())?;
            if envelope::is_envelope(value) {
                return Err(PiiError::AlreadyProtected {
                    path: node.concrete(),
                });
            }
            let key = if field.erasable() {
                keys.key_for(&subject)?.ok_or(PiiError::KeyMissing)?
            } else {
                keys.retained_key()?
            };
            let nonce = nonces.next_nonce().ok_or(PiiError::NonceUnavailable)?;
            let aad = Aad {
                entity: &record.entity,
                id: &record.id,
                version: record.version,
                path: &node.concrete(),
                subject: &subject,
            };
            envelope::seal(&key, &aad, value, nonce)
        })?;
    }
    Ok(out)
}

/// Puts every personal field value back, replacing a field whose subject's key is gone with the
/// documented placeholder `{"$erased": 1, "purpose": ..., "erased_at": null}`.
///
/// A value that is not an envelope (a record written before the field was declared personal, or
/// a placeholder written by an earlier `reveal`) is passed through unchanged, so revealing is
/// idempotent and the call still succeeds once a subject has been erased.
///
/// # Errors
///
/// * `journal_pii.spec_invalid` — the description is unusable.
/// * `journal_pii.image_invalid` — the image is not an object with a string `id` and an
///   integer `version`.
/// * `journal_pii.subject_mismatch` — the subject at the field's subject path in the image is
///   absent, not a string, empty, or not the subject the envelope names.
/// * `journal_pii.envelope_invalid` — an envelope is malformed, tampered with, or was moved to
///   another entity, record, version or field.
/// * `journal_pii.envelope_version` — an envelope carries a version this build does not read.
/// * `journal_pii.key_store` — the key store failed, including a missing retained key.
pub fn reveal(
    image: &Value,
    spec: &PersonalFields,
    keys: &dyn SubjectKeys,
) -> Result<Value, PiiError> {
    spec.validate()?;
    let record = Record::of(image, spec)?;
    let mut out = image.clone();
    for field in spec.fields() {
        if field.secret() {
            continue;
        }
        let mut path = NodePath::default();
        out = map_at(&out, field.path().steps(), &mut path, &mut |value, node| {
            if !envelope::is_envelope(value) {
                return Ok(value.clone());
            }
            let subject = envelope::read_header(value, &node.concrete())?;
            // Before any key lookup, and whether or not the key still exists: the subject the
            // image itself names must be the subject the envelope names.
            match image_subject(image, field.subject_path().as_str()) {
                Some(named) if envelope::same_subject(named, &subject) => {}
                _ => return Err(PiiError::SubjectMismatch),
            }
            let aad = Aad {
                entity: &record.entity,
                id: &record.id,
                version: record.version,
                path: &node.concrete(),
                subject: &subject,
            };
            envelope::check_aad(value, &aad)?;
            let key = if field.erasable() {
                match keys.key_for(&subject)? {
                    Some(key) => key,
                    None => return Ok(envelope::placeholder(field.purpose())),
                }
            } else {
                keys.retained_key()?
            };
            envelope::open(&key, &aad, value)
        })?;
    }
    Ok(out)
}

/// The data-subject id at `subject_path`, which must be a non-empty string.
fn subject_of(image: &Value, subject_path: &str) -> Result<String, PiiError> {
    let mut node = image;
    for segment in subject_path.split('.') {
        let Value::Object(map) = node else {
            return Err(PiiError::SubjectInvalid(
                "the subject path does not reach a value",
            ));
        };
        node = map.get(segment).ok_or(PiiError::SubjectInvalid(
            "the subject path does not reach a value",
        ))?;
    }
    match node.as_str() {
        Some(text) if !text.is_empty() => Ok(text.to_owned()),
        Some(_) => Err(PiiError::SubjectInvalid("the data-subject id is empty")),
        None => Err(PiiError::SubjectInvalid(
            "the data-subject id is not a string",
        )),
    }
}

/// The non-empty string at `subject_path` in `image`, or `None` when it is absent, not a string
/// or empty.
fn image_subject<'a>(image: &'a Value, subject_path: &str) -> Option<&'a str> {
    let mut node = image;
    for segment in subject_path.split('.') {
        node = node.as_object()?.get(segment)?;
    }
    node.as_str().filter(|text| !text.is_empty())
}
