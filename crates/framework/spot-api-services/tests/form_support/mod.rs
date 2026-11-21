//! Fixtures for the form-event and permission-summary suites (WP-UI-01.T1).
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value as Json;
use spot_api_services::access::{AccessControl, engine_for};
use spot_api_services::form_event::{EngineFormEvents, FieldChangedPlan};
use spot_app_api::controller::{StepDecl, StepName};
use spot_app_api::prelude::{Decimal, Name, RecordId, Value};
use spot_app_api::{
    AppError, AppResult, BoxFuture as AppFuture, Controller, Document, ErrorClass, FieldPath,
    HookContext, HookPoint, Message,
};
use spot_core::{Clock, ManualClock, SiteId, SystemClock};
use spot_document::engine::{Engine, MemoryPrincipals, V7Ids};
use spot_events_contract::lifecycle::Operation;
use spot_http::api::codec::parse_record_id;
use spot_http::api::registry::Code;
use spot_http::api::service::{ApiError, Credential, RequestCtx};
use spot_http::request::RequestId;
use spot_meta::{Definition, EffectiveMeta, Registry};
use spot_permissions::model::{Model, ModelTexts};
use spot_permissions::{Principal, load};
use spot_seams::RegistryBuilder;
use spot_storage_contract::StorageBackend;
use spot_storage_fake::FakeBackend;

use spot_api_services::form_event::{FormEvent, FormEventService};

/// The signed-in user of the form-event fixtures.
pub const USER: &str = "01970000-0000-7000-8000-0000000000aa";
/// A user with the `ReaderOnly` role in the permission fixtures.
pub const READER: &str = "01970000-0000-7000-8000-0000000000bb";
/// A user with no assignment at all.
pub const NOBODY: &str = "01970000-0000-7000-8000-0000000000cc";
/// A tenant administrator in the permission fixtures.
pub const ADMIN: &str = "01970000-0000-7000-8000-0000000000dd";
/// A principal holding an `if_owner` grant on invoices.
pub const OWNER: &str = "01970000-0000-7000-8000-0000000000de";
/// A principal whose assignment restricts the invoice customer link.
pub const SCOPED: &str = "01970000-0000-7000-8000-0000000000df";
/// The customer fixture id.
pub const CUSTOMER_ACME: &str = "01970000-0000-7000-8000-0000000000c1";
/// A second customer fixture id.
pub const CUSTOMER_GLOBEX: &str = "01970000-0000-7000-8000-0000000000c2";
/// The item fixture id.
pub const ITEM_WIDGET: &str = "01970000-0000-7000-8000-0000000000e1";
/// A second item fixture id.
pub const ITEM_GADGET: &str = "01970000-0000-7000-8000-0000000000e2";
/// A user who reads invoices and lines, but only lines whose `item` is [`ITEM_WIDGET`].
pub const ROW_SCOPED: &str = "01970000-0000-7000-8000-0000000000e0";
/// The document id the golden drafts use for a stored document.
pub const DOC: &str = "01970000-0000-7000-8000-0000000000d1";
/// The first row id the golden requests use.
pub const ROW_ONE: &str = "01970000-0000-7000-8000-0000000000f1";
/// The second row id the golden requests use.
pub const ROW_TWO: &str = "01970000-0000-7000-8000-0000000000f2";

/// A storage name, for tests.
pub fn n(name: &str) -> Name {
    Name::new(name).unwrap()
}

/// A record id, for tests.
pub fn rid(id: &str) -> RecordId {
    parse_record_id(id).unwrap()
}

/// The clock reading every fixture starts at.
pub fn now() -> time::OffsetDateTime {
    time::OffsetDateTime::from_unix_timestamp(1_790_000_000).unwrap()
}

/// The definitions the fixtures build over.
pub fn definitions() -> Vec<Definition> {
    vec![
        Definition::parse("invoice.toml", include_str!("../fixtures/invoice.toml")).unwrap(),
        Definition::parse(
            "invoice_line.toml",
            include_str!("../fixtures/invoice_line.toml"),
        )
        .unwrap(),
        Definition::parse("customer.toml", include_str!("../fixtures/customer.toml")).unwrap(),
        Definition::parse("item.toml", include_str!("../fixtures/item.toml")).unwrap(),
        Definition::parse("region.toml", include_str!("../fixtures/region.toml")).unwrap(),
    ]
}

/// The effective metadata of the fixtures.
pub fn meta() -> Arc<EffectiveMeta> {
    Registry::new(definitions()).unwrap().effective()
}

/// The permission model: `FormUser` reads and writes invoices, sees `secret` masked and
/// `hidden_note` not at all; `ReaderOnly` reads invoices; `NobodyRole` grants nothing.
pub fn model() -> Model {
    load(&ModelTexts {
        grants: r#"
[[entries]]
role = "FormUser"
doctype = "invoice"
rights = ["read", "write", "create", "delete", "submit", "cancel", "correct", "report"]
[[entries]]
role = "FormUser"
doctype = "invoice_line"
rights = ["read", "write"]
[[entries]]
role = "FormUser"
doctype = "customer"
rights = ["read", "write", "create"]
[[entries]]
role = "FormUser"
doctype = "item"
rights = ["read", "write", "create"]
[[entries]]
role = "ReaderOnly"
doctype = "invoice"
rights = ["read"]
[[entries]]
role = "ReaderOnly"
doctype = "invoice_line"
rights = ["read"]
[[entries]]
role = "ReaderOnly"
doctype = "customer"
rights = ["read"]
[[entries]]
role = "OwnerRole"
doctype = "invoice"
rights = ["read", "write"]
if_owner = true
[[entries]]
role = "ScopedRole"
doctype = "invoice"
rights = ["read", "write"]
[[entries]]
role = "ScopedRole"
doctype = "customer"
rights = ["read"]
[[entries]]
role = "RowScopedRole"
doctype = "invoice"
rights = ["read", "write"]
[[entries]]
role = "RowScopedRole"
doctype = "invoice_line"
rights = ["read"]
[[entries]]
role = "Nobody"
doctype = "customer"
rights = ["read"]
"#
        .to_owned(),
        field_grants: r#"
[[entries]]
role = "FormUser"
doctype = "invoice"
target = { group = "pii" }
access = ["read_masked"]
[[entries]]
role = "ReaderOnly"
doctype = "invoice"
target = { group = "pii" }
access = ["read_masked"]
"#
        .to_owned(),
        groups: r#"
[[entries]]
name = "pii"
[[entries]]
name = "internal"
"#
        .to_owned(),
        shares: String::new(),
    })
    .unwrap_or_else(|e| panic!("fixture model: {:?}", e.as_slice()))
}

/// The principals the fixtures resolve.
pub fn principals() -> MemoryPrincipals {
    let mut source = MemoryPrincipals::new();
    for user in [USER, READER, ADMIN, OWNER, SCOPED, ROW_SCOPED, NOBODY] {
        source = source.with(&rid(user), principal_of(user));
    }
    source
}

/// The principal one fixture user resolves to.
pub fn principal_of(user: &str) -> Principal {
    match user {
        USER => user_principal(),
        READER => Principal::with_roles(READER, &["ReaderOnly"]),
        ADMIN => Principal::with_roles(ADMIN, &[spot_permissions::TENANT_ADMINISTRATOR]),
        OWNER => Principal::with_roles(OWNER, &["OwnerRole"]),
        SCOPED => scoped_principal(),
        ROW_SCOPED => row_scoped_principal(),
        NOBODY => Principal::user(NOBODY, Vec::new()),
        other => panic!("no fixture principal for {other}"),
    }
}

/// The metadata's permission engine with the fixture model.
pub fn permission_engine() -> Arc<spot_permissions::PermissionEngine> {
    Arc::new(engine_for(&meta(), &model()).unwrap())
}

/// The request context every fixture call carries.
pub fn ctx() -> RequestCtx {
    RequestCtx {
        site: SiteId::new("test").unwrap(),
        credential: Some(Credential::new("form-user")),
        idempotency_key: None,
        language: None,
        request_id: RequestId::new("form-event-test").unwrap(),
        peer: None,
    }
}

/// The actor resolver: `form-user` is [`USER`], anything else is refused.
pub fn resolver() -> Arc<spot_api_services::engine_resources::ActorResolver> {
    resolver_for(USER)
}

/// The actor resolver for one fixture user.
pub fn resolver_for(user: &'static str) -> Arc<spot_api_services::engine_resources::ActorResolver> {
    Arc::new(move |ctx: &RequestCtx| {
        if ctx.site.as_str() != "test"
            || ctx.credential.as_ref().map(|c| c.token.as_str()) != Some("form-user")
        {
            return Err(ApiError::from_code(Code::AuthRequired));
        }
        Ok(spot_app_api::Actor::User {
            id: rid(user),
            via: None,
        })
    })
}

/// The access view over the fixture metadata and model.
pub fn access() -> Arc<AccessControl> {
    Arc::new(AccessControl::new(
        meta(),
        permission_engine(),
        Arc::new(principals()),
        7,
        Arc::new(ManualClock::new(now())),
        resolver(),
    ))
}

/// The step plan built from the registered controllers: which `field_changed:<point>` names
/// answer on each DocType.
#[derive(Default)]
pub struct Plan {
    points: BTreeMap<String, BTreeSet<String>>,
}

impl Plan {
    /// The plan of a controller set.
    pub fn of(controllers: &[Arc<dyn Controller>]) -> Self {
        let mut points: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for controller in controllers {
            let set = points.entry(controller.doctype().to_owned()).or_default();
            for step in controller.steps() {
                if let HookPoint::FieldChanged(field) = step.point {
                    set.insert(field);
                }
            }
        }
        Plan { points }
    }
}

impl FieldChangedPlan for Plan {
    fn answers(&self, doctype: &str, point: &str) -> bool {
        self.points
            .get(doctype)
            .is_some_and(|points| points.contains(point))
    }
}

/// The form-event fixture: a real engine over the fake backend, with the controller below.
pub struct Forms {
    pub service: EngineFormEvents<Engine<FakeBackend>>,
    pub engine: Arc<Engine<FakeBackend>>,
    pub access: Arc<AccessControl>,
    pub clock: Arc<dyn Clock>,
    pub plan: Arc<dyn FieldChangedPlan>,
}

impl Forms {
    /// Build the fixture: the signed-in [`USER`] over a manual clock.
    pub async fn new() -> Self {
        Self::build(Arc::new(ManualClock::new(now())), USER).await
    }

    /// The same fixture with `user` signed in.
    pub async fn for_user(user: &'static str) -> Self {
        Self::build(Arc::new(ManualClock::new(now())), user).await
    }

    /// The same fixture on the real clock, for the deadline tests.
    pub async fn system_clock() -> Self {
        Self::build(Arc::new(SystemClock::new()), USER).await
    }

    async fn build(clock: Arc<dyn Clock>, user: &'static str) -> Self {
        let meta = meta();
        let controller = Arc::new(InvoiceForms) as Arc<dyn Controller>;
        let mut registry = RegistryBuilder::new();
        registry.add_controller("probe", controller.clone());
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
            Arc::new(move |ctx: &RequestCtx| {
                if ctx.site.as_str() != "test"
                    || ctx.credential.as_ref().map(|c| c.token.as_str()) != Some("form-user")
                {
                    return Err(ApiError::from_code(Code::AuthRequired));
                }
                Ok(spot_app_api::Actor::User {
                    id: rid(user),
                    via: None,
                })
            });
        let access = Arc::new(AccessControl::new(
            meta,
            permission_engine(),
            Arc::new(principals()),
            7,
            clock.clone(),
            actor,
        ));
        let plan: Arc<dyn FieldChangedPlan> = Arc::new(Plan::of(&[controller]));
        let service = EngineFormEvents::new(engine.clone(), access.clone()).with_plan(plan.clone());
        Self {
            service,
            engine,
            access,
            clock,
            plan,
        }
    }

    /// A second service over the same engine and permission view, with other bounds. The rate
    /// limiter is per service, so a test starts from a full bucket.
    pub fn service_with(
        &self,
        limits: spot_api_services::form_event::Limits,
    ) -> EngineFormEvents<Engine<FakeBackend>> {
        EngineFormEvents::new(self.engine.clone(), self.access.clone())
            .with_limits(limits)
            .with_plan(self.plan.clone())
    }

    /// Run one form event over the fixture metadata.
    pub async fn call(&self, body: &Json) -> Result<FormEvent, ApiError> {
        self.raw(body.to_string().as_bytes()).await
    }

    /// Run one form event over the exact bytes.
    pub async fn raw(&self, body: &[u8]) -> Result<FormEvent, ApiError> {
        self.service
            .form_event(&ctx(), "invoice", body, Duration::from_secs(5))
            .await
    }

    /// The committed-write count the fake backend reports (its health detail), for the
    /// write-free assertions.
    pub async fn commits(&self) -> String {
        let health = self.engine.backend().health().await;
        format!("{health:?}")
    }
}

/// The invoice form steps: the derivations a metadata-driven form asks the server for.
pub struct InvoiceForms;

impl Controller for InvoiceForms {
    fn doctype(&self) -> &str {
        "invoice"
    }

    fn steps(&self) -> Vec<StepDecl> {
        [
            "customer", "item", "qty", "rate", "lines", "discount", "number",
        ]
        .into_iter()
        .map(|field| StepDecl {
            name: StepName::new(&format!("probe:{field}")).unwrap(),
            point: HookPoint::FieldChanged(field.to_owned()),
            priority: 0,
            budget: Duration::from_secs(5),
        })
        .collect()
    }

    fn run_step<'a>(
        &'a self,
        step: &'a StepName,
        ctx: &'a mut dyn HookContext,
    ) -> AppFuture<'a, AppResult<()>> {
        Box::pin(async move {
            let point = ctx.point().clone();
            let HookPoint::FieldChanged(field) = point else {
                return Err(AppError::new(
                    ErrorClass::Internal,
                    "platform.phase_violation",
                    format!("{} is not a form step.", step.as_str()),
                ));
            };
            match field.as_str() {
                "customer" => {
                    // A step that outlives a short request budget, for the deadline tests.
                    let slow = ctx
                        .doc()
                        .text("number")
                        .ok()
                        .flatten()
                        .is_some_and(|number| number.starts_with("INV-SLOW"));
                    if slow {
                        tokio::time::sleep(Duration::from_millis(250)).await;
                    }
                    let customer = ctx.doc().get("customer");
                    let name = match customer {
                        Value::Id(id) if id.to_string() == CUSTOMER_ACME => "Acme Ltd",
                        Value::Id(id) if id.to_string() == CUSTOMER_GLOBEX => "Globex",
                        _ => "",
                    };
                    let doc = ctx.doc_mut()?;
                    doc.set(n("customer_name"), name.to_owned());
                    ctx.message(
                        Message::info("document.created", "Customer selected.")
                            .on(FieldPath::field("customer_name")),
                    );
                }
                "item" => {
                    fill_rows(ctx.doc_mut()?);
                    recompute(ctx.doc_mut()?);
                }
                "qty" | "rate" | "lines" => {
                    recompute(ctx.doc_mut()?);
                    if field == "qty" {
                        let out = ctx.doc().rows("lines").to_vec();
                        for (index, row) in out.iter().enumerate() {
                            let qty = decimal(row, "qty");
                            if qty > Decimal::from(10) {
                                ctx.message(
                                    Message::warning("document.created", "Large quantity.")
                                        .on(FieldPath::row("lines", index as u32 + 1, "qty")),
                                );
                            }
                        }
                        if !out.is_empty() {
                            ctx.message(
                                Message::info("document.created", "Internal note checked.")
                                    .on(FieldPath::field("hidden_note")),
                            );
                        }
                    }
                }
                "discount" => recompute(ctx.doc_mut()?),
                "number" if is_error_probe(ctx.doc()) => return Err(error_probe(ctx.doc())),
                "number" => {
                    // A server-derived internal code (masked for this user) and an audit note
                    // (hidden from this user): neither may leak unless the caller may read it.
                    let number = ctx
                        .doc()
                        .text("number")
                        .ok()
                        .flatten()
                        .unwrap_or("")
                        .to_owned();
                    let doc = ctx.doc_mut()?;
                    doc.set(n("secret"), format!("SEC-{number}"));
                    doc.set(n("hidden_note"), format!("audit:{number}"));
                    ctx.message(
                        Message::info("document.created", "Internal code derived.")
                            .on(FieldPath::field("secret")),
                    );
                    ctx.message(
                        Message::info("document.created", "Audit note written.")
                            .on(FieldPath::field("hidden_note")),
                    );
                }
                other => {
                    return Err(AppError::new(
                        ErrorClass::Internal,
                        "platform.phase_violation",
                        format!("no form step answers `{other}`."),
                    ));
                }
            }
            Ok(())
        })
    }
}

/// Derive every row's item name and rate from the item link, as a fetch-from would.
fn fill_rows(doc: &mut Document) {
    for row in doc.rows_mut(n("lines")).iter_mut() {
        let item = row.fields.get(&n("item")).cloned();
        let (name, rate) = match item {
            Some(Value::Id(id)) if id.to_string() == ITEM_WIDGET => ("Widget", "12.50"),
            Some(Value::Id(id)) if id.to_string() == ITEM_GADGET => ("Gadget", "7.25"),
            _ => ("", "0"),
        };
        row.fields
            .insert(n("item_name"), Value::Text(name.to_owned()));
        row.fields
            .insert(n("rate"), Value::Decimal(rate.parse().unwrap()));
    }
}

/// Amounts, subtotal and grand total — exact decimals, never floats (ADR-12).
fn recompute(doc: &mut Document) {
    let mut subtotal = Decimal::ZERO;
    for row in doc.rows_mut(n("lines")).iter_mut() {
        let qty = row
            .fields
            .get(&n("qty"))
            .and_then(as_decimal)
            .unwrap_or(Decimal::ZERO);
        let rate = row
            .fields
            .get(&n("rate"))
            .and_then(as_decimal)
            .unwrap_or(Decimal::ZERO);
        let mut amount = (qty * rate).round_dp(2);
        amount.rescale(2);
        row.fields.insert(n("amount"), Value::Decimal(amount));
        subtotal += amount;
    }
    let mut discount = doc
        .decimal("discount")
        .ok()
        .flatten()
        .unwrap_or(Decimal::ZERO);
    discount.rescale(2);
    let mut total = subtotal - discount;
    total.rescale(2);
    doc.set(n("subtotal"), subtotal);
    doc.set(n("grand_total"), total);
}

fn as_decimal(value: &Value) -> Option<Decimal> {
    match value {
        Value::Decimal(d) => Some(*d),
        _ => None,
    }
}

fn decimal(row: &spot_app_api::ChildRow, field: &str) -> Decimal {
    row.fields
        .get(&n(field))
        .and_then(as_decimal)
        .unwrap_or(Decimal::ZERO)
}

/// A principal whose one assignment restricts the invoice's `customer` link to Acme.
pub fn scoped_principal() -> Principal {
    Principal::user(
        SCOPED,
        vec![spot_permissions::AssignmentContext {
            id: "scoped".to_owned(),
            roles: vec![spot_permissions::Role("ScopedRole".to_owned())],
            scope: vec![spot_permissions::UserPermission {
                user: SCOPED.to_owned(),
                doctype: Some("invoice".to_owned()),
                field_or_link_doctype: "invoice.customer".to_owned(),
                allowed: vec![CUSTOMER_ACME.to_owned()],
                applies_to: spot_permissions::AppliesTo::AllDoctypes,
                include_descendants: false,
            }],
        }],
    )
}

/// A principal whose one assignment restricts the invoice line's `item` link to the widget.
pub fn row_scoped_principal() -> Principal {
    Principal::user(
        ROW_SCOPED,
        vec![spot_permissions::AssignmentContext {
            id: "row-scoped".to_owned(),
            roles: vec![spot_permissions::Role("RowScopedRole".to_owned())],
            scope: vec![spot_permissions::UserPermission {
                user: ROW_SCOPED.to_owned(),
                doctype: Some("invoice_line".to_owned()),
                field_or_link_doctype: "invoice_line.item".to_owned(),
                allowed: vec![ITEM_WIDGET.to_owned()],
                applies_to: spot_permissions::AppliesTo::AllDoctypes,
                include_descendants: false,
            }],
        }],
    )
}

/// A principal's name for the permission fixtures.
pub fn user_principal() -> Principal {
    Principal::with_roles(USER, &["FormUser"])
}

/// The operations the permission matrix asks the decision core about, in the contract's order.
pub const ACTION_NAMES: [&str; 8] = [
    "read", "create", "write", "delete", "submit", "cancel", "correct", "rename",
];

/// The decision-core action behind each contract right.
pub fn action_of(right: &str) -> spot_permissions::Action {
    match right {
        "read" => spot_permissions::Action::Read,
        "create" => spot_permissions::Action::Op(Operation::Create),
        "write" => spot_permissions::Action::Op(Operation::Update),
        "delete" => spot_permissions::Action::Op(Operation::Delete),
        "submit" => spot_permissions::Action::Op(Operation::Post),
        "cancel" => spot_permissions::Action::Op(Operation::Cancel),
        "correct" => spot_permissions::Action::Correct,
        "rename" => spot_permissions::Action::Op(Operation::Rename),
        other => panic!("unknown right {other}"),
    }
}

fn is_error_probe(doc: &Document) -> bool {
    doc.text("number")
        .ok()
        .flatten()
        .is_some_and(|number| number.starts_with("INV-ERR"))
}

/// The failures a step may raise: one naming a hidden field in its issue, message, explanation
/// and details; one naming it only in its message; one naming only a readable field.
fn error_probe(doc: &Document) -> AppError {
    let number = doc.text("number").ok().flatten().unwrap_or("").to_owned();
    match number.as_str() {
        "INV-ERR-HIDDEN" => AppError::field(
            FieldPath::field("hidden_note"),
            "probe.bad_note",
            "hidden_note must not be blank.",
        )
        .explained("The hidden_note column is checked against the audit log.")
        .detail("field", "hidden_note"),
        "INV-ERR-TEXT" => AppError::new(
            ErrorClass::Validation,
            "document.validation_failed",
            "Cannot continue: hidden_note is stale.",
        ),
        _ => AppError::field(
            FieldPath::field("customer"),
            "probe.customer_closed",
            "The customer is closed.",
        )
        .detail("closed", true),
    }
}
