//! The one place an image meets `spot-journal-pii`: look the entity up, bind the record identity,
//! protect or reveal, and hand back either a new image or "unchanged".
//!
//! `spot-journal-pii` binds each envelope to the record's `id` and `version`, which it reads from
//! members of the image. A journal image is the document body and may carry neither, while the
//! authoritative identity is the envelope's subject and `subject_version` (or the snapshot
//! record's). So the identity is bound in for the call and taken out again afterwards; a body
//! that already carries an `id` or `version` must agree with the authority or the call fails.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde_json::Value;
use spot_journal_pii::{NonceSource, PersonalFields, SubjectKeys, protect, reveal};

use crate::error::ProtectError;
use crate::source::PersonalFieldsSource;

/// The subject-key store the transforms read. One store per tenant: its keys are that tenant's.
pub type SharedKeys = Arc<dyn SubjectKeys + Send + Sync>;
/// The source of entity descriptions.
pub type SharedFields = Arc<dyn PersonalFieldsSource>;
/// The nonce source, behind a lock so the transforms can be shared.
pub type SharedNonces = Arc<Mutex<dyn NonceSource + Send>>;

/// The record an image belongs to, as its entry or its snapshot record names it.
pub(crate) struct Identity<'a> {
    pub(crate) entity: &'a str,
    pub(crate) id: &'a str,
    pub(crate) version: u64,
}

/// What the lookup of an entity found.
fn spec_of(
    fields: &SharedFields,
    entity: &str,
) -> Result<Option<Arc<PersonalFields>>, ProtectError> {
    let found = fields
        .fields_for(entity)
        .map_err(|error| ProtectError::SpecUnavailable {
            reason: error.reason,
        })?;
    Ok(found.filter(|spec| !spec.fields().is_empty()))
}

/// The image with the record identity bound in, and which members were added.
fn bind(image: &Value, identity: &Identity<'_>) -> Result<(Value, bool, bool), ProtectError> {
    let Value::Object(map) = image else {
        return Err(ProtectError::EntryInvalid("a state image is not an object"));
    };
    let mut bound = map.clone();
    let added_id = match bound.get("id") {
        Some(Value::String(existing)) if existing == identity.id => false,
        Some(_) => {
            return Err(ProtectError::IdentityMismatch(
                "the image's id is not the record's id",
            ));
        }
        None => {
            bound.insert("id".to_owned(), Value::String(identity.id.to_owned()));
            true
        }
    };
    let added_version = match bound.get("version") {
        Some(existing) if existing.as_u64() == Some(identity.version) => false,
        Some(_) => {
            return Err(ProtectError::IdentityMismatch(
                "the image's version is not the record's version",
            ));
        }
        None => {
            bound.insert("version".to_owned(), Value::from(identity.version));
            true
        }
    };
    Ok((Value::Object(bound), added_id, added_version))
}

/// Takes the members `bind` added back out.
fn unbind(mut value: Value, added_id: bool, added_version: bool) -> Value {
    if let Value::Object(map) = &mut value {
        if added_id {
            map.remove("id");
        }
        if added_version {
            map.remove("version");
        }
    }
    value
}

/// `Some(result)` only when it differs from `image`: an image with no personal value in it is used
/// as it is, so its entry is not re-serialised.
fn changed(result: Value, image: &Value) -> Option<Value> {
    (result != *image).then_some(result)
}

fn lock<T: ?Sized>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A nonce source holds a counter or an RNG; a panic elsewhere does not make it unsafe to use.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Seals the personal fields of `image`. `Ok(None)` means the entity has no personal value here and
/// the image is to be used as it is.
pub(crate) fn protect_image(
    fields: &SharedFields,
    keys: &SharedKeys,
    nonces: &SharedNonces,
    identity: &Identity<'_>,
    image: &Value,
) -> Result<Option<Value>, ProtectError> {
    let Some(spec) = spec_of(fields, identity.entity)? else {
        return Ok(None);
    };
    let (bound, added_id, added_version) = bind(image, identity)?;
    let sealed = {
        let mut source = lock(nonces);
        protect(&bound, &spec, &**keys, &mut *source).map_err(ProtectError::Protect)?
    };
    Ok(changed(unbind(sealed, added_id, added_version), image))
}

/// Opens the personal fields of `image`; a destroyed subject's fields become the placeholder.
/// `Ok(None)` means there was nothing to open.
pub(crate) fn reveal_image(
    fields: &SharedFields,
    keys: &SharedKeys,
    identity: &Identity<'_>,
    image: &Value,
) -> Result<Option<Value>, ProtectError> {
    let Some(spec) = spec_of(fields, identity.entity)? else {
        return Ok(None);
    };
    let (bound, added_id, added_version) = bind(image, identity)?;
    let opened = reveal(&bound, &spec, &**keys).map_err(ProtectError::Reveal)?;
    Ok(changed(unbind(opened, added_id, added_version), image))
}
