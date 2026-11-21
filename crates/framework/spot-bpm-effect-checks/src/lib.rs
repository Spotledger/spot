//! `spot-bpm-effect-checks`: design-time checks for the effects of a process (WS8, WP-BPM-15.T3;
//! ADR-080 section 6.8).
//!
//! These run at publish and in the designer. They read a process version and the effect
//! declarations in the seam registry, and never need a running system: nothing here executes a
//! process, touches storage or reads the clock.
//!
//! - [`check`]: the problems ([`ProblemKind`]): an undo edge past an irreversible effect, a record
//!   type with effects but no declared reversal, an empty commit point, a rewind that would
//!   create a record twice, an unknown effect class, a never-held effect in a held region.
//! - [`effect_summary`]: per node, the declared effects with their reversibility (the designer's
//!   badges).
//! - [`preview_from_receipts`]: what a designer test run produced, per node, with undeclared
//!   effects flagged and totals per class.
//! - [`PublishGate::check`]: refuses a version with error-level problems; warnings pass.
//!
//! # What the host supplies
//!
//! The process model does not say which backward edge undoes, which node revises on a rewind, or
//! which classes the site knows. The host answers through [`EffectMeta`]; [`PolicyMeta`] is a
//! table implementation, and by default an edge labelled `undo` or `rewind` has that mode.
//!
//! **The default [`PolicyMeta`] is fail-open by configuration.** It knows every effect class, only
//! the exact labels `undo` and `rewind` (case-sensitive; `Undo` is a forward edge) mark a backward
//! edge, and no node has a revision. It is for tests and tools. The publish path must pass a host
//! [`EffectMeta`] that reads the real edge semantics and the site's class registry, or a typo in
//! a label silently turns a check off.
//!
//! # What is not inspected
//!
//! Only document nodes declare effects the checks can read. Integration, SendMessage and Agent
//! nodes may cause external effects (some irreversible), but they are not inspected: they are
//! invisible to `undo_past_irreversible` and to the held-region checks. This is a known design
//! gap until those nodes carry effect declarations. A document node whose record type has no
//! pipeline at all is reported as a `no_declared_pipeline` warning.
//!
//! # Fail closed, bounded, no secrets
//!
//! A process beyond [`MAX_NODES`], [`MAX_EDGES`], [`MAX_BACK_EDGES`] or [`MAX_COMMIT_POINTS`] gets a `too_large` error
//! problem and no further analysis; a preview takes at most [`MAX_RECEIPTS`] receipts. Problems
//! and previews carry node ids, step names and class names only: no store reference, digest,
//! field value or peer text.

#![forbid(unsafe_code)]

mod check;
mod declared;
pub mod error;
mod gate;
mod graph;
pub mod meta;
mod preview;
mod problem;
mod summary;

pub use check::check;
pub use error::{ChecksError, Result, codes};
pub use gate::PublishGate;
pub use graph::{MAX_BACK_EDGES, MAX_COMMIT_POINTS, MAX_EDGES, MAX_NODES};
pub use meta::{EdgeMode, EffectMeta, LABEL_REWIND, LABEL_UNDO, PolicyMeta};
pub use preview::{
    ClassTotal, MAX_EFFECTS_PER_RECEIPT, MAX_RECEIPTS, NodePreview, Preview, PreviewEffect,
    TestRunReceipt, preview_from_receipts,
};
pub use problem::{Problem, ProblemKind, Severity, Undo, Where};
pub use summary::{EffectBadge, NodeEffects, effect_summary};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-bpm-effect-checks";
