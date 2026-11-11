//! Shared helpers for the integration tests.

#![allow(dead_code)]

use spot_core::SiteId;
use spot_meta::{Definition, FieldSpec, FieldType};
use spot_nocode_model::{
    ChildDraft, Condition, Draft, DraftEditor, DraftOp, Join, Operator, Row, SiteContext, Value,
    apply_op,
};

/// A site id for the tests.
pub fn site_id() -> SiteId {
    SiteId::new("acme.example").unwrap_or_else(|e| panic!("`acme.example` is a site id: {e}"))
}

/// An installed `vehicle` DocType, the link target of the golden design.
pub fn vehicle_definition() -> Definition {
    let mut definition = Definition::new("vehicle");
    definition.label = Some("Vehicle".to_owned());
    definition.fields = vec![
        FieldSpec::new("plate", FieldType::Text),
        FieldSpec::new("status", FieldType::Text),
    ];
    definition
}

/// A site with the `vehicle` DocType installed.
pub fn site_with_vehicle() -> SiteContext {
    SiteContext::new(site_id()).with_definition(vehicle_definition())
}

/// Applies an op, panicking with the error's display on failure.
pub fn apply(editor: &mut DraftEditor, op: DraftOp) {
    editor
        .apply(op)
        .unwrap_or_else(|e| panic!("the golden op failed: {e}"));
}

/// Applies an op directly to a draft, panicking on failure.
pub fn apply_to(draft: &Draft, op: &DraftOp) -> Draft {
    apply_op(draft, op).unwrap_or_else(|e| panic!("the op failed: {e}"))
}

/// Builds the 'Vehicle Service Request' DocType with its child table and conditions, through
/// ops only.
pub fn build_vehicle_service_request() -> DraftEditor {
    let mut editor = DraftEditor::new(Draft::new(
        "draft-vsr",
        "ann",
        "vehicle_service_request",
        "Vehicle Service Request",
    ));
    let field = |name: &str, label: &str, ty: FieldType| DraftOp::AddField {
        child: None,
        name: name.to_owned(),
        label: Some(label.to_owned()),
        ty,
        insert_after: None,
    };
    apply(
        &mut editor,
        field("request_date", "Request Date", FieldType::Date),
    );
    apply(
        &mut editor,
        field("customer_name", "Customer Name", FieldType::Text),
    );
    apply(
        &mut editor,
        field("priority", "Priority", FieldType::Select),
    );
    apply(
        &mut editor,
        DraftOp::SetOptions {
            child: None,
            name: "priority".to_owned(),
            options: vec!["Low".to_owned(), "Medium".to_owned(), "High".to_owned()],
        },
    );
    apply(&mut editor, field("status", "Status", FieldType::Select));
    apply(
        &mut editor,
        DraftOp::SetOptions {
            child: None,
            name: "status".to_owned(),
            options: vec![
                "Open".to_owned(),
                "In Progress".to_owned(),
                "Closed".to_owned(),
            ],
        },
    );
    apply(
        &mut editor,
        DraftOp::AddLink {
            name: "vehicle".to_owned(),
            label: Some("Vehicle".to_owned()),
            target: "vehicle".to_owned(),
            insert_after: None,
        },
    );
    apply(
        &mut editor,
        field("description", "Description", FieldType::LongText),
    );
    apply(
        &mut editor,
        field("customer_phone", "Customer Phone", FieldType::Text),
    );
    apply(
        &mut editor,
        DraftOp::AddChildTable {
            name: "service_items".to_owned(),
            label: Some("Service Items".to_owned()),
            child: Box::new(ChildDraft::new(
                "vehicle_service_item",
                "Vehicle Service Item",
            )),
            insert_after: None,
        },
    );
    let child_field = |name: &str, label: &str, ty: FieldType| DraftOp::AddField {
        child: Some("vehicle_service_item".to_owned()),
        name: name.to_owned(),
        label: Some(label.to_owned()),
        ty,
        insert_after: None,
    };
    apply(
        &mut editor,
        child_field("item_name", "Item", FieldType::Text),
    );
    apply(
        &mut editor,
        DraftOp::SetRequired {
            child: Some("vehicle_service_item".to_owned()),
            name: "item_name".to_owned(),
            required: true,
        },
    );
    apply(&mut editor, child_field("hours", "Hours", FieldType::Int));
    apply(&mut editor, child_field("notes", "Notes", FieldType::Text));
    // `customer_phone` is shown only while the request is open.
    apply(
        &mut editor,
        DraftOp::SetDependsOn {
            child: None,
            name: "customer_phone".to_owned(),
            condition: Some(Condition {
                join: Join::Or,
                rows: vec![
                    Row {
                        field: "status".to_owned(),
                        operator: Operator::Equals,
                        values: vec![Value::Text("Open".to_owned())],
                    },
                    Row {
                        field: "status".to_owned(),
                        operator: Operator::Equals,
                        values: vec![Value::Text("In Progress".to_owned())],
                    },
                ],
            }),
        },
    );
    // The layout: a Request section and a Service Items section on the form.
    apply(
        &mut editor,
        DraftOp::AddFormSection {
            child: None,
            id: "request".to_owned(),
            title: Some("Request".to_owned()),
            columns: Some(2),
            tab: None,
        },
    );
    for name in [
        "request_date",
        "customer_name",
        "priority",
        "status",
        "vehicle",
        "description",
        "customer_phone",
    ] {
        apply(
            &mut editor,
            DraftOp::MoveFieldInLayout {
                child: None,
                field: name.to_owned(),
                section: "request".to_owned(),
                after: None,
            },
        );
    }
    apply(
        &mut editor,
        DraftOp::AddFormSection {
            child: None,
            id: "items".to_owned(),
            title: Some("Service Items".to_owned()),
            columns: None,
            tab: None,
        },
    );
    apply(
        &mut editor,
        DraftOp::MoveFieldInLayout {
            child: None,
            field: "service_items".to_owned(),
            section: "items".to_owned(),
            after: None,
        },
    );
    // The child's own grid layout.
    apply(
        &mut editor,
        DraftOp::AddFormSection {
            child: Some("vehicle_service_item".to_owned()),
            id: "items_grid".to_owned(),
            title: Some("Items".to_owned()),
            columns: None,
            tab: None,
        },
    );
    for name in ["item_name", "hours", "notes"] {
        apply(
            &mut editor,
            DraftOp::MoveFieldInLayout {
                child: Some("vehicle_service_item".to_owned()),
                field: name.to_owned(),
                section: "items_grid".to_owned(),
                after: None,
            },
        );
    }
    editor
}
