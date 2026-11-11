//! Every `OpError` path is exercised, so the guard rails and preconditions are all pinned.

mod support;

use spot_meta::{FieldSpec, FieldType};
use spot_nocode_model::{
    ChildDraft, Condition, Draft, DraftOp, MAX_FIELDS, OpError, apply_op, data_loss_token,
};
use support::*;

/// A draft for a new `vehicle` DocType.
fn draft() -> Draft {
    Draft::new("d", "ann", "vehicle", "Vehicle")
}

/// The code of the error an op raises, or a panic when it unexpectedly succeeds.
fn code(draft: &Draft, op: &DraftOp) -> &'static str {
    match apply_op(draft, op) {
        Ok(_) => panic!("the op unexpectedly succeeded: {op:?}"),
        Err(error) => error.code(),
    }
}

/// Adds a field to a draft in place, panicking on failure.
fn with_field(mut draft: Draft, name: &str, ty: FieldType) -> Draft {
    draft = apply_to(
        &draft,
        &DraftOp::AddField {
            child: None,
            name: name.to_owned(),
            label: None,
            ty,
            insert_after: None,
        },
    );
    draft
}

#[test]
fn unknown_field() {
    let op = DraftOp::SetRequired {
        child: None,
        name: "nope".to_owned(),
        required: true,
    };
    assert_eq!(code(&draft(), &op), "nocode_model.unknown_field");
}

#[test]
fn unknown_child() {
    let op = DraftOp::AddField {
        child: Some("nope".to_owned()),
        name: "x".to_owned(),
        label: None,
        ty: FieldType::Text,
        insert_after: None,
    };
    assert_eq!(code(&draft(), &op), "nocode_model.unknown_child");
}

#[test]
fn duplicate_field() {
    let draft = with_field(draft(), "a", FieldType::Text);
    let op = DraftOp::AddField {
        child: None,
        name: "a".to_owned(),
        label: None,
        ty: FieldType::Text,
        insert_after: None,
    };
    assert_eq!(code(&draft, &op), "nocode_model.duplicate_field");
}

#[test]
fn reserved_name() {
    let op = DraftOp::AddField {
        child: None,
        name: "id".to_owned(),
        label: None,
        ty: FieldType::Int,
        insert_after: None,
    };
    assert_eq!(code(&draft(), &op), "nocode_model.reserved_name");
}

#[test]
fn invalid_name() {
    let op = DraftOp::AddField {
        child: None,
        name: "Bad Name".to_owned(),
        label: None,
        ty: FieldType::Text,
        insert_after: None,
    };
    assert_eq!(code(&draft(), &op), "nocode_model.bad_name");
}

#[test]
fn too_many_fields() {
    let mut draft = draft();
    for index in 0..MAX_FIELDS {
        draft
            .doctype_def
            .fields
            .push(FieldSpec::new(&format!("f{index}"), FieldType::Text));
    }
    let op = DraftOp::AddField {
        child: None,
        name: "extra".to_owned(),
        label: None,
        ty: FieldType::Text,
        insert_after: None,
    };
    assert_eq!(code(&draft, &op), "nocode_model.max_fields");
}

#[test]
fn too_deep() {
    let mut draft = draft();
    draft.doctype_def.is_child = true;
    let op = DraftOp::AddChildTable {
        name: "rows".to_owned(),
        label: None,
        child: Box::new(ChildDraft::new("row", "Row")),
        insert_after: None,
    };
    assert_eq!(code(&draft, &op), "nocode_model.max_depth");
}

#[test]
fn protected_doctype() {
    let protected = Draft::new("d", "ann", "custom_doctype", "Custom DocType");
    let op = DraftOp::AddField {
        child: None,
        name: "x".to_owned(),
        label: None,
        ty: FieldType::Text,
        insert_after: None,
    };
    assert_eq!(code(&protected, &op), "nocode_model.protected_doctype");
}

#[test]
fn needs_data_loss_confirmation() {
    let draft = with_field(draft(), "a", FieldType::Text);
    let op = DraftOp::RemoveField {
        child: None,
        name: "a".to_owned(),
        confirm: None,
    };
    assert_eq!(
        code(&draft, &op),
        "nocode_model.needs_data_loss_confirmation"
    );
    // With the token it succeeds and removes the field.
    let ok = apply_to(
        &draft,
        &DraftOp::RemoveField {
            child: None,
            name: "a".to_owned(),
            confirm: Some(data_loss_token("vehicle", "a")),
        },
    );
    assert!(ok.doctype_def.fields.is_empty());
}

#[test]
fn script_not_allowed() {
    let draft = with_field(draft(), "a", FieldType::Text);
    let op = DraftOp::SetDefault {
        child: None,
        name: "a".to_owned(),
        default: Some("<script>alert(1)</script>".to_owned()),
    };
    assert_eq!(code(&draft, &op), "nocode_model.no_script");
}

#[test]
fn raw_sql_not_allowed() {
    let draft = with_field(draft(), "a", FieldType::Text);
    let op = DraftOp::SetDefault {
        child: None,
        name: "a".to_owned(),
        default: Some("select secret_column".to_owned()),
    };
    assert_eq!(code(&draft, &op), "nocode_model.no_sql");
}

#[test]
fn type_change_refused() {
    let draft = with_field(draft(), "a", FieldType::Text);
    let op = DraftOp::ChangeType {
        child: None,
        name: "a".to_owned(),
        ty: FieldType::Int,
        stats: spot_nocode_model::SampleStats {
            rows: 10,
            non_null: 10,
            unparsable: 3,
            ..spot_nocode_model::SampleStats::default()
        },
    };
    assert_eq!(code(&draft, &op), "nocode_model.type_change_refused");
}

#[test]
fn bad_condition() {
    let draft = with_field(draft(), "a", FieldType::Text);
    let op = DraftOp::SetDependsOn {
        child: None,
        name: "a".to_owned(),
        condition: Some(Condition {
            join: spot_nocode_model::Join::And,
            rows: Vec::new(),
        }),
    };
    assert_eq!(code(&draft, &op), "nocode_model.bad_condition");
}

#[test]
fn bad_options() {
    let draft = with_field(draft(), "a", FieldType::Select);
    let op = DraftOp::SetOptions {
        child: None,
        name: "a".to_owned(),
        options: vec!["X".to_owned(), "X".to_owned()],
    };
    assert_eq!(code(&draft, &op), "nocode_model.bad_options");
}

#[test]
fn unknown_section() {
    let op = DraftOp::SetSectionColumns {
        child: None,
        section: "nope".to_owned(),
        columns: 2,
    };
    assert_eq!(code(&draft(), &op), "nocode_model.unknown_section");
}

#[test]
fn unknown_tab() {
    let op = DraftOp::AddFormSection {
        child: None,
        id: "s".to_owned(),
        title: None,
        columns: None,
        tab: Some("nope".to_owned()),
    };
    assert_eq!(code(&draft(), &op), "nocode_model.unknown_tab");
}

#[test]
fn duplicate_section() {
    let draft = apply_to(
        &draft(),
        &DraftOp::AddFormSection {
            child: None,
            id: "s".to_owned(),
            title: None,
            columns: None,
            tab: None,
        },
    );
    let op = DraftOp::AddFormSection {
        child: None,
        id: "s".to_owned(),
        title: None,
        columns: None,
        tab: None,
    };
    assert_eq!(code(&draft, &op), "nocode_model.duplicate_section");
}

#[test]
fn duplicate_tab() {
    let draft = apply_to(
        &draft(),
        &DraftOp::AddFormTab {
            child: None,
            id: "t".to_owned(),
            title: None,
        },
    );
    let op = DraftOp::AddFormTab {
        child: None,
        id: "t".to_owned(),
        title: None,
    };
    assert_eq!(code(&draft, &op), "nocode_model.duplicate_tab");
}

#[test]
fn bad_layout() {
    // A form with a top-level section cannot also take a tab.
    let draft = apply_to(
        &draft(),
        &DraftOp::AddFormSection {
            child: None,
            id: "s".to_owned(),
            title: None,
            columns: None,
            tab: None,
        },
    );
    let op = DraftOp::AddFormTab {
        child: None,
        id: "t".to_owned(),
        title: None,
    };
    assert_eq!(code(&draft, &op), "nocode_model.bad_layout");
}

#[test]
fn bad_target() {
    let op = DraftOp::AddLink {
        name: "a".to_owned(),
        label: None,
        target: "spot_something".to_owned(),
        insert_after: None,
    };
    assert_eq!(code(&draft(), &op), "nocode_model.bad_target");
}

#[test]
fn bad_naming() {
    let op = DraftOp::SetNamingSeries {
        naming: Some("bad pattern".to_owned()),
    };
    assert_eq!(code(&draft(), &op), "nocode_model.bad_naming");
}

#[test]
fn unsupported_op() {
    let op = DraftOp::AddField {
        child: None,
        name: "rows".to_owned(),
        label: None,
        ty: FieldType::Table,
        insert_after: None,
    };
    assert_eq!(code(&draft(), &op), "nocode_model.unsupported_op");
}

#[test]
fn referenced() {
    let draft = with_field(
        with_field(draft(), "a", FieldType::Link),
        "b",
        FieldType::Text,
    );
    let mut draft = draft;
    // `b` fetches from `a`, so `a` cannot be removed.
    if let Some(field) = draft.doctype_def.fields.iter_mut().find(|f| f.name == "b") {
        field.fetch_from = Some("a.plate".to_owned());
    }
    let op = DraftOp::RemoveField {
        child: None,
        name: "a".to_owned(),
        confirm: Some(data_loss_token("vehicle", "a")),
    };
    assert_eq!(code(&draft, &op), "nocode_model.referenced");
}

#[test]
fn every_op_error_variant_has_a_code() {
    // A compile-time-ish guard: the codes are distinct and all under `nocode_model.`.
    let errors = [
        OpError::UnknownField {
            path: "x".to_owned(),
        },
        OpError::UnknownChild {
            name: "x".to_owned(),
        },
        OpError::DuplicateField {
            path: "x".to_owned(),
        },
        OpError::ReservedName {
            name: "x".to_owned(),
        },
        OpError::InvalidName {
            name: "x".to_owned(),
        },
        OpError::TooManyFields { limit: 1 },
        OpError::TooDeep,
        OpError::ProtectedDoctype {
            name: "x".to_owned(),
        },
        OpError::NeedsDataLossConfirmation {
            path: "x".to_owned(),
        },
        OpError::ScriptNotAllowed {
            path: "x".to_owned(),
        },
        OpError::RawSqlNotAllowed {
            path: "x".to_owned(),
        },
        OpError::TypeChangeRefused {
            path: "x".to_owned(),
            reason: "r".to_owned(),
        },
        OpError::BadCondition {
            path: "x".to_owned(),
            message: "m".to_owned(),
        },
        OpError::BadOptions {
            path: "x".to_owned(),
            why: "w".to_owned(),
        },
        OpError::UnknownSection { id: "x".to_owned() },
        OpError::UnknownTab { id: "x".to_owned() },
        OpError::DuplicateSection { id: "x".to_owned() },
        OpError::DuplicateTab { id: "x".to_owned() },
        OpError::BadLayout {
            why: "w".to_owned(),
        },
        OpError::BadTarget {
            name: "x".to_owned(),
        },
        OpError::BadNaming {
            naming: "x".to_owned(),
        },
        OpError::Unsupported {
            what: "w".to_owned(),
        },
        OpError::Referenced {
            name: "x".to_owned(),
        },
    ];
    let mut seen: Vec<&str> = Vec::new();
    for error in &errors {
        let code = error.code();
        assert!(code.starts_with("nocode_model."), "{code}");
        assert!(!seen.contains(&code), "duplicate code {code}");
        seen.push(code);
    }
}
