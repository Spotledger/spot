//! Contract probes: the app's declared checks, run by the registry or by a test.
//!
//! A probe is the review's window onto the checks an app author can also run locally with
//! `spot-pack`. The review ships [`NullProbe`] (runs nothing) and [`SpotPackProbe`], which
//! re-runs `spot-pack`'s checks on an extracted app directory. A failing probe is a `High`
//! finding.

use std::path::PathBuf;

use spot_bundle::Bundle;

use crate::finding::{Finding, Severity};
use crate::rules;

/// One probe result, before it becomes a [`Finding`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbeFinding {
    /// The rule id the probe reports under.
    pub rule: String,
    /// The path the probe's problem concerns, when it is about one.
    pub where_: Option<String>,
    /// What the probe found, phrased for a reviewer.
    pub message: String,
}

/// A source of contract findings for a bundle.
///
/// The store registry implements this with the checks it already runs (DocType metadata,
/// permissions, fixtures); a test can implement it with an injected fault.
pub trait Probe: Send + Sync {
    /// Runs the probe against an opened bundle and reports what it found.
    fn probe(&self, bundle: &Bundle) -> Vec<ProbeFinding>;
}

/// The default probe: no contract checks. The report then carries no probe finding, which is
/// **not** a statement that the app is sound.
#[derive(Clone, Copy, Debug, Default)]
pub struct NullProbe;

impl Probe for NullProbe {
    fn probe(&self, _bundle: &Bundle) -> Vec<ProbeFinding> {
        Vec::new()
    }
}

/// A probe that runs `spot-pack`'s checks on an extracted app directory.
///
/// The directory is the app crate the registry holds, not the bundle (which carries no
/// `doctypes/`, `permissions/` or `fixtures/`); every `pack.*` problem becomes one
/// [`rules::PROBE_CONTRACT`] finding.
#[derive(Clone, Debug)]
pub struct SpotPackProbe {
    dir: PathBuf,
}

impl SpotPackProbe {
    /// A probe over the app directory at `dir`.
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The directory the probe checks.
    pub fn dir(&self) -> &std::path::Path {
        &self.dir
    }
}

impl Probe for SpotPackProbe {
    fn probe(&self, _bundle: &Bundle) -> Vec<ProbeFinding> {
        // Reuse the author-side check exactly; a review must be a superset of what an author
        // can run, never a fork of it.
        let report = spot_pack::check_app(&self.dir, None);
        report
            .problems
            .iter()
            .map(|problem| ProbeFinding {
                rule: rules::PROBE_CONTRACT.to_owned(),
                where_: problem.path.clone(),
                message: problem.to_string(),
            })
            .collect()
    }
}

/// Turns a probe's results into findings: every one is a `High` finding a person must read.
pub fn probe_findings(bundle: &Bundle, probe: &dyn Probe) -> Vec<Finding> {
    probe
        .probe(bundle)
        .into_iter()
        .map(|found| {
            Finding::new(
                found.rule,
                Severity::High,
                found.where_,
                found.message,
                rules::RISK_PUBLISHER,
            )
        })
        .collect()
}
