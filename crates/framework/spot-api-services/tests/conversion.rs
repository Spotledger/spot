#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
use serde_json::json;
use spot_api_services::{ApiErrorExt, MetaResources, build_invocation};
use spot_app_api::context::Actor;
use spot_app_api::{AppError, ErrorClass, FieldPath};
use spot_core::{ManualClock, SiteId};
use spot_http::RequestId;
use spot_http::api::service::{MetaService, RequestCtx};

fn ctx() -> RequestCtx {
    RequestCtx {
        site: SiteId::new("test").unwrap(),
        credential: None,
        idempotency_key: None,
        language: Some("ur-PK".into()),
        request_id: RequestId::generate(),
        peer: None,
    }
}

#[test]
fn every_error_class_uses_its_registry_code_and_preserves_details_and_explanation() {
    for class in [
        ErrorClass::Validation,
        ErrorClass::NotFound,
        ErrorClass::UnknownDocType,
        ErrorClass::StaleVersion,
        ErrorClass::Duplicate,
        ErrorClass::Linked,
        ErrorClass::InvalidState,
        ErrorClass::PermissionDenied,
        ErrorClass::MethodNotFound,
        ErrorClass::Unavailable,
        ErrorClass::Internal,
    ] {
        let source = AppError::new(class, "document.validation_failed", "Message")
            .explained("Offline help")
            .detail("nested", json!({"values":[1,"2.30",null]}));
        let mapped = source.into_api_error();
        assert_eq!(mapped.code.as_str(), class.api_code());
        assert_eq!(mapped.message, "Message");
        assert_eq!(mapped.explanation.as_deref(), Some("Offline help"));
        assert_eq!(mapped.details["nested"], json!({"values":[1,"2.30",null]}));
        assert_eq!(mapped.retryable(), class.retryable());
    }
    let mapped = AppError::field(
        FieldPath::row("rows", 2, "text"),
        "document.required",
        "Required",
    )
    .into_api_error();
    assert_eq!(mapped.fields[0].field, "rows[2].text");
    assert_eq!(mapped.fields[0].code, "document.required");
}

#[test]
fn invocation_does_not_invent_identity_or_silently_drop_invalid_command_ids() {
    let now = time::OffsetDateTime::from_unix_timestamp(1_790_000_000).unwrap();
    let clock = ManualClock::new(now);
    let actor = Actor::System {
        component: spot_storage_contract::Name::new("tests").unwrap(),
    };
    let mut ctx = ctx();
    ctx.idempotency_key = Some("stable-command".into());
    let invocation = build_invocation(&ctx, &clock, actor.clone()).unwrap();
    assert_eq!(invocation.actor, actor);
    assert_eq!(invocation.command_id.unwrap().as_str(), "stable-command");
    assert_eq!(invocation.now, now);
    ctx.idempotency_key = Some(String::new());
    assert!(build_invocation(&ctx, &clock, actor).is_err());
}

#[tokio::test]
async fn effective_metadata_keeps_formats_and_table_multiselect_child_identity() {
    let definitions = [
        r#"name = "choice"
[[fields]]
name = "title"
type = "text"
format = "barcode"
symbology = "qr"
"#,
        r#"name = "selection"
is_child = true
[[fields]]
name = "choice"
type = "link"
options = "choice"
"#,
        r#"name = "container"
[[fields]]
name = "choices"
type = "table_multiselect"
options = "selection"
"#,
    ]
    .iter()
    .map(|text| spot_meta::Definition::parse("test.toml", text).unwrap())
    .collect();
    let effective = spot_meta::Registry::new(definitions).unwrap().effective();
    let meta = MetaResources::new(effective.clone());
    let choice = meta.doctype_meta(&ctx(), "choice").await.unwrap();
    assert_eq!(choice.version, effective.version());
    assert_eq!(choice.fields[0].format.as_deref(), Some("barcode"));
    assert_eq!(choice.fields[0].format_params["symbology"], "qr");
    let container = meta.doctype_meta(&ctx(), "container").await.unwrap();
    assert_eq!(
        container.fields[0].child_doctype.as_ref().unwrap().as_str(),
        "selection"
    );
    assert_eq!(
        container.fields[0].link_field.as_ref().unwrap().as_str(),
        "choice"
    );
}
