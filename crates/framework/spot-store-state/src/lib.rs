//! `spot-store-state`: the store server's own durable state (WP-STR-03.T4).
//!
//! `spot-registry` holds the catalogue (ownership, releases, publisher status, search). The store
//! server needs more that a catalogue does not have, and ADR-45 keeps SQL out of everything but
//! storage crates, so this port exists: hashed publisher keys, review records and the listing
//! state, idempotency keys, revoked keys and bundles with the revocation list generation, and an
//! append-only audit log. [`MemoryStoreState`] is the reference and the test double;
//! `spot-store-state-pg` is the PostgreSQL implementation, held to the same cases.
//!
//! # The listing fails closed
//!
//! A version is publicly listed only when the store holds an explicit `listed` row for
//! `(app, version)`. No row, `pending_review`, `rejected` and `withdrawn` all mean not listed. A
//! crash between the registry commit and the state insert therefore hides a version and never
//! exposes one. Use [`StoreStateStore::listed_among`] to check a page of candidates in one call.
//!
//! # Secrets
//!
//! A publisher key is stored only as its SHA-256 ([`KeyHash`]); the key never enters this crate.
//! No error, `Debug` output or audit entry carries a key, a key hash, a token, a bundle or a
//! review summary.
//!
//! # Calling it
//!
//! The trait is synchronous. The PostgreSQL implementation must be called from a plain OS thread,
//! never from a tokio runtime thread (including `spawn_blocking`); see its crate docs.
//! No implementation reads a clock; time is an argument. No floats.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod error;
mod memory;
mod model;
mod store;

pub use crate::error::{
    BACKEND, BAD_SUMMARY, BAD_TRANSITION, DUPLICATE_KEY_HASH, IDEMPOTENCY_CONFLICT, KEY_UNKNOWN,
    REVIEW_UNKNOWN, Result, StoreStateError, TOO_LARGE,
};
pub use crate::memory::MemoryStoreState;
pub use crate::model::*;
pub use crate::store::StoreStateStore;
