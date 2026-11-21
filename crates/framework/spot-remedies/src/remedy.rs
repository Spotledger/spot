//! The [`Remedy`] trait: the object-safe contract every remedy implements.
//!
//! A remedy declares itself ([`id`](Remedy::id), [`meta`](Remedy::meta)), decides whether it
//! should run ([`precondition`](Remedy::precondition)), plans without changing anything
//! ([`dry_run`](Remedy::dry_run)), changes the world through the typed ports
//! ([`apply`](Remedy::apply)), reverses itself ([`undo`](Remedy::undo)), and ships the scenario
//! that proves it ([`self_test`](Remedy::self_test)). It has no storage handle: every change
//! goes through [`RemedyCtx`].

use serde_json::Value as Json;

use crate::ctx::RemedyCtx;
use crate::error::RemedyError;
use crate::model::{Applied, Check, Plan, RemedyId, RemedyMeta, Undone};
use crate::scenario::TestSpec;

/// A typed, tested remedy.
///
/// The trait is object-safe: the registry holds `Box<dyn Remedy>`. Implementations are pure over
/// the ports, never read a clock (the context's `now` is the only time) and never use a raw
/// storage handle.
pub trait Remedy: Send + Sync {
    /// The remedy's stable id.
    fn id(&self) -> RemedyId;

    /// Everything the remedy says about itself.
    fn meta(&self) -> RemedyMeta;

    /// Whether the remedy should run against this context and these parameters.
    fn precondition(&self, ctx: &mut RemedyCtx<'_>, params: &Json) -> Check;

    /// Plans the change without making it, sealed with a hash.
    ///
    /// # Errors
    ///
    /// A [`RemedyError`] when the plan cannot be built (a target is missing, a parameter is
    /// wrong for the world).
    fn dry_run(&self, ctx: &mut RemedyCtx<'_>, params: &Json) -> Result<Plan, RemedyError>;

    /// Applies a plan that dry-run produced.
    ///
    /// # Errors
    ///
    /// A [`RemedyError`] when a typed operation fails.
    fn apply(&self, ctx: &mut RemedyCtx<'_>, plan: &Plan) -> Result<Applied, RemedyError>;

    /// Reverses an apply.
    ///
    /// # Errors
    ///
    /// A [`RemedyError`] when the change cannot be reversed; an irreversible remedy always
    /// refuses with [`RemedyError::Irreversible`].
    fn undo(&self, ctx: &mut RemedyCtx<'_>, applied: &Applied) -> Result<Undone, RemedyError>;

    /// The scenario that proves the remedy: precondition, dry-run, apply, invariants and undo.
    fn self_test(&self) -> TestSpec;
}
