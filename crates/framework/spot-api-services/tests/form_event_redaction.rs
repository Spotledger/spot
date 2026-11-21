//! `filter_error` (WP-UI-01.T2): the promise that a step's error cannot say a hidden field
//! exists, and the ways an error could try to say it.
//!
//! The fixtures here are the form-event suite's shape over a DocType of their own, because this
//! file has to vary what the metadata carries: field **labels** (`Grand Total`, `Rate Per Unit`,
//! `Secret Code (internal)`), a hidden column of a table the draft leaves empty, and a one-letter
//! table. The suite in `form_event.rs` proves the adapter; this one proves the redaction rule.
//!
//! Every redacted case asserts the whole output contract at once: the error equals
//! `ApiError::from_code` of its registry code — the contract's default message, no fields, no
//! explanation, no details.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value as Json, json};
use spot_api_services::access::{AccessControl, engine_for};
use spot_api_services::form_event::{EngineFormEvents, FieldChangedPlan, FormEventService};
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
use spot_http::api::service::{ApiError, Credential, RequestCtx};
use spot_http::request::RequestId;
use spot_meta::{Definition, EffectiveMeta, Registry};
use spot_permissions::model::{Model, ModelTexts};
use spot_permissions::{Principal, load};
use spot_seams::RegistryBuilder;
use spot_storage_fake::FakeBackend;

/// The signed-in user of these fixtures.
const USER: &str = "01970000-0000-7000-8000-0000000001a1";

// --------------------------------------------------------------------------------------
// The metadata

/// A parent DocType with hidden fields that carry labels, and two child tables: `items` (whose
/// hidden column is `rate`) and `notes` (which the drafts never give a row).
const ORDER: &str = r#"
name = "order"
label = "Order"
title_field = "number"

[[fields]]
name = "number"
type = "text"
required = true
label = "Number"

[[fields]]
name = "grand_total"
type = "decimal"
scale = 2
precision = 14
read_only = true
protection_group = "internal"
label = "Grand Total"

[[fields]]
name = "tax"
type = "decimal"
scale = 2
precision = 14
protection_group = "internal"
label = "Tax"

[[fields]]
name = "taxonomy"
type = "text"
label = "Taxonomy"

[[fields]]
name = "secret_code"
type = "text"
protection_group = "internal"
label = "Secret Code (internal)"

[[fields]]
name = "items"
type = "table"
options = "order_item"
label = "Items"

[[fields]]
name = "notes"
type = "table"
options = "order_note"
label = "Notes"

[[protection_groups]]
name = "internal"
label = "Internal notes"
"#;

/// The `items` child: `rate` is hidden and labelled, `qty` is visible.
const ORDER_ITEM: &str = r#"
name = "order_item"
is_child = true

[[fields]]
name = "item"
type = "text"

[[fields]]
name = "qty"
type = "decimal"
scale = 3
precision = 14

[[fields]]
name = "rate"
type = "decimal"
scale = 2
precision = 14
protection_group = "internal"
label = "Rate Per Unit"

[[protection_groups]]
name = "internal"
label = "Internal notes"
"#;

/// The `notes` child: `private_note` is hidden and labelled, `text` is visible.
const ORDER_NOTE: &str = r#"
name = "order_note"
is_child = true

[[fields]]
name = "text"
type = "text"

[[fields]]
name = "private_note"
type = "text"
protection_group = "internal"
label = "Private Note"

[[protection_groups]]
name = "internal"
label = "Internal notes"
"#;

fn definitions() -> Vec<Definition> {
    [
        ("order.toml", ORDER),
        ("order_item.toml", ORDER_ITEM),
        ("order_note.toml", ORDER_NOTE),
    ]
    .into_iter()
    .map(|(source, text)| Definition::parse(source, text).unwrap())
    .collect()
}

fn meta() -> Arc<EffectiveMeta> {
    Registry::new(definitions()).unwrap().effective()
}

/// `OrderUser` reads and writes the three DocTypes. No role holds a grant on the `internal`
/// group, so every field in it is hidden — that is the closed-by-default rule.
fn model() -> Model {
    load(&ModelTexts {
        grants: r#"
[[entries]]
role = "OrderUser"
doctype = "order"
rights = ["read", "write", "create"]
[[entries]]
role = "OrderUser"
doctype = "order_item"
rights = ["read", "write"]
[[entries]]
role = "OrderUser"
doctype = "order_note"
rights = ["read", "write"]
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
        request_id: RequestId::new("form-event-redaction").unwrap(),
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
        registry.add_controller("probe", controller.clone());
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
        let access = Arc::new(AccessControl::new(
            meta.clone(),
            Arc::new(engine_for(&meta, &model).unwrap()),
            Arc::new(principals),
            7,
            clock,
            actor,
        ));
        let plan: Arc<dyn FieldChangedPlan> = Arc::new(PlanOf::of(&[controller]));
        let service = EngineFormEvents::new(engine, access).with_plan(plan);
        Self { service }
    }

    /// The error a `number` change answers for the probe named `probe`.
    async fn error(&self, probe: &str) -> ApiError {
        let body = json!({"doc": {"number": probe}, "changed": "number"});
        let bytes = body.to_string();
        self.service
            .form_event(&ctx(), "order", bytes.as_bytes(), Duration::from_secs(5))
            .await
            .unwrap_err()
    }
}

/// The step plan of the controllers: which `field_changed:<point>` names answer.
struct PlanOf {
    points: Vec<(String, String)>,
}

impl PlanOf {
    fn of(controllers: &[Arc<dyn Controller>]) -> Self {
        let mut points = Vec::new();
        for controller in controllers {
            for step in controller.steps() {
                if let HookPoint::FieldChanged(field) = step.point {
                    points.push((controller.doctype().to_owned(), field));
                }
            }
        }
        Self { points }
    }
}

impl FieldChangedPlan for PlanOf {
    fn answers(&self, doctype: &str, point: &str) -> bool {
        self.points
            .iter()
            .any(|(owner, field)| owner == doctype && field == point)
    }
}

/// The step that raises every probe. Its only step point is `number`; the probe's name is the
/// draft's `number`, and the error it raises is the case under test.
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
            let probe = ctx
                .doc()
                .text("number")
                .ok()
                .flatten()
                .unwrap_or("")
                .to_owned();
            if probe.starts_with("REDACT") || probe.starts_with("OPEN") {
                return Err(error_for(&probe));
            }
            Ok(())
        })
    }
}

/// The error each probe raises. `REDACT-*` names a hidden field somewhere an error can carry it;
/// `OPEN-*` is about readable fields only and must reach the caller unchanged.
fn error_for(probe: &str) -> AppError {
    // Every `REDACT-*` error names a readable field in its issue, so only its text can be the
    // reason it is redacted.
    let on_number =
        |message: &str| AppError::field(FieldPath::field("number"), "probe.value", message);
    match probe {
        // A hidden field's label, in the message.
        "REDACT-LABEL" => on_number("Grand Total cannot be negative."),
        // A hidden field's name in another case.
        "REDACT-CASE" => AppError::new(
            ErrorClass::Validation,
            "document.validation_failed",
            "GRAND_TOTAL is out of range.",
        ),
        // A hidden field's label, in the explanation only.
        "REDACT-EXPLAIN" => {
            on_number("The value is invalid.").explained("Grand Total is derived from the rows.")
        }
        // A hidden field's label, in an issue's code.
        "REDACT-ISSUE-CODE" => AppError::field(
            FieldPath::field("number"),
            "probe.GRAND_TOTAL",
            "The value is invalid.",
        ),
        // A hidden column of a table the draft carries no row of, named with its index.
        "REDACT-CHILD" => {
            on_number("The rate comes from items[2].rate, which the draft does not carry.")
        }
        // The same column, named through the table without an index.
        "REDACT-CHILD-PATH" => on_number("The value of items.rate is derived."),
        // A hidden column of a table the draft omits entirely.
        "REDACT-EMPTY" => on_number("private_note must not be blank."),
        // That column's label.
        "REDACT-EMPTY-LABEL" => on_number("Private Note is required."),
        // A hidden field's label, nested in a detail's object value.
        "REDACT-NESTED" => on_number("The value is invalid.").detail(
            "context",
            json!({"column": "Grand Total", "source": "rows"}),
        ),
        // A hidden field's label as a nested detail key.
        "REDACT-NESTED-KEY" => {
            on_number("The value is invalid.").detail("context", json!({"Grand Total": 0}))
        }
        // A hidden field's label inside an array of details.
        "REDACT-NESTED-ARRAY" => on_number("The value is invalid.")
            .detail("context", json!(["items", {"column": "Rate Per Unit"}])),
        // A hidden `tax` must not swallow an error about the visible `taxonomy`.
        "OPEN-PREFIX" => on_number("The taxonomy is closed."),
        // A visible child column is named freely.
        "OPEN-CHILD" => on_number("items[2].qty must be positive."),
        // Readable fields, detail and all.
        _ => AppError::field(
            FieldPath::field("number"),
            "probe.closed",
            "The order is closed.",
        )
        .detail("closed", Json::Bool(true)),
    }
}

/// The whole output contract of a redacted error: its registry code, the contract's default
/// message, and nothing else.
fn assert_redacted(error: &ApiError, case: &str) {
    assert_eq!(
        *error,
        ApiError::from_code(Code::DocumentValidationFailed),
        "{case}: {error:?}"
    );
    assert!(
        !error.message.contains("Grand"),
        "{case}: the default message leaked a label: {error:?}"
    );
}

async fn redacted(probe: &str) {
    let orders = Orders::new().await;
    let error = orders.error(probe).await;
    assert_redacted(&error, probe);
}

// --------------------------------------------------------------------------------------
// The cases

#[tokio::test]
async fn a_hidden_fields_label_in_the_message_is_redacted() {
    // `grand_total` is hidden; `Grand Total` is its label, and the error says only the label.
    redacted("REDACT-LABEL").await;
}

#[tokio::test]
async fn a_hidden_fields_name_in_another_case_is_redacted() {
    redacted("REDACT-CASE").await;
}

#[tokio::test]
async fn a_hidden_fields_label_in_the_explanation_is_redacted() {
    redacted("REDACT-EXPLAIN").await;
}

#[tokio::test]
async fn a_hidden_fields_label_in_an_issue_code_is_redacted() {
    redacted("REDACT-ISSUE-CODE").await;
}

#[tokio::test]
async fn a_hidden_column_of_a_table_the_draft_has_no_row_of_is_redacted() {
    // The draft carries no `items` row at all, so the column is known only from the child
    // DocType's definition.
    redacted("REDACT-CHILD").await;
}

#[tokio::test]
async fn a_hidden_column_named_through_a_table_path_without_an_index_is_redacted() {
    redacted("REDACT-CHILD-PATH").await;
}

#[tokio::test]
async fn a_hidden_column_of_a_table_the_draft_omits_is_redacted() {
    redacted("REDACT-EMPTY").await;
}

#[tokio::test]
async fn a_hidden_column_label_of_a_table_the_draft_omits_is_redacted() {
    redacted("REDACT-EMPTY-LABEL").await;
}

#[tokio::test]
async fn a_hidden_name_nested_in_a_detail_value_is_redacted() {
    // Only the nested string names the field; the top-level detail value is an object.
    redacted("REDACT-NESTED").await;
}

#[tokio::test]
async fn a_hidden_name_as_a_nested_detail_key_is_redacted() {
    redacted("REDACT-NESTED-KEY").await;
}

#[tokio::test]
async fn a_hidden_name_in_an_array_inside_a_detail_is_redacted() {
    redacted("REDACT-NESTED-ARRAY").await;
}

#[tokio::test]
async fn a_hidden_name_that_is_a_prefix_of_a_visible_field_is_not_redacted() {
    // `tax` is hidden and `taxonomy` is visible: an error about `taxonomy` must reach the caller.
    let orders = Orders::new().await;
    let error = orders.error("OPEN-PREFIX").await;
    assert_eq!(error.code.as_str(), "document.validation_failed");
    assert_eq!(error.message, "The taxonomy is closed.");
    assert_eq!(error.fields.len(), 1);
    assert_eq!(error.fields[0].field, "number");
}

#[tokio::test]
async fn a_visible_child_column_is_not_redacted() {
    let orders = Orders::new().await;
    let error = orders.error("OPEN-CHILD").await;
    assert_eq!(error.message, "items[2].qty must be positive.");
    assert_eq!(error.fields.len(), 1);
    assert_eq!(error.fields[0].field, "number");
}

#[tokio::test]
async fn an_error_about_readable_fields_keeps_its_issues_and_details() {
    let orders = Orders::new().await;
    let error = orders.error("OPEN-PLAIN").await;
    assert_eq!(error.message, "The order is closed.");
    assert_eq!(error.fields.len(), 1);
    assert_eq!(error.fields[0].code, "probe.closed");
    assert_eq!(error.details.get("closed"), Some(&Json::Bool(true)));
}
