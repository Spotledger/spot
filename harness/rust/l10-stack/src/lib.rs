//! L10 on the real stack (WP-TST-11.T2).
//!
//! `spot-l10-liveness` proves, on `spot_meta::Registry`, that a change reaches the metadata with
//! no rebuild or restart. This crate runs the same 16 scenarios over the pieces that make the
//! claim true for a running system: [`spot_customise::CustomiseService`] (pipeline, version
//! store, swap), a real PostgreSQL behind [`spot_customise::PgSchema`] (the online migration
//! engine), and the document engine, whose metadata the swap listener replaces.
//!
//! - [`probes`]: the plan, schema and view probes as **pure comparisons** of what a layer holds
//!   against the metadata version, so a broken probe is caught on any machine.
//! - [`stack`]: [`stack::StackHost`], the [`spot_l10_liveness::LivenessHost`] over the stack.
//!
//! The probes never mutate state; the write-through check creates one document through the engine
//! after an additive change and reports it as a note.

pub mod nodes;
pub mod probes;
pub mod stack;

pub use stack::{StackFactory, StackHost};
