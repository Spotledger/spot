use super::*;
use spot_app_api::{AppError, FieldDef, FieldKind, Tag, TextFormat};

const ID: &str = "0190a7c2-5b1e-7c3a-9d4e-1f2a3b4c5d6e";

fn def(tag: Option<&str>) -> DocTypeDef {
    let mut field = FieldDef::new(
        Name::new("total").unwrap(),
        FieldKind::Text {
            max_chars: None,
            format: TextFormat::Plain,
        },
        false,
        false,
    );
    if let Some(tag) = tag {
        field.security.classification = vec![Tag::new(tag).unwrap()];
    }
    DocTypeDef::new(
        Name::new("invoice").unwrap(),
        "Invoice".to_owned(),
        true,
        false,
        None,
        vec![field],
    )
}

#[test]
fn a_reference_is_a_doctype_and_a_record_id() {
    let (doctype, id) = parse_ref(&format!("sales_invoice/{ID}")).unwrap();
    assert_eq!(doctype, "sales_invoice");
    assert_eq!(Some(id), spot_bpm_documents::ids::parse_record_id(ID));
    assert!(parse_ref(ID).is_none());
    assert!(parse_ref("Bad Type/x").is_none());
    assert!(parse_ref(&format!("Sales/{ID}")).is_none());
    assert!(parse_ref("invoice/not-an-id").is_none());
    assert!(parse_ref("").is_none());
}

#[test]
fn an_actor_is_a_user_record_with_or_without_a_prefix() {
    let bare = actor_user(ID).unwrap();
    assert_eq!(actor_user(&format!("agent:{ID}")), Some(bare));
    assert_eq!(actor_user(&format!("user:{ID}")), Some(bare));
    assert!(actor_user("agent:agent-t2").is_none());
    assert!(actor_user("").is_none());
}

#[test]
fn a_doctype_is_financial_when_a_field_is_classified_financial() {
    assert!(is_financial(&def(Some("financial"))));
    assert!(!is_financial(&def(Some("pii"))));
    assert!(!is_financial(&def(None)));
}

#[test]
fn the_floor_refuses_every_edit_of_a_submitted_financial_record_but_cancel_and_amend() {
    let view = DocumentView {
        state: RemedyState::Submitted,
        financial: true,
    };
    let recompute = DocOp::Recompute {
        field: "total".to_owned(),
    };
    let cancel = DocOp::Cancel {
        reason: "r".to_owned(),
    };
    assert!(floor_refused(&view, &recompute));
    assert!(floor_refused(&view, &DocOp::Repost));
    assert!(!floor_refused(&view, &cancel));
    assert!(!floor_refused(&view, &DocOp::Amend));
    let draft = DocumentView {
        state: RemedyState::Draft,
        financial: true,
    };
    assert!(!floor_refused(&draft, &DocOp::Repost));
    let plain = DocumentView {
        state: RemedyState::Submitted,
        financial: false,
    };
    assert!(!floor_refused(&plain, &DocOp::Repost));
}

#[test]
fn the_state_of_a_document_follows_its_lifecycle() {
    let make = |state| {
        Document::loaded(
            Name::new("invoice").unwrap(),
            spot_bpm_documents::ids::parse_record_id(ID).unwrap(),
            Version::FIRST,
            state,
            std::collections::BTreeMap::new(),
            std::collections::BTreeMap::new(),
        )
    };
    assert_eq!(state_of(&make(Some(DocState::Draft))), RemedyState::Draft);
    assert_eq!(state_of(&make(None)), RemedyState::Draft);
    assert_eq!(
        state_of(&make(Some(DocState::Posted))),
        RemedyState::Submitted
    );
    assert_eq!(
        state_of(&make(Some(DocState::Cancelled))),
        RemedyState::Cancelled
    );
}

#[test]
fn an_engine_error_becomes_a_fixed_refusal_that_names_no_data() {
    let secret = "the value 12345.67 of the field total";
    let of = |class| refusal_of(&AppError::new(class, "document.validation_failed", secret));
    assert_eq!(of(ErrorClass::PermissionDenied), DENIED);
    assert_eq!(of(ErrorClass::NotFound), NO_DOCUMENT);
    assert_eq!(of(ErrorClass::StaleVersion), STALE);
    assert_eq!(of(ErrorClass::InvalidState), BAD_STATE);
    assert_eq!(of(ErrorClass::Validation), INVALID);
    assert_eq!(of(ErrorClass::Linked), LINKED);
    assert_eq!(of(ErrorClass::Internal), FAILED);
    for class in [
        ErrorClass::PermissionDenied,
        ErrorClass::NotFound,
        ErrorClass::Validation,
        ErrorClass::Internal,
    ] {
        assert!(!of(class).contains("12345"));
    }
}

#[test]
fn the_settings_default_to_on_and_refuse_unknown_keys() {
    assert!(Settings::default().enabled);
    assert!(toml::from_str::<Settings>("").unwrap().enabled);
    assert!(
        !toml::from_str::<Settings>("enabled = false")
            .unwrap()
            .enabled
    );
    assert!(toml::from_str::<Settings>("enabeld = true").is_err());
}
