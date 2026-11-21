//! Site customization bundles (WP-ALM-01, blueprint WS19): one versioned, content-addressed
//! package that carries **everything a site changed on top of its apps** — custom fields and
//! property overrides, view layers, scripts, rules and fixtures — so it can be reviewed,
//! diffed, moved to another environment, merged and promoted with a rehearsal.
//!
//! This is not the app bundle (`spot-bundle` is a third-party app's signed WASM package).
//! It is the site's own delta. The two never mix: a customization bundle names the app
//! versions it was captured against and refuses to apply where they differ in a way that
//! matters (see [Compatibility](#compatibility)).
//!
//! # The format
//!
//! A deterministic ZIP (entries in byte-wise sorted name order, the fixed 1980 timestamp,
//! DEFLATE) containing:
//!
//! - `bundle.toml`: `format`, `name`, `bundle_version`, `created_by`, `created_at`,
//!   `captured_against`, `host_version` and the section index ([`BundleHeader`]);
//! - `digest`: the canonical SHA-256 of the content, so tampering is detected on open;
//! - `sections/<kind>/...`: each section's files.
//!
//! Sections shipped here:
//!
//! - `meta`: custom fields and property overrides, one TOML per DocType, overrides in the
//!   **existing** `key=value` text form (never a second encoding);
//! - `views`: one TOML per `site`, `role` or `step` layer ([`spot_views::ViewLayer`]);
//! - `scripts`: `.ts` sources plus a `scripts.toml` index (name, app, hook point, source hash);
//! - `rules`: one expression file per rule, each parsed by `spot-expr`;
//! - `fixtures`: one JSON array per DocType.
//!
//! # The digest (content addressing)
//!
//! SHA-256 over a fixed listing: `spotledger-custom-bundle-v1\0` then, per entry in byte-wise
//! sorted name order, `len(name) | name | len(content) | content` with fixed-width
//! little-endian lengths. The digest does not depend on ZIP ordering, timestamps or
//! compression, so the same content packs to the same digest whatever the insertion order.
//!
//! # Capture, plan, apply
//!
//! - [`capture`] reads a [`SiteReader`] and produces a [`Bundle`]; it carries only what
//!   differs from the apps (a redundant override is dropped) and excludes system-managed and
//!   secret items.
//! - [`plan_apply`] validates a bundle against a target site and returns an [`ApplyPlan`] of
//!   typed [`Change`]s (add, update, remove; each with a [`Risk`] and a deterministic hash).
//!   It validates **before** anything is written: `meta` is rebuilt through
//!   [`spot_meta::Registry::build`], every view layer is resolved and validated
//!   ([`spot_views::resolve`] and [`spot_views::validate_layer`]) and every rule parses.
//! - [`apply`] executes a plan through a [`SiteWriter`] in one
//!   `begin`/`commit`/`rollback`; a failure leaves the site unchanged. Applying twice yields
//!   the same state and an empty second plan.
//! - [`diff`] is the plain structural diff between two bundles (ALM-03 owns the merge).
//!
//! # Unknown sections
//!
//! A workflow, report, print template or BPM definition section does not exist yet. A bundle
//! that carries such a section must not break this reader: [`Bundle::open`] refuses it with
//! [`custom_bundle.unknown_section`](error::codes::UNKNOWN_SECTION) unless `allow_unknown` is
//! set, in which case its files are carried **byte for byte** and written back unchanged on
//! re-pack. An unknown section is never silently dropped.
//!
//! A later WP registers a section by implementing [`Section`] (whose documentation lists the
//! four rules) and calling [`SectionRegistry::register`]; nothing else here changes.
//!
//! # Error codes
//!
//! Every error is a [`CustomBundleError`] with a stable `custom_bundle.*` code; the code's
//! meaning and remedy live in `crates/framework/spot-errors/catalogue/custom_bundle.toml`.

#![forbid(unsafe_code)]

/// Planning and applying: validation, typed changes and the transaction.
mod apply;
/// The bundle: assembling, reading and re-packing.
mod bundle;
/// Capturing a site into a bundle.
mod capture;
pub mod digest;
pub mod error;
/// The in-memory site used by tests and the contract suite.
pub mod fake;
pub mod header;
pub mod limits;
mod paths;
pub mod registry;
pub mod section;
mod zip;

pub use apply::{ApplyPlan, ApplyReport, apply, diff, plan_apply};
pub use bundle::{Bundle, SectionData};
pub use capture::{CaptureOptions, capture, capture_with};
pub use digest::Digest;
pub use error::CustomBundleError;
pub use fake::FakeSite;
pub use header::{AppVersion, BundleHeader, SectionRef};
pub use limits::Limits;
pub use registry::{
    Change, ChangeAction, Files, FixtureSet, Problem, Risk, RuleRecord, ScriptRecord, Section,
    SectionRegistry, SiteReader, SiteWriter,
};
