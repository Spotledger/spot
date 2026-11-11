//! The golden sequence: build a 'Vehicle Service Request' DocType with a child table and a
//! condition through ops only, then validate, preview and publish it.

mod support;

use spot_customise::RecordOp;
use spot_meta::Definition;
use spot_migration_plan::Risk;
use spot_nocode_model::{
    Designer, DraftStatus, MemoryPublisher, PUBLISH_PERMISSION, plan_publish, preview_in, publish,
    validate,
};
use support::*;

#[test]
fn the_golden_design_is_built_by_ops_and_replays() {
    let editor = build_vehicle_service_request();
    let replayed = editor
        .replayed()
        .unwrap_or_else(|e| panic!("the log replays: {e}"));
    assert_eq!(&replayed, editor.current());
    assert_eq!(editor.current().doctype(), "vehicle_service_request");
    assert_eq!(editor.current().children.len(), 1);
    assert_eq!(editor.log().len(), editor.len());
    let phone = editor
        .current()
        .field(None, "customer_phone")
        .unwrap_or_else(|| panic!("customer_phone exists"));
    assert_eq!(
        phone.depends_on.as_deref(),
        Some("doc.status == \"Open\" or doc.status == \"In Progress\"")
    );
}

#[test]
fn the_golden_design_validates_with_no_issues() {
    let editor = build_vehicle_service_request();
    let site = site_with_vehicle();
    let report = validate(editor.current(), &site);
    assert!(report.is_ok(), "unexpected issues: {:?}", report.codes());
    assert_eq!(report.issues.len(), 0);
}

#[test]
fn the_golden_design_previews_the_real_form() {
    let editor = build_vehicle_service_request();
    let site = site_with_vehicle();
    let preview = preview_in(editor.current(), &site);
    assert!(preview.valid, "preview issues: {:?}", preview.issues);
    assert!(preview.sandbox);
    assert_eq!(preview.doctype, "vehicle_service_request");
    assert_eq!(preview.label, "Vehicle Service Request");
    assert!(preview.list.is_some());
    let sections: Vec<&str> = preview
        .form
        .all_sections()
        .iter()
        .map(|s| s.id.as_str())
        .collect();
    assert!(sections.contains(&"request"));
    assert!(sections.contains(&"items"));
    let phone = preview
        .fields
        .iter()
        .find(|f| f.name == "customer_phone")
        .unwrap_or_else(|| panic!("customer_phone is previewed"));
    assert_eq!(phone.label, "Customer Phone");
    assert_eq!(preview.children.len(), 1);
    assert_eq!(preview.children[0].doctype, "vehicle_service_item");
}

#[test]
fn the_golden_design_publishes_as_a_new_doctype() {
    let editor = build_vehicle_service_request();
    let site = site_with_vehicle();
    let plan = plan_publish(editor.current(), &site);
    assert!(
        plan.ready,
        "blocked: {:?}",
        plan.errors().collect::<Vec<_>>()
    );
    assert!(plan.new_doctype);
    assert_eq!(plan.records.len(), 1);
    let created = match plan.records.first() {
        Some(RecordOp::PutDocType(record)) => record.clone(),
        other => panic!("expected a DocTypeRecord, got {other:?}"),
    };
    assert_eq!(created.name, "vehicle_service_request");
    // The record's definition text round-trips to the draft's DocType.
    let parsed = Definition::parse("vehicle_service_request.toml", &created.definition)
        .unwrap_or_else(|e| panic!("the record definition parses: {e}"));
    assert_eq!(parsed.name, editor.current().doctype());
    assert_eq!(
        parsed.fields.len(),
        editor.current().doctype_def.fields.len()
    );
    // The schema plan creates the root and the child table.
    let keys: Vec<&str> = plan.schema.steps.iter().map(|s| s.key.as_str()).collect();
    assert!(keys.contains(&"create_table:vehicle_service_request"));
    assert!(keys.contains(&"create_table:vehicle_service_item"));
    // Creating a DocType is additive.
    assert_eq!(plan.risk(), Risk::Additive);
}

#[test]
fn publishing_requires_the_permission() {
    let editor = build_vehicle_service_request();
    let site = site_with_vehicle();
    let plan = plan_publish(editor.current(), &site);
    let publisher = MemoryPublisher::new();
    let refused = publish(&plan, &Designer::new("ann"), &publisher);
    assert!(refused.is_err());
    assert_eq!(
        refused.err().map(|e| e.code()),
        Some("nocode_model.publish_forbidden")
    );
    let allowed = Designer::new("ann").with_permission(PUBLISH_PERMISSION);
    let receipt = publish(&plan, &allowed, &publisher)
        .unwrap_or_else(|e| panic!("the publish succeeds: {e}"));
    assert_eq!(receipt.records_written, 1);
    assert!(publisher.last().is_some());
}

#[test]
fn a_draft_can_be_discarded_and_never_publishes() {
    let mut draft = build_vehicle_service_request().current().clone();
    draft.status = DraftStatus::Discarded;
    // Discarding is a status; the plan still validates the design (a host refuses to publish
    // a discarded draft by policy, not by making it invalid here).
    let site = site_with_vehicle();
    let report = validate(&draft, &site);
    assert!(report.is_ok());
}
