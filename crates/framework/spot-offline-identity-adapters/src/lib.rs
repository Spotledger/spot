//! `spot-offline-identity-adapters`: the adapters and hardenings around
//! `spot-offline-identity` (WP-IAM-09.T2, ADR-070 rule 12.13 c, d, h and i).
//!
//! The core crate defines the ports and leaves their implementations out. This crate is the
//! three implementations a node and a hub actually run:
//!
//! - [`key_release`]: [`release::ProtectedKeysRelease`], a
//!   [`KeyReleasePort`](spot_offline_identity::KeyReleasePort) over
//!   `spot_protected_keys::Hub::release`. It returns the **wrapped** key only and maps every
//!   refusal of the key hierarchy to [`OfflineError`], so no key material can reach an error or
//!   an audit record.
//! - [`quarantine`]: [`quarantine::OfflineQuarantine`], a
//!   [`QuarantinePort`](spot_offline_identity::QuarantinePort) over the `spot-quarantine` item
//!   model. It is **idempotent on `(node, journal_seq)`**: a replayed position stores nothing
//!   new and returns `Ok`, while the same position with a different payload is an error,
//!   because a held payload is immutable.
//! - [`persistence`]: [`persistence::SealedPersistence`], a
//!   [`Persistence`](spot_offline_identity::Persistence) that seals the epoch and lease state
//!   under the node's key with node-bound associated data, and refuses a state sealed for
//!   another node, an altered state and a rolled-back epoch.
//!
//! # Rules this crate keeps
//!
//! - **Sans-I/O.** Time is an argument: the hub's monotonic reading for a key release, and the
//!   audit microseconds for a queue item. Nothing here reads a clock.
//! - **No secrets.** A sealed state and a wrapped key are ciphertext. No error, log or audit
//!   record carries key material or a payload.
//! - **Fail closed.** A store that cannot be read, an envelope that does not decode and a
//!   state that does not authenticate are refusals, never allowances. A queue item that cannot
//!   be placed is returned as an error so the sync report leaves the write unacknowledged.
//! - **Monotonic epoch.** The sealed state's epoch fence only ever rises; a stored epoch below
//!   it is refused rather than restored.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(clippy::float_arithmetic)]

pub mod error;
pub mod key_release;
pub mod persistence;
pub mod quarantine;

pub use error::AdapterError;
pub use key_release::ProtectedKeysRelease;
pub use persistence::{MemoryStateStore, SealedPersistence, SealedStateStore, Slot};
pub use quarantine::{MemoryEnvelopes, OfflineQuarantine, Placement, QuarantineEnvelopes};

/// The name of this crate, for diagnostics.
pub const CRATE_NAME: &str = "spot-offline-identity-adapters";
