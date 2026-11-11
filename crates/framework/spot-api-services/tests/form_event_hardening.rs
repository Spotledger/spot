//! WP-UI-01.T3: the three hardening items of the form-event endpoint, over a DocType of their
//! own so the metadata can carry the fields each one needs: an owner column (`created_by` and
//! `owner`), a hidden field with a label (`Grand Total`), and a step that echoes the caller's own
//! text back into its error.
//!
//! * Recorded security finding: the draft's creator comes from the authenticated actor, so a client value has no
//!   effect. The step copies `created_by` into the visible `stamp` column, which is how the test
//!   sees what the engine was given.
//! * Recorded security finding: a step error never carries a client-supplied string, and a guess that happens to
//!   equal a hidden field's label is answered exactly like a guess that does not — no oracle.
//! * Recorded security finding: a detail value deeper or larger than the walk's cap is treated as sensitive and
//!   the error is redacted, never skipped.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value as Json, json};
use spot_api_services::access::{AccessControl, engine_for};
use spot_api_services::form_event::{EngineFormEvents, FormEvent, FormEventService};
use spot_app_api::controller::{StepDecl, StepName};
use spot_app_api::prelude::RecordId;
use spot_app_api::{
    AppError, AppResult, BoxFuture as AppFuture, Controller, ErrorClass, FieldPath, HookContext,
    HookPoint,
};
use spot_core::{Clock, ManualClock, SiteId};
use spot_document::engine::{Engine, MemoryPrincipals, V7Ids};
use spot_http::api::codec::parse_record_id;
use spot_http::api::registry::Code;
use spot_http::api::service::{ApiError, Credential, ErrorCode, RequestCtx};
use spot_http::request::RequestId;
use spot_meta::{Definition, EffectiveMeta, Registry};
use spot_permissions::model::{Model, ModelTexts};
use spot_permissions::{Principal, load};
use spot_seams::RegistryBuilder;
use spot_storage_fake::FakeBackend;

/// The signed-in user of these fixtures.
const USER: &str = "01970000-0000-7000-8000-00000000aa01";
/// Somebody else: the user a forged `created_by` names.
const VICTIM: &str = "01970000-0000-7000-8000-00000000aa02";

// --------------------------------------------------------------------------------------
// The metadata

/// An order with an owner column, a server-derived visible column, and one hidden field that
/// carries a label.
const ORDER: &str = r#"
name = "order"
label = "Order"
title_field = "number"

[[fields]]
name = "number"
type = "text"
required = true
max_chars = 120

[[fields]]
name = "created_by"
type = "text"
max_chars = 64

[[fields]]
name = "owner"
type = "text"
max_chars = 64

[[fields]]
name = "stamp"
type = "text"
read_only = true
max_chars = 64

[[fields]]
name = "grand_total"
type = "decimal"
scale = 2
precision = 14
read_only = true
protection_group = "internal"
label = "Grand Total"

[[protection_groups]]
name = "internal"
label = "Internal notes"
"#;

fn definitions() -> Vec<Definition> {
    [("order.toml", ORDER)]
        .into_iter()
        .map(|(source, text)| Definition::parse(source, text).unwrap())
        .collect()
}

fn meta() -> Arc<EffectiveMeta> {
    Registry::new(definitions()).unwrap().effective()
}

/// `OrderUser` reads and writes orders. No role holds a grant on the `internal` group, so
/// `grand_total` is hidden — the closed-by-default rule.
fn model() -> Model {
    load(&ModelTexts {
        grants: r#"
[[entries]]
role = "OrderUser"
doctype = "order"
rights = ["read", "write", "create"]
"#
        .to_owned(),
        field_grants: String::new(),
        groups: r#"
[[entries]]
name = "internal"
"#
        .to_owned(),
        shares: String::new(),
    })
    .unwrap_or_else(|errors| panic!("fixture model: {:?}", errors.as_slice()))
}

fn rid(text: &str) -> RecordId {
    parse_record_id(text).unwrap()
}

fn now() -> time::OffsetDateTime {
    time::OffsetDateTime::from_unix_timestamp(1_790_000_000).unwrap()
}

/// The request context every call carries.
fn ctx() -> RequestCtx {
    RequestCtx {
        site: SiteId::new("test").unwrap(),
        credential: Some(Credential::new("order-user")),
        idempotency_key: None,
        language: None,
        request_id: RequestId::new("form-event-hardening").unwrap(),
        peer: None,
    }
}

/// The service over the fixture metadata, and a run of one form event.
struct Orders {
    service: EngineFormEvents<Engine<FakeBackend>>,
}

impl Orders {
    async fn new() -> Self {
        let meta = meta();
        let model = model();
        let controller = Arc::new(OrderForms) as Arc<dyn Controller>;
        let mut registry = RegistryBuilder::new();
        registry.add_controller("probe", controller);
        let clock: Arc<dyn Clock> = Arc::new(ManualClock::new(now()));
        let engine = Arc::new(
            Engine::new(
                FakeBackend::new(),
                meta.clone(),
                Arc::new(registry.build().unwrap()),
                clock.clone(),
                Arc::new(V7Ids),
            )
            .unwrap(),
        );
        engine.prepare().await.unwrap();
        let actor: Arc<spot_api_services::engine_resources::ActorResolver> =
            Arc::new(|ctx: &RequestCtx| {
                if ctx.site.as_str() != "test"
                    || ctx.credential.as_ref().map(|c| c.token.as_str()) != Some("order-user")
                {
                    return Err(ApiError::from_code(Code::AuthRequired));
                }
                Ok(spot_app_api::Actor::User {
                    id: rid(USER),
                    via: None,
                })
            });
        let mut principals = MemoryPrincipals::new();
        principals = principals.with(&rid(USER), Principal::with_roles(USER, &["OrderUser"]));
        principals = principals.with(&rid(VICTIM), Principal::with_roles(VICTIM, &["OrderUser"]));
        let access = Arc::new(AccessControl::new(
            meta.clone(),
            Arc::new(engine_for(&meta, &model).unwrap()),
            Arc::new(principals),
            7,
            clock,
            actor,
        ));
        let service = EngineFormEvents::new(engine, access);
        Self { service }
    }

    /// One form event over the fixture DocType.
    async fn call(&self, body: &Json) -> Result<FormEvent, ApiError> {
        let bytes = body.to_string();
        self.service
            .form_event(&ctx(), "order", bytes.as_bytes(), Duration::from_secs(5))
            .await
    }
}

/// The step that answers a `number` change: it stamps `created_by` into `stamp`, and raises the
/// probe's error when the number asks for one.
struct OrderForms;

impl Controller for OrderForms {
    fn doctype(&self) -> &str {
        "order"
    }

    fn steps(&self) -> Vec<StepDecl> {
        vec![StepDecl {
            name: StepName::new("probe:number").unwrap(),
            point: HookPoint::FieldChanged("number".to_owned()),
            priority: 0,
            budget: Duration::from_secs(5),
        }]
    }

    fn run_step<'a>(
        &'a self,
        step: &'a StepName,
        ctx: &'a mut dyn HookContext,
    ) -> AppFuture<'a, AppResult<()>> {
        Box::pin(async move {
            if !matches!(ctx.point(), HookPoint::FieldChanged(field) if field == "number") {
                return Err(AppError::new(
                    ErrorClass::Internal,
                    "platform.phase_violation",
                    format!("{} is not a form step.", step.as_str()),
                ));
            }
            let number = ctx
                .doc()
                .text("number")
                .ok()
                .flatten()
                .unwrap_or("")
                .to_owned();
            let creator = ctx
                .doc()
                .text("created_by")
                .ok()
                .flatten()
                .unwrap_or("<none>")
                .to_owned();
            ctx.doc_mut()?.set(name("stamp"), creator);
            if number.starts_with("ECHO:") {
                return Err(echo_error(&number));
            }
            if number.starts_with("DEEP:") {
                return Err(deep_error());
            }
            if number.starts_with("WIDE:") {
                return Err(wide_error());
            }
            Ok(())
        })
    }
}

fn name(field: &str) -> spot_app_api::prelude::Name {
    spot_app_api::prelude::Name::new(field).unwrap()
}

/// The error a step raises when it echoes the caller's own text back: the rejected value *is* a
/// client-supplied string, and the message names it.
fn echo_error(number: &str) -> AppError {
    AppError::field(
        FieldPath::field("number"),
        "probe.echo",
        format!("The value `{number}` was refused."),
    )
}

/// A detail nested deeper than the walk's cap, with nothing sensitive in it.
fn deep_error() -> AppError {
    let mut value = json!("plain");
    for _ in 0..40 {
        value = Json::Array(vec![value]);
    }
    AppError::field(
        FieldPath::field("number"),
        "probe.deep",
        "The value is invalid.",
    )
    .detail("context", value)
}

/// A detail with more nodes than the walk's cap, with nothing sensitive in it.
fn wide_error() -> AppError {
    let items: Vec<Json> = (0..20_000).map(Json::from).collect();
    AppError::field(
        FieldPath::field("number"),
        "probe.wide",
        "The value is invalid.",
    )
    .detail("context", Json::Array(items))
}

fn request(number: &str) -> Json {
    json!({"doc": {"number": number}, "changed": "number"})
}

/// Every text an error can carry, so a "no client string" assertion covers the whole error.
fn texts_of(error: &ApiError) -> String {
    let mut text = format!(
        "{:?} {} {:?} {:?} {:?}",
        error.code, error.message, error.explanation, error.fields, error.details
    );
    // The details' JSON form, so a string nested in an object or an array is covered too.
    text.push_str(&serde_json::to_string(&error.details).unwrap_or_default());
    text
}

// --------------------------------------------------------------------------------------
// Recorded security finding: the creator comes from the actor

#[tokio::test]
async fn a_forged_creator_in_the_draft_has_no_effect() {
    let orders = Orders::new().await;
    // The draft claims the other user created it, in both owner columns the metadata has.
    let body = json!({
        "doc": {"number": "ORD-1", "created_by": VICTIM, "owner": VICTIM},
        "changed": "number"
    });
    let event = orders.call(&body).await.unwrap();
    // The step copied the draft's `created_by` into the visible `stamp` column: it is the
    // authenticated actor, not the value the client sent.
    assert_eq!(
        event.patch["stamp"], USER,
        "the draft's creator is the actor: {:?}",
        event.patch
    );
    let text = event.to_json().to_string();
    assert!(
        !text.contains(VICTIM),
        "the forged value reaches the client: {text}"
    );
}

#[tokio::test]
async fn a_draft_without_a_creator_is_stamped_with_the_actor() {
    let orders = Orders::new().await;
    let event = orders.call(&request("ORD-2")).await.unwrap();
    assert_eq!(event.patch["stamp"], USER, "{:?}", event.patch);
}

// --------------------------------------------------------------------------------------
// Recorded security finding: no client string in a step error

#[tokio::test]
async fn a_step_error_carries_no_client_supplied_string() {
    let orders = Orders::new().await;
    let marker = "MARKER-7f3a2c";
    let error = orders
        .call(&request(&format!("ECHO:{marker}")))
        .await
        .unwrap_err();
    let text = texts_of(&error);
    assert!(
        !text.contains(marker),
        "the error echoes the client: {text}"
    );
    // The refused value is the whole message, so the error carries the field path and a stable
    // registry code instead of the step's own text.
    assert_eq!(
        error.code,
        ErrorCode::Registry(Code::DocumentValidationFailed)
    );
    for issue in &error.fields {
        assert_eq!(issue.field, "number", "{text}");
        assert_eq!(issue.code, "document.validation_failed", "{text}");
    }
    assert!(error.details.is_empty(), "{text}");
}

#[tokio::test]
async fn guessing_a_hidden_label_is_answered_like_any_other_guess() {
    let orders = Orders::new().await;
    // A guess that happens to be the hidden field's label, and one that is not.
    let hidden = orders.call(&request("ECHO:Grand Total")).await.unwrap_err();
    let other = orders.call(&request("ECHO:Not A Label")).await.unwrap_err();
    // The two answers must be indistinguishable, or the reply says whether the guess matched.
    assert_eq!(hidden, other, "a guess at a hidden name is an oracle");
    let text = texts_of(&hidden);
    assert!(!text.contains("Grand Total"), "{text}");
}

// --------------------------------------------------------------------------------------
// Recorded security finding: the walk's caps

#[tokio::test]
async fn a_value_deeper_than_the_walk_cap_is_redacted() {
    let orders = Orders::new().await;
    let error = orders.call(&request("DEEP:1")).await.unwrap_err();
    assert_eq!(
        error,
        ApiError::from_code(Code::DocumentValidationFailed),
        "a value past the depth cap is sensitive: {error:?}"
    );
}

#[tokio::test]
async fn a_value_with_more_nodes_than_the_walk_cap_is_redacted() {
    let orders = Orders::new().await;
    let error = orders.call(&request("WIDE:1")).await.unwrap_err();
    // The detail is 20_000 numbers; the assertion names only the code and the shape.
    assert_eq!(
        (error.code.clone(), error.fields.len(), error.details.len()),
        (ErrorCode::Registry(Code::DocumentValidationFailed), 0, 0),
        "a value past the node cap is sensitive"
    );
    let expected = ApiError::from_code(Code::DocumentValidationFailed);
    assert_eq!(error.message, expected.message);
    assert!(error.explanation.is_none());
}

#[tokio::test]
async fn a_plain_change_answers_no_error() {
    let orders = Orders::new().await;
    let event = orders.call(&request("ORD-3")).await.unwrap();
    assert_eq!(event.patch["stamp"], USER, "{:?}", event.patch);
}
