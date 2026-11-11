//! `spot-sync-policy` — per-DocType sync policy declared in metadata (WP-EDG-08.T1; ADR-062).
//!
//! Design rule 1 of ADR-062 is *nothing syncs unless declared*: the default policy is `local`.
//! This crate makes a DocType opt in **by configuration alone** — a runtime-created DocType
//! syncs because its metadata carries a policy, never because Rust code names it.
//!
//! It provides the five pieces WP-EDG-08 asks for:
//!
//! * [`SyncPolicy`] — the four fields the blueprint names (`class`, `owner_scope`, `direction`,
//!   `trigger`) plus the `exclude_fields` set — in the metadata **text form** as a DocType
//!   property, parsed exactly like [`spot_meta::Property`]'s key/value form so it composes with
//!   customisation (see [`text`]).
//! * [`resolve`] — builds a [`PolicyTable`] from effective metadata (runtime-created DocTypes
//!   and customisations included) and a [`PolicySet`] of declared policies.
//! * [`lint`] — every problem at once: a synced DocType linking to a `local` one, a direction
//!   that contradicts the class, a missing `owner_scope`, an excluded field that is required or
//!   part of a link, and a policy change that would orphan queued records ([`lint_change`]).
//! * [`PolicyLookup`] — the seam `spot-partition` and `spot-sync-master` consult instead of
//!   depending on this crate's internals.
//! * [`verify`] / [`verify_change`] — the function the metadata registry build and the app
//!   installer call: resolve, lint, and hand back every problem at once.
//!
//! [`outgoing_view`] is the sender side of `exclude_fields`: an excluded name never leaves the
//! origin. [`accept_incoming`] is the receiver side: a document that still carries an excluded name
//! is refused rather than applied.
//!
//! The exclusion rule is **by name, at every level** — the same rule the hub's JSON redactor
//! applies to a journal image. `SyncPolicy::excludes(name)` answers for a parent field, for a
//! whole child table, and for a field inside a child row; there is no `table.field` syntax. So
//! `outgoing_view` removes an excluded name from the parent fields, drops an excluded table
//! entirely, and strips an excluded name from each row of every remaining table (keeping the row,
//! its `id` and its order), and `accept_incoming` reports the same three cases.
//!
//! # What this crate does not decide
//!
//! ADR-063 (presets and scope-key semantics) is still *Proposed*. This crate implements the four
//! fields the blueprint fixes and keeps every policy decision behind a trait or a value the
//! caller passes in. It invents no presets, defines no meaning for a scope key type beyond its
//! syntax, and reads no clock — every instant, count and destination is an argument. The places
//! a later ADR will matter are listed under `## Open questions` in the task report.

#![warn(missing_docs)]

pub mod error;
pub mod hook;
pub mod lint;
pub mod lookup;
pub mod policy;
pub mod preset;
pub mod preset_lint;
pub mod table;
pub mod text;
pub mod view;
pub mod vocab;

pub use error::{PolicyError, codes};
pub use hook::{verify, verify_change};
pub use lint::{PolicyProblem, QueuedRecords, lint, lint_change};
pub use lookup::{PolicyLookup, StaticPolicies};
pub use policy::{Class, Direction, FieldName, ScopeKeyType, SyncPolicy, Trigger};
pub use preset::{
    AdoptMode, ClosureOverrides, ExcludedFields, KeyRef, Names, Obligation, Preset, PresetChange,
    PresetIssue, PresetPolicy, ReadingPart, Selector, StalePolicy, WhenEmpty, changes,
};
pub use preset_lint::{
    Edge, KeyScope, MetaExtras, PlainExtras, PresetTable, lint_preset_entity, lint_presets,
};
pub use table::{PolicySet, PolicyTable, resolve};
pub use text::PolicyProperty;
pub use view::{accept_incoming, outgoing_view};
pub use vocab::{
    PolicyMode, Refusal, Report, Rewrite, lint_vocabulary, rewrite_candidate,
    rewrite_candidate_with, verify_with_mode,
};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-sync-policy";
