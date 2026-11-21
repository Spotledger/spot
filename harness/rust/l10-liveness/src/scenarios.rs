//! The suite's scenario table and runner. Each scenario is a comment-cited ADR rule,
//! implemented in the group modules beside its peers, run on a fresh host from the caller's
//! [`HostFactory`](crate::HostFactory) and recorded here as an
//! [`Outcome`](crate::Outcome).
//!
//! Signatures: most scenarios drive one fresh host; the commuting and determinism scenarios
//! build their own hosts, because they exist to compare two sequences on equal footing.

use crate::change::{DocTypeDefInput, FieldSpecInput};
use crate::{HostFactory, LivenessHost, Outcome};

use crate::cases_meta_updates::{
    s08_update_keeps_customisations, s09_update_removal_and_collision,
    s10_commuting_and_order_dependent_pairs, s11_replay_determinism,
};
use crate::cases_single_change::{
    s01_custom_field_visible_version_plus_one, s02_field_in_both_views, s03_override_islands,
    s04_removal_updates_views_snapshot_arc, s05_runtime_doctype_engine_fields,
    s06_doctype_name_collision_refused, s07_refusal_leaves_metadata_intact,
    s12_reader_snapshot_consistency, s13_two_hundred_changes_budget,
    s14_allow_on_posted_reaches_app_view, s15_reserved_names_refused,
    s16_probes_see_the_version_in_apply,
};

/// Runs one scenario on a fresh host from `factory`, mapping failure into the outcome. A
/// failing scenario never aborts the run; its reason is recorded for the report.
pub(crate) fn run_scenario(
    factory: &dyn HostFactory,
    name: &'static str,
    runner: &Runner,
) -> Outcome {
    let result = match runner {
        Runner::Host(run) => {
            let mut host = factory.fresh();
            run(&mut *host)
        }
        Runner::OwnHosts(run) => run(),
    };
    match result {
        Ok(notes) => Outcome {
            scenario: name,
            passed: true,
            failure: None,
            notes,
        },
        Err(why) => Outcome {
            scenario: name,
            passed: false,
            failure: Some(why),
            notes: Vec::new(),
        },
    }
}

/// The runner shape each scenario takes: over a fresh host from the factory, or building its
/// own hosts (the commuting and determinism scenarios, which compare two sequences).
pub(crate) enum Runner {
    /// A scenario over a fresh host.
    Host(fn(&mut dyn LivenessHost) -> Result<Vec<String>, String>),
    /// A scenario that builds its own hosts (the commuting and replay scenarios).
    OwnHosts(fn() -> Result<Vec<String>, String>),
}

/// The `party` DocType: not posted; required text `code` (its title), select `state`.
pub(crate) fn party_input() -> DocTypeDefInput {
    DocTypeDefInput::new(
        "party",
        vec![
            FieldSpecInput::text("code").with_required(true),
            FieldSpecInput::select("state", &["Active", "Inactive"]).with_default("Active"),
        ],
    )
    .with_label("Party")
    .with_title_field("code")
}

/// The `invoice` DocType: posted; text `number` (its title), text `party_code`, decimal
/// `total`, select `state`.
fn invoice_input() -> DocTypeDefInput {
    DocTypeDefInput::new(
        "invoice",
        vec![
            FieldSpecInput::text("number").with_required(true),
            FieldSpecInput::text("party_code"),
            FieldSpecInput::decimal("total", 2),
            FieldSpecInput::select("state", &["Draft", "Sent", "Paid"]).with_default("Draft"),
        ],
    )
    .with_label("Invoice")
    .posted()
    .with_title_field("number")
}

/// The standard set every fresh host starts from.
pub(crate) fn standard_defs() -> Vec<DocTypeDefInput> {
    vec![party_input(), invoice_input()]
}

/// Every scenario, in suite order. Each cites the ADR it enforces in a comment beside its
/// implementation (in the `cases_*` modules) and in `README.md`.
pub(crate) const ALL: [(&str, Runner); 16] = [
    (
        "custom_field_is_immediately_visible_and_version_moves_by_one",
        Runner::Host(s01_custom_field_visible_version_plus_one),
    ),
    (
        "custom_field_reaches_catalog_and_doctype_views_in_one_version",
        Runner::Host(s02_field_in_both_views),
    ),
    (
        "property_override_takes_effect_and_touches_nothing_else",
        Runner::Host(s03_override_islands),
    ),
    (
        "removing_a_custom_field_updates_both_views_and_shares_unrelated_views",
        Runner::Host(s04_removal_updates_views_snapshot_arc),
    ),
    (
        "runtime_doctype_appears_with_engine_fields_and_a_valid_catalog",
        Runner::Host(s05_runtime_doctype_engine_fields),
    ),
    (
        "doctype_name_collision_is_refused_and_the_version_does_not_move",
        Runner::Host(s06_doctype_name_collision_refused),
    ),
    (
        "a_refused_change_leaves_the_previous_metadata_fully_intact",
        Runner::Host(s07_refusal_leaves_metadata_intact),
    ),
    (
        "app_update_adding_a_standard_field_keeps_site_customisations",
        Runner::Host(s08_update_keeps_customisations),
    ),
    (
        "app_update_removal_succeeds_and_a_colliding_update_is_refused_by_name",
        Runner::Host(s09_update_removal_and_collision),
    ),
    (
        "commuting_changes_agree_in_either_order_and_non_commuting_ones_differ_deterministically",
        Runner::OwnHosts(s10_commuting_and_order_dependent_pairs),
    ),
    (
        "replaying_a_change_sequence_from_empty_metadata_reproduces_the_same_version_and_views",
        Runner::OwnHosts(s11_replay_determinism),
    ),
    (
        "readers_holding_an_old_snapshot_see_a_consistent_metadata_while_changes_land",
        Runner::Host(s12_reader_snapshot_consistency),
    ),
    (
        "two_hundred_sequential_changes_stay_within_the_recorded_per_change_budget",
        Runner::Host(s13_two_hundred_changes_budget),
    ),
    (
        "allow_on_posted_override_is_reflected_in_the_app_api_view",
        Runner::Host(s14_allow_on_posted_reaches_app_view),
    ),
    (
        "engine_reserved_field_names_cannot_be_custom_field_names",
        Runner::Host(s15_reserved_names_refused),
    ),
    (
        "every_supported_probe_sees_the_new_version_within_the_same_apply",
        Runner::Host(s16_probes_see_the_version_in_apply),
    ),
];
