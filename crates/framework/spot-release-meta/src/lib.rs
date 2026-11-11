//! `spot-release-meta` — the release metadata contract of the Spotledger Platform
//! (blueprint WS12, WP-MIG-02, ADR-22).
//!
//! A release carries, in data, **what it changes and how to undo it**, so the migration planner
//! (WP-MIG-01), the expand/contract gate (WP-MIG-09) and the down-patches (WP-MIG-10) can rely
//! on it instead of on a promise. This crate is the format and the checker; it changes nothing,
//! connects to nothing, reads no clock and contains no float arithmetic (ADR-12) or `unsafe`.
//!
//! Two documents make up a release:
//!
//! - **Patch declarations** (`release-meta/patches/<id>.toml`, [`PatchDecl`]): every patch that
//!   exists in the app manifests declares its intent, the DocTypes it touches, its risk class
//!   (from [`spot_migration_plan::Risk`]), whether it is reversible and idempotent, its
//!   down-step and whether it needs a fixed epoch.
//! - **The behaviour changelog** (`release-meta/changelog.toml`, [`BehaviourChangelog`]): every
//!   behaviour change keyed to a DocType or a seam, with its kind and sensitivity.
//!
//! [`check_release`] refuses a release that leaves a patch undeclared, and
//! [`BehaviourChangelog::diff_against`] returns the divergences the L5 differential engine
//! (WP-TST-05) found that the changelog does not name. The `release-meta-check` binary and
//! `tools/ci/release_meta_check.py` put the first of those in CI.
//!
//! The patch convention itself is **not** redefined here: a patch is the app-manifest
//! `[[patches]]` row the plan engine already models
//! ([`spot_migration_plan::PatchDecl`]), and [`patches_on_disk`] reads the ones a
//! [`SystemDescription`](spot_migration_plan::SystemDescription) declares.
//!
//! # Example
//!
//! ```
//! use spot_release_meta::{PatchDecl, check_release};
//!
//! let decl = PatchDecl::parse(
//!     "id = \"acme_0001\"\nintent = \"add the acme field\"\nclass = \"additive\"\n\
//!      reversible = true\nidempotent = true\n",
//! )
//! .expect("a valid declaration");
//! assert!(check_release(&[decl.to_plan_decl()], &[decl]).is_empty());
//! ```

mod behaviour;
pub mod check;
pub mod cli;
mod error;
mod patch;

pub use behaviour::{
    BehaviourChangelog, BehaviourEntry, BehaviourKey, BehaviourKind, Divergence, DivergenceSource,
    Sensitivity,
};
pub use check::{Problem, ProblemKind, ReleaseContext, check_release, check_release_with};
pub use error::{Error, codes};
pub use patch::{PatchDecl, PatchRef, patches_on_disk};
