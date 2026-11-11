//! `spot-bpm-commit`: commit points of the Business Process engine (WS8, WP-BPM-15.T2;
//! ADR-080 section 6.7).
//!
//! External effects (mail, bank calls, webhooks, messages to another organisation) wait for a
//! commit point. A process marks commit points with the `commit` node kind; the effects of the
//! nodes in the **region** before one are held in a hold scope owned by the request (the
//! migration hold, generalised in `spot-effect-hold`) and released at the commit point; if the
//! request is cancelled or undone first, they are discarded and nothing left the building.
//!
//! - [`region`]: [`RegionPlan`] decides, on the compiled graph, which nodes are held and for
//!   which commit point, and produces the design-time findings: a commit point with nothing to
//!   hold, and a **point of no return** wherever a never-held class (fiscal transmission) is
//!   caused inside a held region.
//! - [`handler`]: [`CommitHandler`], the node kind's handler (pure, like every handler).
//! - [`service`]: [`CommitService`] admits effects into the region, releases at the commit point
//!   in the original order (idempotent after a crash) and discards on cancel or undo.
//!
//! # Defaults and guarantees
//!
//! A process with no commit point holds nothing and flows exactly as before. A live migration
//! hold wins: a commit point reached during one releases nothing and reports it, so the host
//! retries afterwards. The guarantees of `spot-effect-hold` are kept: a failed dispatch stops the
//! walk, an effect that may have left is never claimed as discarded, and no payload or peer text
//! is stored or returned. Time is always an argument; there is no wall clock here.
//!
//! # Host obligations
//!
//! - **Principal and authority.** The API takes a [`RequestId`](spot_effect_hold::RequestId)
//!   only. The host decides who may commit or end a request and must check it before calling.
//! - **Gate on [`CommitOutcome::completed`].** The service keeps no per-request progress, so it
//!   does not stop `commit(B)` running before `commit(A)`. The host runs a process's commit
//!   points in graph order and moves past one only when `completed` is true (not deferred by a
//!   migration, nothing awaiting confirmation).
//! - **Failed `admit` means not recorded.** Any `admit` error means the effect was neither sent
//!   nor held; the caller must not treat it as held and must not send it.
//! - **Undo lock.** This crate does not depend on `spot-bpm-effects` and takes no undo guard.
//!   The host takes the undo lock of the request record file before `end(Undone)`, so an undo
//!   and a commit are not decided concurrently. Effects already released are compensated by
//!   that undo, not here.
//! - **Multi-node.** Single flight per request is an in-process mutex; with more than one node
//!   serving a site, give the `HoldService` a coordinator.
//!
//! # Not here
//!
//! Reversing effects that already left is the request record file's undo (`spot-bpm-effects`);
//! this crate decides only what has not yet left.

#![forbid(unsafe_code)]

pub mod error;
pub mod handler;
pub mod region;
pub mod service;

pub use error::{CommitError, Result, codes};
pub use handler::{CommitHandler, KIND_COMMIT, commit_kind, validate_config};
pub use region::{
    Finding, FindingKind, MAX_CLASSES_PER_NODE, MAX_COMMIT_POINTS, MAX_NODES, NodeEffects,
    RegionPlan, Severity, node_source,
};
pub use service::{CommitOutcome, CommitService, EndCause, NodeAdmission};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-bpm-commit";
