//! Personal-field protection wired into the journal (WP-JRN-08.T4, ADR-061 s3 and s6).
//!
//! `spot-journal-pii` is the mechanism: it seals the personal fields of one record image under a
//! per-subject key and opens them again. This crate puts that mechanism where the data moves:
//!
//! ```text
//!  outbox line --ProtectTransform--> relay --> segments / tail chunks     (never clear)
//!  database    --ProtectedParts----> snapshot writer --> snapshot parts   (never clear)
//!  segments + snapshot --> rebuild executor --RevealSink--> apply sink    (revealed here only)
//! ```
//!
//! Personal data therefore reaches the journal and its snapshots only as envelopes, and becomes
//! readable again only inside a rebuild, through a key. **Erasure is destroying the key**:
//! afterwards a rebuild of the whole journal yields the documented placeholder
//! (`{"$erased": 1, "purpose": ..., "erased_at": null}`) for each erasable field of that subject,
//! and every other byte of the rebuilt database is what it was: the same records, versions,
//! positions and links.
//!
//! # What is transformed
//!
//! * **State images only** (topic `doc_state`, the rebuild executor's own
//!   [`spot_journal_rebuild::classify`] rule). A tombstone's image is protected too when it
//!   carries personal fields.
//! * **Audit and domain events pass through as the same bytes.** A domain event is a fact for
//!   consumers, not a state image, and this crate cannot know which of its values are personal.
//!   Keeping personal values out of domain events is the **declarer's responsibility**: the
//!   metadata must not flag personal fields on domain events, and a check for that is a lint task
//!   of its own. A test here pins that a domain event carrying a personal-looking value is left
//!   exactly as it is.
//! * A field declared `secret` is passed through untouched both ways (the journal never decrypts a
//!   secret; `spot-secrets` stores it encrypted already), and a field declared `erasable = false`
//!   is sealed under the tenant journal key, so erasing its subject does not touch it.
//!
//! # Failing closed
//!
//! * **In:** a state image whose entity has personal fields but whose protect call fails (no key
//!   for the subject, the key store is down, no nonce, a malformed image) is not written.
//!   [`ProtectTransform::transform_batch`] returns the error and no batch; the relay-driving
//!   [`ProtectTransform::append_batch`] has not called the relay. A line that cannot even be
//!   parsed is refused too, because whether it is a state entry cannot be told. A description
//!   source that fails is a refusal, never "no personal fields".
//! * **Out:** a key store outage, a tampered envelope or one moved to another record is an error
//!   from [`RevealSink`], which makes the executor abort the rebuild. Only a destroyed key yields
//!   the placeholder.
//!
//! # Tenancy
//!
//! A transform, a [`ProtectedParts`] and a [`RevealSink`] are built over one tenant's
//! [`SharedKeys`] (its subject keys and its tenant journal key). An envelope another tenant
//! wrote does not open with them: the tenant journal key and the subject keys differ, so opening
//! fails with `journal_pii.envelope_invalid` (or, for an erasable field whose subject has no key
//! in this tenant, yields the placeholder, never the value).
//!
//! # Record identity
//!
//! An envelope is bound to its record's entity, id and version. The id and version come from the
//! entry (the envelope's subject and `subject_version`) or the snapshot record, not from the
//! image body; a body that carries its own `id` or `version` must agree, or the call fails with
//! `journal_protect.identity_mismatch`.
//!
//! # Not here
//!
//! The declaration of personal fields (the metadata layer supplies a [`PersonalFieldsSource`]),
//! legal retention policy (data the declarer supplies, ADR-061 open question 2), the key store
//! itself (`spot-journal-pii-secrets`), and the nonce source's randomness (an argument).
//!
//! The crate reads no clock and no randomness, creates and stores no key, logs nothing, and no
//! error carries a plaintext, a ciphertext or a key.

#![forbid(unsafe_code)]

mod error;
mod image;
mod parts;
mod reveal;
mod source;
mod transform;

#[cfg(test)]
mod tests;

pub use error::{AppendError, ProtectError};
pub use image::{SharedFields, SharedKeys, SharedNonces};
pub use parts::ProtectedParts;
pub use reveal::RevealSink;
pub use source::{PersonalFieldsSource, SourceError, StaticFields};
pub use transform::ProtectTransform;
