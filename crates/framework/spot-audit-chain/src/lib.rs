//! The tamper-evident audit chain (WP-AI-11.T1).
//!
//! An append-only sequence of audit entries per site; each entry commits to the one before it by
//! SHA-256, and signed checkpoints commit to the head so that cutting off the whole tail is also
//! caught. The crate is a pure library:
//!
//! * it decides nothing about *what* is audited (callers build an [`AuditRecord`]);
//! * it stores nothing itself (an [`AuditStore`] does; [`InMemoryStore`] is the reference);
//! * it reads no clock, random source or environment (callers pass time and keys in);
//! * it is not a second journal: it proves the audit trail was not altered, it does not rebuild
//!   state (ADR-061).
//!
//! # Canonical encoding (format version 1)
//!
//! Hashes never depend on a serde implementation. All integers are big-endian; a *field* is a
//! `u32` byte length followed by the bytes; an *optional field* is a `0` byte (absent) or a `1`
//! byte then the field.
//!
//! ```text
//! record  = field(kind) field(actor) opt(on_behalf_of) opt(subject: field(doctype) field(id))
//!           i64(at) u8(masked) payload
//! payload = u32(count) then, with keys in ascending byte order,
//!           field(key) tag value        tag 1 = text:  field(value)
//!                                       tag 2 = int:   i64
//!                                       tag 3 = bool:  u8 (0 or 1)
//! entry hash      = SHA-256("spot.audit.entry.v1\0"      || u64(seq) || prev_hash || record)
//! genesis prev    = SHA-256("spot.audit.genesis.v1\0"    || field(site) || u32(format version))
//! checkpoint body = "spot.audit.checkpoint.v1\0" || field(site) || u64(seq) || head_hash
//!                   || i64(at) || field(key_id)           (the Ed25519 signature covers it)
//! ```
//!
//! The distinct domain tags keep an entry hash from ever being a valid checkpoint body.
//!
//! # Segments
//!
//! Checkpoints are taken every `N` entries ([`CheckpointPolicy`]); the `N` entries between two
//! checkpoint boundaries are a *segment* ([`segment_bounds`]) that can be verified on its own
//! ([`Verifier::verify_segment`]).
//!
//! # Limits that are inherent
//!
//! A chain alone cannot show that its newest entries were cut off: only a signed checkpoint (kept
//! in the store, or better held by a third party) that names the missing tail can. That is what
//! [`Verifier::verify_incremental`] with an externally trusted checkpoint is for.

mod canonical;
mod chain;
mod checkpoint;
pub mod contract;
mod entry;
mod error;
mod record;
mod store;
mod verify;

#[cfg(test)]
mod tests;

pub use chain::{AuditChain, CheckpointPolicy, segment_bounds};
pub use checkpoint::{Checkpoint, CheckpointSigner, KeyRing, LocalSigner};
pub use entry::{ChainEntry, FORMAT_VERSION, Hash, genesis_hash};
pub use error::AuditError;
pub use record::{
    AuditRecord, MAX_PAYLOAD_BYTES, Payload, PayloadValue, Subject, payload_encoded_len,
};
pub use store::{AppendOutcome, AuditStore, InMemoryStore, StoreFuture};
pub use verify::{Failure, FailureKind, Verifier, VerifyReport};
