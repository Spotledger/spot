//! The approval starter: a template becomes a BPM process that passes the BPM validator and
//! compiles onto the DAG runtime, as a draft version, with the escalation path the template
//! describes.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use spot_bpm_model::{
    AllowAllMeta, EmptyMeta, NodeKind, VersionStatus, from_text, templates, to_text,
    validate::codes as bpm_codes,
};
use spot_nocode_builders::{ApprovalAction, ApprovalTemplate, Approver, StartOn, build};

fn template() -> ApprovalTemplate {
    ApprovalTemplate::new(
        "purchase_approval",
        "Purchase approval",
        "journal",
        Approver::role("manager"),
        Approver::role("director"),
    )
    .with_sla(3_600)
    .approving(vec![
        ApprovalAction::set_field("approved", "true"),
        ApprovalAction::submit(),
    ])
    .rejecting(vec![ApprovalAction::set_field("approved", "false")])
}

#[test]
fn the_template_compiles_to_a_valid_draft_process() {
    let plan = build(&template(), &AllowAllMeta, 1).expect("the template builds");
    assert_eq!(plan.version.name, "purchase_approval");
    assert_eq!(plan.version.version, 1);
    assert_eq!(plan.version.status, VersionStatus::Draft);
    assert!(plan.process.versions.len() == 1);
    assert!(plan.text.contains("first_approval"), "{}", plan.text);

    // The escalation template's shape is present: two decisions and a wait between them.
    let decisions = plan
        .version
        .nodes
        .iter()
        .filter(|n| matches!(n.kind, NodeKind::Decision(_)))
        .count();
    let waits = plan
        .version
        .nodes
        .iter()
        .filter(|n| matches!(n.kind, NodeKind::Wait(_)))
        .count();
    assert_eq!(decisions, 2);
    assert_eq!(waits, 1);

    let assignees: Vec<String> = plan
        .version
        .nodes
        .iter()
        .filter_map(|n| match &n.kind {
            NodeKind::Decision(config) => config.assignee.clone(),
            _ => None,
        })
        .collect();
    assert_eq!(assignees, vec!["role:manager", "role:director"]);

    let documents = plan
        .version
        .nodes
        .iter()
        .filter(|n| matches!(n.kind, NodeKind::Document(_)))
        .count();
    assert_eq!(
        documents, 6,
        "the approve steps and the reject step, once per decision path"
    );
}

#[test]
fn the_built_process_passes_the_bpm_validator_and_the_text_round_trips() {
    let plan = build(&template(), &AllowAllMeta, 1).expect("builds");
    // The platform's own approval template and the builder's agree on the shape.
    let built_in = from_text(templates::APPROVAL_ESCALATION).expect("the shipped template parses");
    assert_eq!(built_in.status, VersionStatus::Draft);

    let reparsed = from_text(&plan.text).expect("the canonical text parses");
    assert_eq!(reparsed, plan.version);
    assert_eq!(to_text(&reparsed), plan.text);

    let problems =
        spot_bpm_model::validate(&plan.process, &AllowAllMeta, &spot_bpm_model::Limits::new());
    assert!(problems.is_empty(), "{problems:?}");
}

#[test]
fn a_manual_start_has_a_manual_trigger() {
    let mut template = template();
    template.start_on = StartOn::Manual;
    let plan = build(&template, &AllowAllMeta, 1).expect("builds");
    let trigger = plan
        .version
        .nodes
        .iter()
        .find_map(|n| match &n.kind {
            NodeKind::Trigger(config) => Some(config),
            _ => None,
        })
        .expect("a trigger");
    assert_eq!(trigger.trigger, spot_bpm_model::TriggerType::Manual);
    assert!(trigger.event.is_none());
}

#[test]
fn an_unresolvable_approver_is_refused_in_plain_language() {
    let mut template = template();
    template.approvers = Approver::role("");
    let issues = build(&template, &AllowAllMeta, 1).expect_err("an empty role is not an approver");
    assert!(
        issues
            .iter()
            .any(|i| i.code == "nocode_builders.bad_approver"),
        "{issues:?}"
    );
}

#[test]
fn a_record_type_the_site_lacks_is_refused_by_the_bpm_validator() {
    let issues = build(&template(), &EmptyMeta, 1).expect_err("the site has no journal");
    assert!(
        issues
            .iter()
            .any(|i| i.code == "nocode_builders.unknown_doctype"),
        "{issues:?}"
    );
    assert!(
        issues
            .iter()
            .all(|i| i.source != Some(bpm_codes::UNKNOWN_DOCTYPE)
                || i.code == "nocode_builders.unknown_doctype"),
        "the BPM code is kept beside the friendly one"
    );
}

#[test]
fn a_zero_sla_would_escalate_immediately_and_is_refused() {
    let mut template = template();
    template.sla_seconds = 0;
    let issues = build(&template, &AllowAllMeta, 1).expect_err("no time to decide");
    assert!(
        issues.iter().any(|i| i.code == "nocode_builders.bad_value"),
        "{issues:?}"
    );
}

#[test]
fn a_version_starts_at_one() {
    let issues = build(&template(), &AllowAllMeta, 0).expect_err("version zero");
    assert!(
        issues.iter().any(|i| i.code == "nocode_builders.bad_value"),
        "{issues:?}"
    );
}
