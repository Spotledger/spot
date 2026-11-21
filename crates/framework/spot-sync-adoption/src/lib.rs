//! `spot-sync-adoption`: the adoption of a `handover` record as a pure state machine
//! (WP-SYN-01.T3, ADR-063 section 5 and amendment 1).
//!
//! The exchange, in stream order: the hub writes a request naming the version it has seen
//! (it does not own the record yet); the creating node applies it, **stops owning** (its copy
//! becomes a read-only mirror) and writes an acknowledgement carrying its **final version**;
//! because the acknowledgement follows the node's own version events in its stream, the hub
//! holds every version up to that one when it processes it, and only then takes ownership and
//! writes the adopted version. Edits the node makes before it applies the request are allowed
//! and flow up; afterwards a local edit is refused (`sync_adoption.not_owner`).
//!
//! * [`HubSide`] and [`NodeSide`] each have a pure `step(self, input) -> (Self, Vec<Output>)`.
//!   A refused input leaves the state unchanged and yields one `Rejected` output.
//! * The request is issued at most once per record: `adopt = auto` issues it when the record
//!   arrives; `manual` waits for [`HubInput::ProcessStepFired`] (the business-process step that
//!   overrides the DocType default for a record of that process) or [`HubInput::AdminAdopt`];
//!   `never` never requests. The `adopt` setting is an input, never a hidden constant.
//! * A pure library: no clock, no randomness, no I/O, no persistence.
//!
//! The condition (`AdoptionPending`, amber, data only) is raised when the node is offline
//! while the adoption is pending, and cleared when the adoption completes. It never blocks an
//! edit at either side.

mod error;
mod hub;
mod node;

pub use error::AdoptionError;
pub use hub::{HubInput, HubOutput, HubSide};
pub use node::{NodeInput, NodeOutput, NodeSide, Ownership};

/// A version of a record, counting from the version the creating node wrote.
pub type Version = u64;

/// The adoption state of one record, as the hub sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RecordState {
    /// Created at the node, not yet requested.
    Provisional,
    /// The request is written; the node still owns the record until it acknowledges.
    AdoptionPending {
        /// The version the request names.
        requested_version: Version,
    },
    /// The hub owns the record.
    Adopted,
    /// `adopt = never`: the node owns the record for ever.
    NeverAdopted,
}
