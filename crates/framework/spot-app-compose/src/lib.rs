//! App-to-app composition (WP-STR-06.T1, ADR-21; blueprint WS11): one app extends another
//! through WIT imports.
//!
//! A regional tax app is not a copy of the accounts app: it **imports** an interface the accounts
//! app exports, is resolved against it at install, links to its component at run time and
//! contributes steps through the same registry as any native app. This crate is the part that
//! decides whether that composition is allowed and describes how it runs.
//!
//! # Shape
//!
//! - [`interface`]: the model. An [`ExportedInterface`] is `{ app, interface, version, functions }`
//!   plus the [`Capability`]s it grants to importers; an [`ImportedInterface`] is
//!   `{ app, interface, version_range, optional }`. An [`AppInterfaces`] is everything `resolve`
//!   reads about one app. The manifest has no field for these yet, so the resolution input is
//!   modelled here as data (the manifest extension is proposed under the task's open questions).
//! - [`resolve`]: match every import to an export by semver range, refuse a missing provider, a
//!   version conflict, a cycle between apps, a capability escalation and a grant a provider does
//!   not hold, and give the provider-before-consumer order and the candidate's blast radius.
//!   Everything is reported at once, as [`Problems`].
//! - [`link`]: [`ComposedInstance`], the description the `spot-wasm` host links from — which app
//!   answers which interface for which importer, how they pool, and the per-call fuel that is
//!   bounded by and charged to the **caller**, so an extension cannot make its provider run
//!   unbounded.
//! - [`seams`]: composition contributes its controllers, overrides and seam providers to
//!   `spot-seams`' `RegistryBuilder`, so an extension's overrides face the same conflict detection
//!   and produce the same `explain` output as any other app.
//! - [`lifecycle`]: uninstall refuses to remove a provider with dependents unless they go too or a
//!   replacement satisfies their ranges; an upgrade lists the dependents it impacts.
//!
//! # Determinism and purity
//!
//! Every function reads only its arguments: no storage, no network, no clock, no WebAssembly
//! execution, no floats, no `unsafe`. Reports are sorted and deduplicated, so the same input gives
//! the same bytes whatever order it arrived in.
//!
//! # Error codes
//!
//! Every refusal is a [`Problem`] with a stable `app_compose.*` code, catalogued in
//! `crates/framework/spot-errors/catalogue/app_compose.toml`:
//!
//! - `app_compose.duplicate_app` — an input names one app twice, or the candidate is installed.
//! - `app_compose.duplicate_export` — two apps export one interface at the same version.
//! - `app_compose.missing_provider` — an import has no provider and is not optional.
//! - `app_compose.version_conflict` — a provider exists but no version satisfies the range.
//! - `app_compose.cycle` — the compose graph is cyclic.
//! - `app_compose.capability_escalation` — an importing app declares a capability no provider grants.
//! - `app_compose.grant_not_held` — a provider grants a capability it does not hold.
//! - `app_compose.fuel_exhausted` — a composed call used more fuel than the caller's budget allows.
//! - `app_compose.uninstall_has_dependents` — a provider with dependents cannot be removed.
//! - `app_compose.replacement_incomplete` — a replacement does not satisfy a dependent's range.
//! - `app_compose.unknown_app` — an operation names an app that is not installed.
//! - `app_compose.unknown_interface` — a link names an interface the importer does not import.
//!
//! # What this crate does not do
//!
//! It does not open bundles, verify signatures, read manifests, touch storage or run a component.
//! Those are `spot-bundle`, `spot-bundle-signing`, `spot-manifest`, `spot-installer` and
//! `spot-wasm`; the composition here is the missing link between them.

#![forbid(unsafe_code)]

pub mod interface;
pub mod lifecycle;
pub mod link;
pub mod problem;
pub mod resolve;
pub mod seams;

pub use interface::{
    AppInterfaces, Capability, ExportedInterface, FunctionSig, ImportedInterface, ParamSig, TypeRef,
};
pub use lifecycle::{ImpactedDependent, UninstallPlan, UpgradePlan, uninstall, upgrade};
pub use link::{CallChain, CallStep, ComposedInstance, FuelCharge, InstanceSlot, Link, PoolPlan};
pub use problem::{Problem, Problems, codes};
pub use resolve::{Binding, Resolution, resolve};
pub use seams::{SeamNeed, SeamProviderDecl, StepContribution, build_registry};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-app-compose";
