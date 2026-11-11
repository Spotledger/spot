//! `spot-expand-contract` — expand/contract enforcement for the migration lifecycle
//! (blueprint WS12, WP-MIG-09; ADR-22).
//!
//! A schema change and the code that stops using the old shape must not ship in the same
//! release. The rule of ADR-22 is that a **contract** step (a drop, a rename, tightening a
//! column to `NOT NULL`, or narrowing a type) ships **one release after** the code stopped
//! reading or writing the object it removes. This crate is the gate:
//!
//! - [`phase`]: [`classify`](phase::classify) reads the plan engine's
//!   [`ReleasePlan`](spot_migration_plan::ReleasePlan) and gives every schema-shaping change a
//!   [`Phase`](phase::Phase) — [`Expand`](phase::Phase::Expand) (add a table, a nullable or
//!   defaulted column, an index, a foreign key; widen a type), [`Migrate`](phase::Phase::Migrate)
//!   (a backfill or a validation) or [`Contract`](phase::Phase::Contract) (drop, rename, tighten
//!   `NOT NULL`, narrow a type). The change model is `spot-migration-plan`'s; this crate never
//!   re-diffs a schema.
//! - [`usage`]: the **usage manifest** — the tables, columns and indexes a release's binary can touch —
//!   with its TOML form, a derivation from the storage layer's compiled schema registry
//!   ([`Catalog`](spot_storage_contract::Catalog)) and a check against that registry.
//! - [`check`]: [`check`](check::check) refuses a `Contract` change whose object the **previous**
//!   release still reads or writes, naming the object, the release whose code stops using it and
//!   the earliest release the contract may ship in.
//!
//! # Why the manifest comes from the schema registry
//!
//! The brief asks to investigate whether a usage manifest is cheaper to emit from a build script
//! or from the storage layer's schema registry. It is the registry: storage only ever sees the
//! [`Catalog`](spot_storage_contract::Catalog) compiled from metadata (ADR-045 — queries are
//! typed, never hand-built SQL), so the set of objects a binary *can* touch is exactly its
//! compiled catalogue. Deriving the manifest from it needs no new build step, no source scan and
//! no runtime instrumentation, and it cannot drift from the binary. It over-approximates (a
//! declared field the code never touches is still listed), which is the safe direction for a
//! gate: a contract ships a little later, never too early. [`check_manifest`](usage::check_manifest)
//! proves a checked-in manifest still covers the registry.
//!
//! The manifest of a release is **generated** from its compiled catalogue by the release build
//! (`UsageManifest::from_catalog(..).to_toml()`) and checked in only as an artefact; nobody edits
//! it by hand. Since the catalogue holds the derived traversal indexes, every index is listed as
//! `index:table.name` and cannot be forgotten.
//!
//! # What this crate does not do
//!
//! It never connects to a database, reads a clock or the environment, or applies anything. It
//! contains no float arithmetic (ADR-12), no `unsafe`, and no hand-built SQL (ADR-45). The
//! N-1 orchestration that migrates a database and runs the previous release's tests against it
//! is [`tools/ci/n_minus_one.py`], not Rust.
//!
//! # Example
//!
//! ```
//! use spot_expand_contract::{Release, ReleaseId, check};
//! use spot_expand_contract::usage::UsageManifest;
//! use spot_migration_plan::{SchemaPlan, SchemaRefusal, SystemDescription, plan};
//!
//! // The backend refuses the drop of `party.note`, so the plan carries it as a contract.
//! let schema = SchemaPlan {
//!     steps: Vec::new(),
//!     refusals: vec![SchemaRefusal {
//!         entity: "party".to_owned(),
//!         field: Some("note".to_owned()),
//!         code: "migrate.refused".to_owned(),
//!         reason: "it drops a column".to_owned(),
//!     }],
//! };
//! let release_plan = plan(
//!     &SystemDescription::default(),
//!     &SystemDescription::default(),
//!     &schema,
//! );
//!
//! let release_id = ReleaseId::new("2.0.0").expect("a label");
//! let next_id = ReleaseId::new("2.1.0").expect("a label");
//! // The previous release's code still touches the column: the contract is too early.
//! let previous =
//!     UsageManifest::from_toml("release = \"1.0.0\"\nobjects = [\"party.note\"]\n")
//!         .expect("a manifest");
//! let current = UsageManifest::from_toml("release = \"2.0.0\"\nobjects = []\n")
//!     .expect("a manifest");
//! let release = Release {
//!     id: &release_id,
//!     next: Some(&next_id),
//!     plan: &release_plan,
//!     usage: &current,
//! };
//! let report = check(&release, &previous);
//! assert!(!report.is_ok());
//! assert_eq!(report.refusals.len(), 1);
//! ```

#![forbid(unsafe_code)]

/// The usage gate: a contract change against the previous release's usage.
pub mod check;
/// The stable `expand_contract.*` codes.
pub mod codes;
/// Typed errors with stable `expand_contract.*` codes.
pub mod error;
/// The table, column or index a schema change acts on.
pub mod object;
/// The expand/migrate/contract classification of a plan's changes.
pub mod phase;
/// The identity of a release.
pub mod release;
/// The usage manifest: what a binary touches, its TOML form and its checker.
pub mod usage;

pub use check::{Accepted, Earliest, Refusal, Release, Report, check};
pub use error::Error;
pub use object::Object;
pub use phase::{Phase, PhaseChange, classify, classify_change, contract_object};
pub use release::ReleaseId;
pub use usage::{ManifestProblem, ManifestProblemKind, StopContext, UsageManifest, check_manifest};
