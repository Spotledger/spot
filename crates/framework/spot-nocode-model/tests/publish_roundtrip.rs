//! Publish equals what `spot-custom-bundle` captures afterwards: the records a publish plans
//! are exactly the customisation items a capture carries, and a captured bundle round-trips.

mod support;

use spot_custom_bundle::{
    CaptureOptions, FakeSite, SectionRegistry, SiteWriter, apply as apply_bundle, capture,
    plan_apply,
};
use spot_customise::RecordOp;
use spot_meta::{FieldSpec, FieldType, Property};
use spot_nocode_model::{Draft, DraftOp, plan_publish};
use support::*;

/// Builds a draft that edits the installed `vehicle` DocType: it adds a custom field and sets a
/// default on an existing field.
fn vehicle_edit_draft() -> Draft {
    let mut editor = spot_nocode_model::DraftEditor::new(Draft::from_definition(
        "draft-vehicle",
        "ann",
        vehicle_definition(),
    ));
    apply(
        &mut editor,
        DraftOp::AddField {
            child: None,
            name: "custom_notes".to_owned(),
            label: Some("Notes".to_owned()),
            ty: FieldType::Text,
            insert_after: None,
        },
    );
    apply(
        &mut editor,
        DraftOp::SetDefault {
            child: None,
            name: "plate".to_owned(),
            default: Some("UNKNOWN".to_owned()),
        },
    );
    editor.current().clone()
}

/// Writes a plan's records into a site through its `SiteWriter`.
fn write_records(site: &mut FakeSite, records: &[RecordOp]) {
    site.begin().unwrap_or_else(|e| panic!("begin: {e}"));
    for record in records {
        match record {
            RecordOp::PutField(field) => {
                let spec: FieldSpec = toml::from_str(&field.spec)
                    .unwrap_or_else(|e| panic!("the field spec parses: {e}"));
                site.upsert_custom_field(&field.doctype, spec, field.insert_after.as_deref())
                    .unwrap_or_else(|e| panic!("upsert field: {e}"));
            }
            RecordOp::PutProperty(property) => {
                let parsed = Property::parse(&property.property, &property.value)
                    .unwrap_or_else(|e| panic!("the property text parses: {e}"));
                site.set_override(&property.doctype, property.field.as_deref(), parsed)
                    .unwrap_or_else(|e| panic!("set override: {e}"));
            }
            RecordOp::PutDocType(record) => {
                // A new DocType is a definition, not a customisation; the bundle's meta
                // section does not carry it. Assert we only ever see this in the new path.
                panic!("unexpected DocTypeRecord for {}: {record:?}", record.name);
            }
            RecordOp::Remove(key) => panic!("unexpected removal: {key:?}"),
        }
    }
    site.commit().unwrap_or_else(|e| panic!("commit: {e}"));
}

/// The stable meta-section keys a plan's records produce.
fn expected_meta_keys(records: &[RecordOp]) -> Vec<String> {
    let mut keys = Vec::new();
    for record in records {
        match record {
            RecordOp::PutField(field) => keys.push(format!("{}.{}", field.doctype, field.name)),
            RecordOp::PutProperty(property) => match &property.field {
                Some(field) => keys.push(format!(
                    "{}.{}.{}",
                    property.doctype, field, property.property
                )),
                None => keys.push(format!(
                    "{}.@doctype.{}",
                    property.doctype, property.property
                )),
            },
            _ => {}
        }
    }
    keys.sort();
    keys
}

#[test]
fn publish_equals_what_a_capture_carries() {
    let draft = vehicle_edit_draft();
    let site = site_with_vehicle();
    let plan = plan_publish(&draft, &site);
    assert!(
        plan.ready,
        "blocked: {:?}",
        plan.errors().collect::<Vec<_>>()
    );
    assert!(!plan.new_doctype);

    // Write the plan's records into a site, then capture it.
    let mut published = FakeSite::new().with_definition(vehicle_definition());
    write_records(&mut published, &plan.records);

    let options = CaptureOptions::new("vehicle-edit", "1.0.0", "ann", "2026-01-01T00:00:00Z");
    let bundle = capture(&published, &options).unwrap_or_else(|e| panic!("capture: {e}"));
    let registry = SectionRegistry::new();
    let meta = bundle
        .section("meta")
        .map(|data| {
            registry
                .get("meta")
                .map(|section| section.items(&data.files))
                .unwrap_or_default()
        })
        .unwrap_or_default();
    let captured: Vec<String> = meta.keys().cloned().collect();
    assert_eq!(captured, expected_meta_keys(&plan.records));
    assert_eq!(
        captured,
        vec![
            "vehicle.custom_notes".to_owned(),
            "vehicle.plate.default".to_owned()
        ]
    );
}

#[test]
fn a_published_bundle_round_trips_through_an_empty_site() {
    let draft = vehicle_edit_draft();
    let site = site_with_vehicle();
    let plan = plan_publish(&draft, &site);
    let mut source = FakeSite::new().with_definition(vehicle_definition());
    write_records(&mut source, &plan.records);
    let options = CaptureOptions::new("vehicle-edit", "1.0.0", "ann", "2026-01-01T00:00:00Z");
    let registry = SectionRegistry::new();
    let bundle = capture(&source, &options).unwrap_or_else(|e| panic!("capture: {e}"));

    // Apply the captured bundle to a fresh site and capture again.
    let mut target = FakeSite::new().with_definition(vehicle_definition());
    let apply_plan =
        plan_apply(&bundle, &target, &registry).unwrap_or_else(|e| panic!("plan_apply: {e:?}"));
    apply_bundle(&apply_plan, &mut target, &registry).unwrap_or_else(|e| panic!("apply: {e}"));
    let again = capture(&target, &options).unwrap_or_else(|e| panic!("capture again: {e}"));
    assert_eq!(bundle.digest(), again.digest());
}

#[test]
fn a_new_doctype_plans_a_doctype_record_and_no_field_records() {
    let editor = build_vehicle_service_request();
    let site = site_with_vehicle();
    let plan = plan_publish(editor.current(), &site);
    assert!(plan.new_doctype);
    assert!(matches!(
        plan.records.first(),
        Some(RecordOp::PutDocType(_))
    ));
    assert_eq!(plan.records.len(), 1);
}
