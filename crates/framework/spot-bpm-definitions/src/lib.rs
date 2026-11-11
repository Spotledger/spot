//! `spot-bpm-definitions`: durable process definitions (WP-BPM-09.T2, ADR-49).
//!
//! `spot-bpm-engine` keeps compiled processes in [`ProcessRegistry`](spot_bpm_engine::ProcessRegistry),
//! an in-memory map, so a restarted server knew no process and a Run could not be resumed on its
//! pinned version. This crate is the durable source of truth:
//!
//! - [`DefinitionStore`]: published versions are stored, immutable, with their content hash,
//!   state, publisher and note. `publish` assigns `latest + 1` and checks `expected_previous` in
//!   one atomic step, so two concurrent publishes cannot both win.
//! - [`MemoryDefinitionStore`]: the reference implementation.
//! - [`conformance`]: the suite every store runs (the PostgreSQL store is `spot-bpm-defs-pg`).
//! - [`load_into`]: rebuilds a `ProcessRegistry` from a store and reports every version that no
//!   longer compiles, by name and number.
//!
//! Time is always an argument; no function here reads a clock. Nothing here logs.

#![forbid(unsafe_code)]

pub mod conformance;
mod error;
mod load;
mod memory;
mod store;
mod types;

pub use error::{DefinitionError, Result, codes};
pub use load::{LoadFailure, LoadFailureKind, LoadReport, load_into};
pub use memory::MemoryDefinitionStore;
pub use store::{DefFuture, DefinitionStore};
pub use types::{
    MAX_LABEL_BYTES, MAX_PAGE, MAX_TEXT_BYTES, StoredVersion, VersionState, check_name,
    check_publish, content_hash,
};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-bpm-definitions";
