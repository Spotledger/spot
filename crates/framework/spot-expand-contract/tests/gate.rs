//! The usage gate: a contract step's release against the previous release's usage.
//!
//! The rule is ADR-22's: a contract step ships one release after the code stopped using the
//! object. A rename is the worked example — expand and backfill in release 1, contract in
//! release 2 only once release 1's code stopped touching the old name.

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use common::{blocked, change, manifest, plan, release_id};
use spot_expand_contract::{Earliest, Phase, Release, check, codes};
use spot_migration_plan::risk::rules;
use spot_migration_plan::{ChangeKind, Risk};

fn expand_plan() -> spot_migration_plan::ReleasePlan {
    plan(vec![
        change(
            "schema/add_column:party.new_name",
            ChangeKind::SchemaAddColumn,
            rules::SCHEMA_ADD_COLUMN,
            "party.new_name",
            Risk::Additive,
        ),
        change(
            "schema/backfill:party.new_name",
            ChangeKind::SchemaBackfill,
            rules::SCHEMA_BACKFILL,
            "party.new_name",
            Risk::NeedsReview,
        ),
    ])
}

#[test]
fn a_contract_in_the_same_release_as_the_stop_is_refused() {
    let release_plan = plan(vec![blocked("party.note")]);
    let id = release_id("2.0.0");
    let next = release_id("2.1.0");
    let usage = manifest("2.0.0", &[]);
    let previous = manifest("1.0.0", &["party.note"]);
    let release = Release {
        id: &id,
        next: Some(&next),
        plan: &release_plan,
        usage: &usage,
    };

    let report = check(&release, &previous);
    assert!(!report.is_ok());
    assert!(report.accepted.is_empty());
    assert_eq!(report.refusals.len(), 1);

    let refusal = &report.refusals[0];
    assert_eq!(refusal.code, codes::CONTRACT_TOO_EARLY);
    assert_eq!(refusal.object.as_ref().unwrap().to_string(), "party.note");
    assert_eq!(refusal.still_used_by, release_id("1.0.0"));
    assert_eq!(refusal.stopped_using, Some(release_id("2.0.0")));
    assert_eq!(
        refusal.earliest_allowed,
        Earliest::Release(release_id("2.1.0"))
    );
    assert!(refusal.detail.contains("party.note"));
    assert!(refusal.detail.contains("2.1.0"));
}

#[test]
fn a_contract_one_release_after_the_stop_is_allowed() {
    let release_plan = plan(vec![blocked("party.note")]);
    let id = release_id("2.0.0");
    let usage = manifest("2.0.0", &["party.new_name"]);
    // The release before this one already stopped using the old column.
    let previous = manifest("1.0.0", &["party.new_name"]);
    let release = Release {
        id: &id,
        next: None,
        plan: &release_plan,
        usage: &usage,
    };

    let report = check(&release, &previous);
    assert!(report.is_ok());
    assert_eq!(report.accepted.len(), 1);
    assert_eq!(report.accepted[0].object.to_string(), "party.note");
    assert_eq!(report.accepted[0].change_id, "schema/refused/party.note");
}

#[test]
fn a_rename_across_two_releases_is_allowed() {
    // Release 1 expands and backfills and its code no longer touches the old column.
    let release_one_id = release_id("1.0.0");
    let release_one = Release {
        id: &release_one_id,
        next: None,
        plan: &expand_plan(),
        usage: &manifest("1.0.0", &["party.new_name"]),
    };
    let before_release_one = manifest("0.9.0", &["party.note", "party.new_name"]);
    assert!(check(&release_one, &before_release_one).is_ok());

    // Release 2 contracts the old column; release 1 stopped using it, so it may ship.
    let release_two_plan = plan(vec![blocked("party.note")]);
    let release_two_id = release_id("2.0.0");
    let release_three_id = release_id("2.1.0");
    let release_two = Release {
        id: &release_two_id,
        next: Some(&release_three_id),
        plan: &release_two_plan,
        usage: &manifest("2.0.0", &["party.new_name"]),
    };
    let report = check(&release_two, &manifest("1.0.0", &["party.new_name"]));
    assert!(report.is_ok(), "{}", report.render_text());
    assert_eq!(report.accepted.len(), 1);
    assert_eq!(report.accepted[0].object.to_string(), "party.note");
}

#[test]
fn a_rename_that_stops_and_contracts_together_is_refused() {
    let release_two_plan = plan(vec![blocked("party.note")]);
    let release_two_id = release_id("2.0.0");
    let release_two = Release {
        id: &release_two_id,
        next: None,
        plan: &release_two_plan,
        usage: &manifest("2.0.0", &["party.new_name"]),
    };
    // Release 1 still writes the old column, so release 2 is the release that stops using it.
    let report = check(
        &release_two,
        &manifest("1.0.0", &["party.note", "party.new_name"]),
    );
    assert!(!report.is_ok());
    assert_eq!(report.refusals[0].code, codes::CONTRACT_TOO_EARLY);
    // The train has no name for the release after this one yet.
    assert_eq!(
        report.refusals[0].earliest_allowed,
        Earliest::After(release_id("2.0.0"))
    );
}

#[test]
fn a_contract_this_release_still_uses_is_refused() {
    let release_plan = plan(vec![blocked("party.note")]);
    let id = release_id("2.0.0");
    let usage = manifest("2.0.0", &["party.note"]);
    let release = Release {
        id: &id,
        next: None,
        plan: &release_plan,
        usage: &usage,
    };
    let report = check(&release, &manifest("1.0.0", &["party.note"]));
    assert!(!report.is_ok());
    assert_eq!(report.refusals[0].code, codes::CONTRACT_WHILE_USED);
    assert_eq!(report.refusals[0].still_used_by, release_id("2.0.0"));
    assert_eq!(report.refusals[0].stopped_using, None);
}

#[test]
fn expand_and_migrate_steps_are_not_gated() {
    let id = release_id("2.0.0");
    let usage = manifest("2.0.0", &["party.note"]);
    let release = Release {
        id: &id,
        next: None,
        plan: &expand_plan(),
        usage: &usage,
    };
    // Even though the previous release touches the column and this release adds another one,
    // only contract steps are checked.
    let report = check(&release, &manifest("1.0.0", &["party.note"]));
    assert!(report.is_ok());
    assert!(report.accepted.is_empty());
    assert!(report.refusals.is_empty());
    assert_eq!(
        report.render_text(),
        "expand/contract gate: 0 contract step(s) allowed\n"
    );
}

#[test]
fn a_table_contract_against_a_column_user_is_refused() {
    // Dropping the whole table is refused while the previous release touches any of its columns.
    let release_plan = plan(vec![blocked("party")]);
    let id = release_id("2.0.0");
    let usage = manifest("2.0.0", &[]);
    let release = Release {
        id: &id,
        next: None,
        plan: &release_plan,
        usage: &usage,
    };
    let report = check(&release, &manifest("1.0.0", &["party.note"]));
    assert!(!report.is_ok());
    assert_eq!(
        report.refusals[0].object.as_ref().unwrap().to_string(),
        "party"
    );
    assert!(report.refusals[0].object.as_ref().unwrap().is_table());
}

#[test]
fn the_report_renders_the_refusal_and_the_phase() {
    let release_plan = plan(vec![blocked("party.note")]);
    let id = release_id("2.0.0");
    let next = release_id("2.1.0");
    let usage = manifest("2.0.0", &[]);
    let release = Release {
        id: &id,
        next: Some(&next),
        plan: &release_plan,
        usage: &usage,
    };
    let report = check(&release, &manifest("1.0.0", &["party.note"]));
    let text = report.render_text();
    assert!(text.contains("1 of 1 contract step(s) refused"));
    assert!(text.contains(codes::CONTRACT_TOO_EARLY));
    assert_eq!(report.refusals().len(), 1);
    assert_eq!(
        spot_expand_contract::classify(&release_plan)[0].phase,
        Phase::Contract
    );
}

#[test]
fn dropping_a_traversal_index_is_a_contract_the_gate_holds_back() {
    // The drop classifies as Contract, so the gate treats it like any removal: refused while the
    // previous release's code may still rely on the index, allowed one release after.
    let drop = change(
        "schema/traversal_index/drop:edge.gx_edge_parent_rev",
        ChangeKind::SchemaDropIndex,
        rules::SCHEMA_DROP_TRAVERSAL_INDEX,
        "edge.gx_edge_parent_rev",
        Risk::NeedsReview,
    );
    let release_plan = plan(vec![drop]);
    let id = release_id("2.0.0");
    let usage = manifest("2.0.0", &[]);

    let release = Release {
        id: &id,
        next: None,
        plan: &release_plan,
        usage: &usage,
    };
    let early = check(
        &release,
        &manifest("1.0.0", &["index:edge.gx_edge_parent_rev"]),
    );
    assert!(!early.is_ok());
    assert_eq!(early.refusals[0].code, codes::CONTRACT_TOO_EARLY);

    let late = check(&release, &manifest("1.0.0", &[]));
    assert!(late.is_ok(), "{:?}", late.refusals);
    assert_eq!(late.accepted.len(), 1);

    // The same release still using the index is refused outright.
    let using = manifest("2.0.0", &["index:edge.gx_edge_parent_rev"]);
    let release = Release {
        id: &id,
        next: None,
        plan: &release_plan,
        usage: &using,
    };
    let still = check(&release, &manifest("1.0.0", &[]));
    assert_eq!(still.refusals[0].code, codes::CONTRACT_WHILE_USED);
}

#[test]
fn a_drop_index_change_in_an_expand_release_is_still_a_contract_step() {
    let mut changes = expand_plan().changes;
    changes.push(change(
        "schema/traversal_index/drop:party.gx_party_owner_rev",
        ChangeKind::SchemaDropIndex,
        rules::SCHEMA_DROP_TRAVERSAL_INDEX,
        "party.gx_party_owner_rev",
        Risk::NeedsReview,
    ));
    let release_plan = plan(changes);
    let id = release_id("2.0.0");
    let usage = manifest("2.0.0", &[]);
    let next = release_id("2.1.0");
    let release = Release {
        id: &id,
        next: Some(&next),
        plan: &release_plan,
        usage: &usage,
    };
    let report = check(
        &release,
        &manifest("1.0.0", &["index:party.gx_party_owner_rev"]),
    );
    assert_eq!(report.refusals.len(), 1);
    assert_eq!(
        report.refusals[0].change_id,
        "schema/traversal_index/drop:party.gx_party_owner_rev"
    );
    assert_eq!(report.refusals[0].code, codes::CONTRACT_TOO_EARLY);
    // The expand and migrate steps of the same plan are not held back.
    assert!(report.accepted.is_empty());
}
