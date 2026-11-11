//! Version skew for sync (ADR-066 section 5, WP-SYN-08.T1).
//!
//! A pure library: from what two sides report in the handshake it decides whether a session may
//! run ([`session_decision`]), which schema majors each node is served ([`served_majors`]), when a
//! producer may stop publishing an old major ([`can_retire`]), whether the hub accepts the previous
//! major from an older node ([`accepts_upward`]) and which single record is held when it cannot
//! be decoded ([`on_undecodable`], [`triage`], [`release_holds`]). No clock, randomness, network,
//! file or database. The hub-side half is [`filter_outgoing`] and [`release_for_upgrade`]. Tunable values ([`Settings`]) are arguments, never hidden constants.

mod error;
mod filter;
mod holds;
mod majors;
mod model;
mod session;

pub use error::{
    HOLD_RETIRED, HOLDS_FULL, NODE_TOO_OLD, SCHEMA_RETIRE_BLOCKED, SESSION_EPOCH_UNVERIFIED_HUB,
    SESSION_EPOCH_UNVERIFIED_NODE, SESSION_MIGRATING_HUB, SESSION_MIGRATING_NODE,
};
pub use filter::{
    FilterResult, Outgoing, filter_outgoing, filter_outgoing_with_fleet, release_for_upgrade,
};
pub use holds::{
    DEFAULT_HOLD_LIMIT, Hold, HoldReason, HoldSet, Incoming, RecordRef, Release, Triage,
    on_undecodable, release_holds, triage,
};
pub use majors::{Retirement, Upward, accepts_upward, can_retire, served_majors};
pub use model::{
    Major, NodeId, NodeReport, NodeState, NodeStatus, Settings, SoftwareVersion, Topic,
};
pub use session::{Decision, Retry, releases_behind, session_decision};
