//! The step and seam registry (WP-EXT-02.T1; blueprint WS6; ADR-08, ADR-17, ADR-057).
//!
//! The document engine (WP-DOC-04, WP-DOC-10) asks this crate which steps run at each hook
//! point, in which order, with which overrides applied, and which provider answers each seam.
//! It decides; it never runs a step.
//!
//! 1. Apps contribute through a [`RegistryBuilder`]: controllers, [`StepOverride`]s, seam
//!    providers and explicit seam replacements. An app that needs a seam at a major version
//!    states the requirement through [`RegistryBuilder::require_seam`]; it is checked with
//!    everything else at build time (WP-EXT-03).
//! 2. [`RegistryBuilder::build`] checks everything together and reports **every** [`Conflict`]
//!    at once. These are the install-time checks of WP-EXT-03: an app that conflicts is refused.
//! 3. A [`Registry`] answers [`Registry::plan`] with a precomputed slice (no allocation per call),
//!    [`Registry::explain`] with the reasons, implements [`Seams`](spot_app_api::Seams), and
//!    prints the site's overrides and seams as data ([`Registry::override_report`]) for the
//!    admin listing (WP-EXT-03).
//!
//! Effect declarations (ADR-080 6.3) ride on the steps: see the [`effects`](Registry::effects)
//! method, [`check_receipt`] and the `effects.*` conflict codes.
//!
//! Registration order never changes the result. The override rules ADR-057 leaves open are
//! listed on [`RegistryBuilder::build`].
//!
//! [`StepOverride`]: spot_app_api::StepOverride

mod builder;
mod conflict;
mod effects;
mod explain;
mod platform_classes;
mod registry;
mod report;

pub use builder::RegistryBuilder;
pub use conflict::{Conflict, codes};
pub use effects::{DeclaredEffect, EffectChange, Undeclared, UndeclaredReason, check_receipt};
pub use explain::{AppliedOverride, ExplainLine, Role};
pub use platform_classes::{PLATFORM_EFFECT_CLASSES, platform_effect_classes};
pub use registry::{InstalledOverride, PlannedStep, Registry, SeamInfo, WrappedStep};
pub use report::{OverrideReport, OverrideRow, OverrideRowKind, SeamRow};
