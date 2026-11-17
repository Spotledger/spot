//! `spot-sync-process`: process definitions as hub-owned data that syncs (WP-SYN-12.T1).
//!
//! A published process version is a **master record**: it flows from the hub to every edge in
//! scope and never back. The edge installs it **beside** the versions it has (a running Case is
//! pinned to its version and keeps running on it), verifies its hash, **holds** (never guesses)
//! one its engine cannot run, and removes a retired version only when no Case is pinned to it.
//!
//! | Piece | Side | What |
//! | --- | --- | --- |
//! | [`ProcessRecord`] | both | `(process, version, definition_hash, min_engine_version, compat_class, payload)`, named `process:<process>@<version>` |
//! | [`ProcessPublisher`] | hub | emits the record with its scope keys; a republish is a no-op, a changed definition under one version is refused; refuses an inbound `process:*` from a node |
//! | [`ProcessIntake`] | edge | install, hold, release on upgrade, retire when unpinned, [`start_version`](ProcessIntake::start_version) |
//! | [`CaseOrigin`] | both | the `(process, version)` a Case records in its first event; the hub resolves the same version or holds, never re-pins |
//!
//! Everything is behind traits ([`DefinitionStore`], [`PinCounter`], [`HubSink`]) with in-memory
//! implementations; nothing here reads a clock, a database or the network. No definition content
//! appears in an error or a log: names, versions and counts only.
//!
//! # Verification
//!
//! The edge checks the definition hash and the tenant, and that the record's `process` equals the
//! payload's. The publisher's signature is **not** checked here: the hub's record signature on the
//! sync wire is the control (see the task report); `min_engine_version` can only make an edge hold,
//! never relax a check.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod error;
mod intake;
mod origin;
mod publisher;
mod record;
mod store;

pub use error::{CODES, Error, Result};
pub use intake::{Capability, Installed, ProcessIntake, Received};
pub use origin::{CaseOrigin, Resolution};
pub use publisher::{HubSink, ProcessPublisher, Published};
pub use record::{CompatClass, ProcessRecord, hash_definition, record_name};
pub use store::{DefinitionStore, MemoryPins, MemoryStore, PinCounter};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-sync-process";
