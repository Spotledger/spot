//! Personal-field encryption inside a journal state image (WP-JRN-08.T1, ADR-061 s3 and s6).
//!
//! A state event carries the whole record image. Fields flagged `personal` are encrypted in
//! that image under a key per data subject, so destroying the key makes them unreadable in the
//! journal and in every snapshot while the rest of the record is untouched.
//!
//! This crate is that mechanism and nothing else: a pure library over `serde_json::Value`. It
//! touches no storage, reads no clock and no random source, creates no keys and stores none.
//! Keys come from [`SubjectKeys`], nonces from [`NonceSource`], and the entity and field policy
//! from [`PersonalFields`].
//!
//! # What it does
//!
//! ```text
//! protect(image, spec, keys, nonces)  -> image with each personal value replaced by an envelope
//! reveal(image, spec, keys)           -> image with the values back, or the erased placeholder
//! erase(subject, keys, index)         -> destroys the subject's key, returns a receipt
//! ```
//!
//! Everything that is not a personal field value is byte-identical after `protect`: keys, links,
//! child rows, their order and their count, and the record version. `reveal` is the inverse
//! until a subject is erased, and from then on yields
//! `{"$erased": 1, "purpose": <the field's purpose>, "erased_at": null}`.
//!
//! # Envelope
//!
//! ```text
//! envelope = { "$pii": 1, "subject": <data-subject id>, "ct": <base64url>,
//!              "nonce": <base64url>, "aad_hash": <base64url> }
//! aad      = "spot.journal.pii:aad:v1"
//!            u32be(len) entity  entity
//!            u32be(len) id      id
//!            u32be(len) path    path       (concrete: `items.0.note`, not `items.*.note`)
//!            u32be(len) subject subject
//!            u64be      version
//! ct       = XChaCha20-Poly1305(aad, serde_json::to_vec(value))   (ciphertext || tag)
//! nonce    = 24 bytes from the caller's NonceSource
//! aad_hash = SHA-256(aad)
//! ```
//!
//! The associated data binds the entity, the record id, the record version, the concrete field
//! path and the data subject, so an envelope moved to another field, row, record, version or
//! subject fails with `journal_pii.envelope_invalid` rather than opening.
//!
//! The subject an envelope names is the one whose key sealed it, and it travels inside the
//! envelope rather than being re-read from the record, so editing the record's subject column
//! cannot move a value out of its subject's erasure: `reveal` looks the key up by the label the
//! envelope carries, and first requires that label to equal the subject the image names at the
//! field's subject path (`journal_pii.subject_mismatch`, before any key lookup and whether or
//! not the key still exists); `aad_hash` then refuses a relabelled envelope.
//!
//! The AEAD is the one `spot-secrets` already uses — XChaCha20-Poly1305 with a 24-byte nonce,
//! base64url without padding, the same `nonce || ciphertext` convention — so the platform keeps
//! one sealing primitive. `SecretBox` itself is not called: it is async and provider-driven,
//! and this crate's `protect` is synchronous with no runtime, so the crate uses the same AEAD
//! library directly (see the report's Open questions).
//!
//! # Record identity
//!
//! The entity is [`PersonalFields::entity`]; the record id and version are the image's own
//! `id` (string) and `version` (integer) members, which `protect` reads and never rewrites.
//!
//! # Erasure
//!
//! A field declared `erasable = false` is a legal-retention field: it is sealed under
//! [`SubjectKeys::retained_key`], the tenant journal key, so erasing its subject does not touch
//! it. A field declared `secret` is passed through untouched in both directions, because the
//! journal never decrypts a secret (ADR-061 s3).
//!
//! # Example
//!
//! ```
//! use serde_json::json;
//! use spot_journal_pii::{
//!     CounterNonces, FieldSpec, KeyHandle, MemoryKeys, PersonalFields, protect, reveal,
//! };
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let spec = PersonalFields::new("Contact")?.with_field(FieldSpec::new(
//!     "email",
//!     "id",
//!     "notifications",
//!     "contact_5y",
//! )?);
//! let keys = MemoryKeys::new(KeyHandle::new([9u8; 32]));
//! keys.insert("c-1", KeyHandle::new([7u8; 32]));
//!
//! let image = json!({
//!     "id": "c-1",
//!     "version": 1,
//!     "email": "ada@example.test",
//!     "city": "Leeds",
//! });
//! let protected = protect(&image, &spec, &keys, &mut CounterNonces::new())?;
//! assert_ne!(protected["email"], image["email"]);
//! assert_eq!(protected["city"], image["city"]);
//! assert_eq!(reveal(&protected, &spec, &keys)?, image);
//! # Ok(())
//! # }
//! ```

mod envelope;
mod erase;
mod error;
mod key;
mod nonce;
mod protect;
mod spec;
mod testing;
mod walk;

#[cfg(test)]
mod tests;

pub use erase::{
    EntityErasure, ErasureDetail, ErasureReceipt, FieldErasure, IndexEntry, SubjectIndex, erase,
};
pub use error::PiiError;
pub use key::{KEY_LEN, KeyHandle, KeyStoreError, SubjectKeys};
pub use nonce::{NONCE_LEN, NonceSource};
pub use protect::{protect, reveal};
pub use spec::{FieldPath, FieldSpec, PersonalFields, Step};
pub use testing::{CounterNonces, MemoryKeys};
