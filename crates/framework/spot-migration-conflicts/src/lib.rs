//! `spot-migration-conflicts` — customisation conflict detection for a release plan
//! (blueprint WS12, WP-MIG-03; ADR-22).
//!
//! The planner (WP-MIG-01, `spot-migration-plan`) knows **what a release changes**. This crate
//! finds **what those changes break in this site's customisations**, before anything is applied:
//! a custom field a new standard field collides with, a property override whose field the release
//! removes, a fixture that sets a removed field, a view layer that names one, a script that no
//! longer type-checks against the new `.d.ts`, a rule whose expression no longer parses or reads a
//! removed field, and an installed app whose WIT world, DocTypes or hooks the new host and
//! metadata no longer satisfy.
//!
//! # The shape of a run
//!
//! 1. A caller builds a [`ReleaseContext`]: the [`ReleasePlan`](spot_migration_plan::ReleasePlan),
//!    the new app definitions, the new effective metadata, and the site's customisations, view
//!    layers, scripts, rules, fixtures and installed apps.
//! 2. [`detect`] reads the release items out of the plan, runs every check and returns a
//!    [`ConflictReport`] of [`ConflictItem`]s.
//! 3. Each item carries a stable [`code`](codes), the [`Risk`](spot_migration_plan::Risk), the
//!    [`Cause`] (the release item and its effect), the affected [`ArtefactRef`]s — the pointer to
//!    the owning customisation — and a suggested resolution. One root cause that breaks many
//!    artefacts is **one** item with many artefacts, never a duplicate per artefact.
//!    [`ConflictReport::to_plan_changes`] folds the items into the plan's `Change`s.
//!
//! # What this crate does not do
//!
//! It never executes a script or a WASM app, never changes the site's customisations, never
//! connects to a database and never reads a clock or the environment: time and every input are
//! arguments. It contains no `unsafe`, no `unwrap`/`expect` outside tests and no float arithmetic
//! (ADR-12). A script is *checked*, by transpiling it (no execution) and reading its `doc.<field>`
//! accesses against the declarations `spot-dts` generated from the new metadata.
//!
//! # Example
//!
//! ```
//! use spot_meta::{Customisations, Definition, Registry};
//! use spot_migration_plan::{plan, SchemaPlan, SystemDescription};
//! use spot_migration_conflicts::{ReleaseContext, detect};
//!
//! let from = SystemDescription::new();
//! let to = SystemDescription::new();
//! let release = plan(&from, &to, &SchemaPlan::default());
//! let definitions = vec![Definition::new("party")];
//! let customisations = Customisations::new();
//! let meta = Registry::build(definitions.clone(), &customisations)
//!     .unwrap_or_else(|_| panic!("the definitions build"));
//! let context = ReleaseContext::new(&release, &definitions, &meta, &customisations);
//! assert!(detect(&context).is_empty());
//! ```

#![forbid(unsafe_code)]

/// The stable `migration_conflicts.*` codes.
pub mod codes;
/// The input to one detection run.
pub mod input;
/// The conflict model and the report.
pub mod model;

mod apps;
mod collect;
mod detect;
mod fields;
mod fixtures;
mod release;
mod rules;
mod scripts;
mod views;

pub use detect::detect;
pub use input::{HostSpec, InstalledApp, ReleaseContext, SiteFixture, SiteRule, SiteScript};
pub use model::{ArtefactKind, ArtefactRef, Cause, ConflictCategory, ConflictItem, ConflictReport};
