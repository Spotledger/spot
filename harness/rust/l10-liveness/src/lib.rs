//! L10 metadata liveness suite (WP-TST-11; ADR-05).
//!
//! ADR-05: **nothing a user changes in the UI may need a rebuild or restart.** This crate pins
//! that promise as scripted scenarios ([`run_all`]) over a host-neutral trait,
//! [`LivenessHost`]. The suite exists before the things that must satisfy it: the compiled
//! plans (WP-MET-02), the schema migration engine (WP-STO-11) and the view layer (WP-UI-10)
//! each plug in through a [`Probe`] and run the same scenarios. Today it runs green on
//! [`spot_meta::Registry`] alone, through the [`StandardHost`] adapter (`tests/meta.rs`).
//!
//! A scenario drives one [`Change`] at a time through [`LivenessHost::apply`], reads the merged
//! metadata through [`LivenessHost::meta`], and — where the host has the layer — consults a
//! [`Probe`] (a plan probe, a schema probe, a view probe) to confirm the change reached that
//! layer **within the same `apply` call**. A host without a layer returns
//! [`ProbeOutcome::NotSupported`]; the scenario then skips that part and the report says so in
//! the outcome's notes, so a layer can join later without the suite changing.
//!
//! Every scenario cites the decision record it enforces (ADR-05, or ADR-068 §1/§7 for what a
//! site may and may not change) in a comment beside its implementation and in `README.md`.

use std::sync::Arc;

use spot_meta::EffectiveMeta;

pub(crate) mod cases_meta_updates;
pub(crate) mod cases_single_change;
pub mod change;
mod checks;
mod host;
pub mod report;
mod scenarios;

pub use change::{
    AddDocTypeDef, Change, ChangeEffect, DocTypeDefInput, FieldSpecInput, HostError, Refusal,
    SetProperty,
};
pub use host::{StandardFactory, StandardHost, definition_of, property_of, refusal_of, spec_of};
pub use report::{Outcome, Report};

/// One metadata layer a host may expose to the suite.
///
/// The plan probe is WP-MET-02, the schema probe WP-STO-11, the view probe WP-UI-10. A host
/// without a layer returns [`ProbeOutcome::NotSupported`] and scenarios skip it, saying so.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Probe {
    /// The compiled execution plans (field index, validation, naming): WP-MET-02.
    Plan,
    /// The storage schema the migration engine would apply next: WP-STO-11.
    Schema,
    /// The UI view layer (groups, tabs, order): WP-UI-10.
    View,
}

/// What one probe saw.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// The host has no such layer; the scenario says so in its notes.
    NotSupported,
    /// The layer exists and already sees this metadata version.
    Live {
        /// The metadata version the probe observed.
        version: u64,
    },
    /// The layer exists but still sees an older version than `seen` — a liveness failure.
    Stale {
        /// The version the probe observed.
        observed: u64,
        /// The version the metadata is at.
        seen: u64,
    },
}

/// The host contract: the minimum the L10 scenarios need (ADR-05).
///
/// Implement this (with [`StandardHost`] as the reference) to run the suite on another layer
/// stack; see `README.md`, "How to add a host adapter". Every scenario runs on a fresh host
/// from a [`HostFactory`], so scenarios never see each other's changes.
pub trait LivenessHost {
    /// Applies one change. On `Err` nothing may have changed: the metadata, its version and
    /// the customisations stay exactly as before — a refused edit must never leave a
    /// half-applied site (ADR-05).
    fn apply(&mut self, change: &Change) -> Result<ChangeEffect, HostError>;

    /// The currently effective, merged metadata. Immutable: readers holding a previously
    /// returned [`Arc`] keep a consistent snapshot while further changes land.
    fn meta(&self) -> Arc<EffectiveMeta>;

    /// Asks one optional layer whether it already sees the metadata it was handed. The default
    /// is "not supported", so a metadata-only host needs no probes (the brief's `probes()`:
    /// returning [`ProbeOutcome::NotSupported`] makes a scenario skip that probe and say so in
    /// the report).
    fn probe(&mut self, _layer: Probe) -> ProbeOutcome {
        ProbeOutcome::NotSupported
    }
}

/// Builds fresh hosts, one per scenario (the L3 suite's `Fixture`, for this suite).
pub trait HostFactory: Sync {
    /// A new host over the standard metadata, with no customisations.
    fn fresh(&self) -> Box<dyn LivenessHost>;
}

/// How long one change in the 200-change scenario may take. Generous by design: L10 proves
/// liveness, not speed (that is L9's job); per-change wall time is also recorded in the
/// report's notes and the README states the bound. Only a gross regression — a change that
/// costs a rebuild-shaped fraction of a second — fails.
pub const SEQUENTIAL_BUDGET: std::time::Duration = std::time::Duration::from_millis(500);

/// The suite, every scenario on a fresh host from `factory`.
///
/// Returns a [`Report`]; a scenario that fails is recorded with its reason, it never aborts
/// the run. Callers assert `report.assert_all_passed()` — or, for a host with a scenario that
/// genuinely cannot pass (none on the reference host once `spot-meta` shares views; see the
/// README's open questions), inspect the named outcome's failure.
pub fn run_all(factory: &dyn HostFactory) -> Report {
    let mut outcomes = Vec::new();
    for (name, runner) in scenarios::ALL {
        outcomes.push(scenarios::run_scenario(factory, name, &runner));
    }
    Report { outcomes }
}

/// The names of every scenario, in suite order.
#[must_use]
pub fn scenario_names() -> Vec<&'static str> {
    scenarios::ALL.iter().map(|(name, _)| *name).collect()
}

/// Runs one scenario by name on a fresh host from `factory`; `None` when there is no scenario
/// of that name. This is what an `#[ignore]`d per-scenario test calls.
#[must_use]
pub fn run_one(factory: &dyn HostFactory, scenario: &str) -> Option<Outcome> {
    scenarios::ALL
        .iter()
        .find(|(name, _)| *name == scenario)
        .map(|(name, runner)| scenarios::run_scenario(factory, name, runner))
}
