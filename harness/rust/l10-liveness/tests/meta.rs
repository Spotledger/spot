//! The L10 metadata liveness suite on the `MetaHost` adapter over `spot_meta::Registry`.
//!
//! `MetaHost` (below, test-side like the L3 suite's stub fixture) wraps a `Registry` and
//! implements [`LivenessHost`]: the change inputs convert to registry calls, the merged
//! metadata is the registry's `Arc<EffectiveMeta>`, and every probe answers `NotSupported`
//! (the registry has no plan, schema-migration or view layer of its own — WP-MET-02, WP-STO-11
//! and WP-UI-10 supply the hosts that do).
//!
//! 15 of the 16 scenarios pass here. The 16th
//! (`removing_a_custom_field_updates_both_views_and_shares_unrelated_views`) asserts that an
//! unrelated DocType's compiled view keeps the **same `Arc`** across a change to another
//! DocType; `spot_meta::Registry` rebuilds *every* view on *every* change
//! (`Registry::change`), so the scenario cannot pass on this host today. Per the brief it is
//! `#[ignore]`d with the reason and listed under Open questions — never weakened: the
//! `#[ignore]`d test below runs the actual scenario and asserts that it fails **only** on the
//! sharing assertion (every other assertion of the scenario passes here).

#![allow(missing_docs)]

use spot_l10_liveness::{
    Change, ChangeEffect, HostError, HostFactory, LivenessHost, Outcome, Probe, ProbeOutcome,
    StandardHost, run_all, run_one, scenario_names,
};
use spot_meta::{Definition, EffectiveMeta, Registry};
use std::sync::Arc;

/// The scenario the registry cannot satisfy today, with the reason (also in the README and in
/// the `#[ignore]` below).
const CANNOT_PASS_ON_SPOT_META: &str =
    "removing_a_custom_field_updates_both_views_and_shares_unrelated_views";

/// The `MetaHost` adapter over `spot_meta::Registry`: the suite's host today.
struct MetaHost(StandardHost);

/// Supplies fresh `MetaHost`s.
struct MetaFactory;

impl HostFactory for MetaFactory {
    fn fresh(&self) -> Box<dyn LivenessHost> {
        Box::new(MetaHost(StandardHost::new()))
    }
}

impl LivenessHost for MetaHost {
    fn apply(&mut self, change: &Change) -> Result<ChangeEffect, HostError> {
        self.0.apply(change)
    }

    fn meta(&self) -> Arc<EffectiveMeta> {
        self.0.meta()
    }

    fn probe(&mut self, _layer: Probe) -> ProbeOutcome {
        // No plan (WP-MET-02), no schema-migration state (WP-STO-11), no view layer
        // (WP-UI-10) exists on the registry. The suite's cross-layer scenarios record this.
        ProbeOutcome::NotSupported
    }
}

/// A registry built directly, for the ignore-reason proof below (a bare `Registry`, not the
/// suite's adapter, to show the gap is the registry's own behaviour).
fn bare_registry() -> Registry {
    let defs: Vec<Definition> = vec![spot_l10_liveness::definition_of(
        &spot_l10_liveness::DocTypeDefInput::new(
            "party",
            vec![spot_l10_liveness::FieldSpecInput::text("code").with_required(true)],
        )
        .with_label("Party")
        .with_title_field("code"),
    )];
    Registry::new(defs).unwrap_or_else(|e| panic!("the definition does not build: {e:?}"))
}

/// 15 scenarios run and pass on the registry adapter; the suite stays complete (the 16th is
/// the one `#[ignore]`d below, excluded here so nothing is silently weakened).
#[test]
fn spot_meta_registry_passes_the_liveness_suite() {
    let all = scenario_names();
    assert_eq!(
        all.len(),
        16,
        "the suite must hold every scenario of the brief"
    );
    assert!(
        all.contains(&CANNOT_PASS_ON_SPOT_META),
        "the excluded scenario is still in the suite table"
    );

    let report = run_all(&MetaFactory);
    assert_eq!(
        report.outcomes.len(),
        16,
        "run_all reports every scenario, the impossible one with its recorded failure"
    );
    let failed: Vec<&Outcome> = report.outcomes.iter().filter(|o| !o.passed).collect();
    assert_eq!(
        failed.len(),
        1,
        "exactly the one unsatisfiable scenario fails on this host: {failed:?}"
    );
    assert_eq!(failed[0].scenario, CANNOT_PASS_ON_SPOT_META);
    assert!(
        failed[0]
            .failure
            .as_deref()
            .is_some_and(|f| f.contains("same Arc")),
        "the failure is the sharing assertion, nothing else: {:?}",
        failed[0].failure
    );

    // Every other scenario passes.
    let passed: Vec<&Outcome> = report.outcomes.iter().filter(|o| o.passed).collect();
    assert_eq!(
        passed.len(),
        15,
        "the 15 satisfiable scenarios pass: {passed:?}"
    );
}

/// The scenario runs one by one (the `run_one` seam a host adapter with per-scenario ignores
/// uses) and the 15 satisfiable ones pass individually too.
#[test]
fn the_satisfiable_scenarios_pass_individually() {
    for name in scenario_names() {
        if name == CANNOT_PASS_ON_SPOT_META {
            continue;
        }
        let outcome = run_one(&MetaFactory, name).unwrap_or_else(|| panic!("{name} exists"));
        assert!(outcome.passed, "{name}: {:?}", outcome.failure);
    }
}

/// The ignored scenario's failure is exactly the sharing gap: run it and inspect. This is the
/// `#[ignore]` the brief asks for, with the reason; deleting this test or flipping the
/// scenario's assertion would be weakening.
#[test]
#[ignore = "spot_meta::Registry rebuilds every DocType's compiled view on every change, so an \
            unrelated DocType's view is a new Arc, not the same Arc; ADR-05's snapshot \
            consistency holds today, the sharing this scenario pins does not exist yet"]
fn the_unshared_views_scenario_fails_only_on_sharing() {
    let outcome = run_one(&MetaFactory, CANNOT_PASS_ON_SPOT_META)
        .unwrap_or_else(|| panic!("the scenario exists"));
    assert!(
        !outcome.passed,
        "on spot-meta today the scenario cannot pass: {:?}",
        outcome.failure
    );
    let failure = outcome
        .failure
        .as_deref()
        .unwrap_or_else(|| panic!("the failure names the sharing gap"));
    assert!(
        failure.contains("same Arc"),
        "the failure is the sharing assertion itself: {failure}"
    );
    // The registry demonstrably rebuilds every view per change, even an untouched DocType's.
    let mut reg = bare_registry();
    let before = reg.effective();
    let v1 = reg
        .add_custom_field(
            "party",
            spot_l10_liveness::FieldSpecInput::text("custom_x").into_meta_spec(),
            None,
        )
        .unwrap_or_else(|e| panic!("the custom field builds: {e:?}"));
    assert_eq!(v1, before.version() + 1);
    // The whole EffectiveMeta is a fresh Arc (the doc comment on `Registry::change` says so,
    // and the type gives no per-view reuse), so no held view Arc can survive a change.
    assert!(
        !Arc::ptr_eq(&before, &reg.effective()),
        "a change replaces the whole effective metadata Arc (Registry::change)"
    );
    let old_view = before
        .doctype("party")
        .unwrap_or_else(|| panic!("the view"));
    let new_view = reg
        .effective()
        .doctype("party")
        .unwrap_or_else(|| panic!("the view"));
    assert!(
        !Arc::ptr_eq(&old_view, &new_view),
        "an unrelated DocType's view is rebuilt too: the sharing the scenario pins is absent"
    );
}

/// The suite's own reference fixture passes the 15 satisfiable scenarios as well, so the
/// library's default host and the test adapter agree (no host-specific behaviour).
#[test]
fn the_standard_factory_passes_the_satisfiable_scenarios() {
    use spot_l10_liveness::StandardFactory;
    for name in scenario_names() {
        if name == CANNOT_PASS_ON_SPOT_META {
            continue;
        }
        let outcome = run_one(&StandardFactory, name).unwrap_or_else(|| panic!("{name} exists"));
        assert!(outcome.passed, "{name}: {:?}", outcome.failure);
    }
}
