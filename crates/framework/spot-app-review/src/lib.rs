//! The app store review pipeline (WP-STR-05; ADR-21, blueprint WS11).
//!
//! One bundle goes in, one [`Report`] comes out: static checks, a security scan of the
//! manifest and the component's bytes, a bounded resource profile under the real WASM host, and
//! the contract probes the caller supplies. **The review never approves an app.** It produces
//! evidence; a person decides. The best verdict is [`Verdict::LooksClean`], which still means a
//! person must accept the app, and it is never a statement that the app is safe.
//!
//! # Shape
//!
//! - [`review`] is the whole pipeline: it opens the bundle, runs the four stages and assembles
//!   one report. It **never panics on any bytes**; a bundle that does not open yields a report
//!   with [`Verdict::Rejected`] and the `bundle.*` problems as findings, not an error.
//! - Each stage is also runnable alone: [`open_bundle`], [`static_findings`], [`security_scan`],
//!   [`resource_profile`] and [`probe_findings`].
//! - [`Finding`] carries a stable rule id, a severity, where it is, a message and the
//!   threat-model risk it maps to. Rules are one of this crate's `app_review.*` ids (catalogued
//!   in `crates/framework/spot-errors/catalogue/app_review.toml`) or a `bundle.*` code passed
//!   through from `spot-bundle`.
//! - [`Report`] is serde with a stable field order and a [`Report::to_markdown`] page for a
//!   human reviewer.
//!
//! # Determinism
//!
//! Two runs over the same bytes and the same configuration produce byte-identical JSON apart
//! from [`Report::generated_at`], which the caller passes in (the review never reads a clock).
//! The resource profile measures wall time with an injected [`Stopwatch`], so a test can make
//! it reproducible too.
//!
//! # What this crate does not do
//!
//! Approve, install or sign anything; choose the host's capability policy (that is the host's
//! decision); make a network call; or run any app code outside `spot-wasm`'s limits.

#![forbid(unsafe_code)]

mod config;
mod finding;
mod probe;
mod report;
mod resource;
mod rules;
mod scan;
mod stage;
mod stopwatch;
#[cfg(test)]
mod test_support;

pub use crate::config::{BundleLimits, CallLimits, HostLimits, ReviewConfig, ReviewConfigError};
pub use crate::finding::{Finding, Severity, SeverityCounts};
pub use crate::probe::{NullProbe, Probe, ProbeFinding, SpotPackProbe, probe_findings};
pub use crate::report::{CallOutcome, CallRecord, ManifestSummary, Report, Verdict};
pub use crate::resource::resource_profile;
pub use crate::scan::security_scan;
pub use crate::stage::{open_bundle, static_findings};
pub use crate::stopwatch::{Stopwatch, SystemStopwatch};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-app-review";

/// Reviews a bundle and returns one report, measuring the resource profile with the real
/// stopwatch and leaving [`Report::generated_at`] empty for the caller to fill in.
pub fn review(bytes: &[u8], config: &ReviewConfig, probe: &dyn Probe) -> Report {
    review_with_stopwatch(bytes, config, probe, &SystemStopwatch::new(), "")
}

/// Reviews a bundle with the caller's timestamp. The review never reads a clock, so two runs
/// with the same timestamp are byte-identical.
pub fn review_at(
    bytes: &[u8],
    config: &ReviewConfig,
    probe: &dyn Probe,
    generated_at: &str,
) -> Report {
    review_with_stopwatch(bytes, config, probe, &SystemStopwatch::new(), generated_at)
}

/// Reviews a bundle with an injected [`Stopwatch`] and the caller's timestamp: the whole
/// pipeline, with the resource profile's wall times drawn from `stopwatch`.
pub fn review_with_stopwatch(
    bytes: &[u8],
    config: &ReviewConfig,
    probe: &dyn Probe,
    stopwatch: &dyn Stopwatch,
    generated_at: &str,
) -> Report {
    let (bundle, mut findings) = open_bundle(bytes, config);
    let mut resources = Vec::new();
    let mut manifest = None;
    let mut digest = None;
    if let Some(bundle) = &bundle {
        findings.extend(static_findings(bundle));
        findings.extend(security_scan(bundle, config));
        let (calls, resource_findings) = resource_profile(bundle, config, stopwatch);
        resources = calls;
        findings.extend(resource_findings);
        findings.extend(probe_findings(bundle, probe));
        manifest = Some(ManifestSummary::from_manifest(bundle.manifest()));
        digest = Some(bundle.digest().to_hex());
    }
    Report::assemble(findings, resources, manifest, digest, generated_at)
}
