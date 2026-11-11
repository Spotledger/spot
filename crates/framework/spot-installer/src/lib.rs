//! Spotledger's install planner (WP-STR-04.T1, ADR-21; blueprint WS11–WS12): the pure,
//! deterministic per-site plan an install, upgrade or uninstall will run, built from a catalogue
//! of verified bundles **before** anything changes.
//!
//! The plan and its execution are split. This crate produces the plan and runs it. The plan is
//! ordered actions, each with the patches to run, the fixtures to load, a [`Risk`], whether it can
//! be undone, and the capabilities it asks for beyond what an administrator approved. It carries
//! an [`InstallPlan::hash`] so the migration lifecycle (plan, approve, rehearse, apply, verify;
//! ADR-22) binds an approval to exactly the actions it saw, and [`InstallPlan::explain`] gives the
//! sentences an administrator reads. The [`Executor`] runs a ready plan step by step over traits
//! ([`AppRegistryStore`], [`PatchRunner`], [`ComponentHost`], [`FixtureLoader`], [`Snapshotter`]),
//! journals every step, rolls back on failure and can [`Executor::resume`] a run that was
//! interrupted. The executor is pure of storage and the wasm host: a real adapter wires the traits
//! later.
//!
//! The crate is pure otherwise: no storage, no network, no clock of its own (the [`Executor`]
//! reads time only through an injected `spot_core::Clock`), no WebAssembly. Signature checks are
//! the caller's job with `spot-bundle-signing` and arrive as [`Verified`] on each [`Candidate`];
//! bundles are opened with `spot-bundle` and arrive as an [`AppManifest`](spot_manifest::AppManifest)
//! plus its canonical digest.
//!
//! # Building a plan
//!
//! ```
//! use semver::Version;
//! use spot_installer::{plan, Approval, Catalogue, Host, Request, SiteState};
//!
//! let site = SiteState::new();
//! let catalogue = Catalogue::new(Vec::new());
//! let host = Host::new(Version::new(0, 1, 0), "spot:app@0.3.0");
//! let request = Request::Uninstall { app: "erp_selling".to_owned(), purge: false };
//! // No such app is installed, so this is refused with `installer.not_installed`.
//! let err = plan(&site, &catalogue, &host, &request, &Approval::new()).unwrap_err();
//! assert_eq!(err.code, "installer.not_installed");
//! ```
//!
//! # Error codes
//!
//! Every refusal is a [`PlanError`] with a stable `installer.*` code, documented in `installer.toml`
//! in `spot-errors`:
//!
//! - `installer.unverified` — a needed bundle is not verified.
//! - `installer.requires_host` — a manifest's `requires_host` excludes this host.
//! - `installer.wit_world_mismatch` — a bundle targets another WIT world.
//! - `installer.downgrade` — a change would move an app down without a downgrade request.
//! - `installer.downgrade_unsafe` — an explicit downgrade would run a data or schema patch.
//! - `installer.digest_changed` — the same version is offered with a different digest.
//! - `installer.already_installed` — an install names an installed app.
//! - `installer.not_installed` — an upgrade, downgrade or uninstall names an absent app.
//! - `installer.has_dependents` — an uninstall would break a dependent.
//! - `installer.patch_removed` — a manifest drops a patch the site already applied.
//! - `installer.missing_dependency` — a required app has no catalogue bundle.
//! - `installer.conflicting_requirement` — no version satisfies every requirement.
//! - `installer.breaks_dependent` — a chosen version violates an installed app's requirement.
//! - `installer.dependency_cycle` — the dependencies form a cycle.
//! - `installer.no_candidate` — the catalogue holds no bundle matching a request.
//! - `installer.plan_not_approved` — the plan's hash is not the approved hash.
//! - `installer.needs_approval` — the plan waits for capability approval.
//! - `installer.lease_held` — another installer holds the site's lease.
//! - `installer.lease_lost` — the per-site lease was lost mid-run.
//! - `installer.rollback_failed` — a step failed and undoing an earlier step failed too.
//! - `installer.journal_corrupt` — the resume journal cannot be trusted.
//! - `installer.step_failed` — a trait method failed while running a step.
//! - `installer.interrupted` — `resume` put back a run that stopped before its commit.
//! - `installer.no_plan` — `run_many` was given a site with no approved plan.

#![forbid(unsafe_code)]

mod error;
mod executor;
mod journal;
mod model;
mod outcome;
mod plan;
mod resolve;
mod steps;
mod traits;

#[cfg(feature = "testing")]
pub mod testing;

pub use error::PlanError;
pub use executor::{ApprovedPlan, DEFAULT_LEASE_TTL, Executor, Options};
pub use journal::{
    InstallJournal, LoadedComponent, SiteLifecycle, SnapshotId, StepId, StepKind, StepState, Undo,
};
pub use model::{
    Approval, Candidate, CapabilityDelta, CapabilityRequest, Catalogue, GrantedCapabilities, Host,
    InstalledApp, PlanState, Request, SiteState, Verified,
};
pub use outcome::{ExecError, Failure, Outcome};
pub use plan::{Action, InstallPlan, Operation, PatchStep, Risk};
pub use resolve::plan;
pub use traits::{
    AppRegistryStore, BoxFuture, ComponentHost, FixtureLoader, InstallChange, LifecycleConflict,
    LifecycleError, PatchOutcome, PatchRunner, Snapshotter, UpgradeChange,
};

/// The capability vocabulary an approval and a delta are written in, re-exported from
/// `spot-manifest` so a caller needs no second import.
pub use spot_manifest::{DocTypeAccess, Endpoint, FileAccess};
