//! The app API contract suite: behaviour every host must show (WP-CON-02; run by the stub host
//! here and by the document engine in WP-DOC-04/WP-DOC-10).
//!
//! ```ignore
//! #[tokio::test]
//! async fn app_api_contract() {
//!     spot_app_api::contract_tests::run_all(&MyFixture).await.assert_all_passed();
//! }
//! ```
//!
//! Each case gets a fresh host with the [`catalog`] defined, registers its own probe
//! controllers, and checks one rule: the ADR-51 order, the phase rules, rollback, committed
//! handling, merge-patch semantics, state rules, typed field errors, lifecycle events, messages,
//! overrides, methods, form events and seams.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use rust_decimal::Decimal;
use serde_json::{Value as Json, json};
use spot_core::SiteId;
use spot_events_contract::lifecycle::Operation;
use spot_events_contract::{CommandId, PayloadDocument};
use spot_storage_contract::{Name, OutboxEvent, RecordId, Value, Version};
use time::OffsetDateTime;

use crate::BoxFuture;
use crate::context::{
    Actor, DocumentService, HookContext, Invocation, JobRequest, MethodContext, Outcome,
};
use crate::controller::{Controller, MethodDecl, MethodRight, StepDecl, StepName};
use crate::document::{DocState, Document, DocumentPatch, RowPatch};
use crate::effects::{
    ActionRef, EffectClassName, EffectDecl, EffectKind, EffectReceipt, Hex32, OpaqueRef,
    OperationReceipt, ReceiptState, Reversal, Reversibility, StepEffects, StoreName,
    validate_declarations,
};
use crate::error::{AppError, AppResult, ErrorClass};
use crate::lifecycle::{CorrectPart, HookPoint, Phase, Stage, Target, correct_sequence, sequence};
use crate::lookup::{
    LookupPageWire, LookupRequest, LookupRowWire, request_from_wire, request_wire,
};
use crate::message::Message;
use crate::meta::{
    AttachFormat, DecimalFormat, DocTypeDef, FieldDef, FieldHints, FieldKind, FieldSecurity,
    JsonFormat, LongTextFormat, MaskFormat, ProtectionGroupDef, SyncClassHint, Tag, TextFormat,
};
use crate::seams::{OverrideKind, SeamProvider, StepOverride};

/// Why a case failed.
#[derive(Clone, Debug, PartialEq)]
pub struct Failure(pub String);

impl From<AppError> for Failure {
    fn from(e: AppError) -> Self {
        Failure(format!("unexpected error: {} ({})", e.code, e.message))
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Result of one case.
pub type CaseResult = Result<(), Failure>;

/// A host under test: the document service plus the setup and inspection the suite needs.
pub trait TestHost: DocumentService {
    /// Defines a DocType.
    fn define(&self, def: DocTypeDef);
    /// Registers a controller.
    fn register(&self, controller: Arc<dyn Controller>);
    /// Installs a step override.
    fn add_override(&self, o: StepOverride);
    /// Provides a seam.
    fn provide_seam(&self, name: &str, version: u32, provider: Arc<dyn SeamProvider>);
    /// Outbox events published so far.
    fn outbox(&self) -> Vec<OutboxEvent>;
    /// Jobs delivered so far.
    fn jobs(&self) -> Vec<JobRequest>;
    /// Committed steps that failed.
    fn committed_failures(&self) -> Vec<(String, AppError)>;
    /// Whether a document is stored, and as what.
    fn stored(&self, doctype: &str, id: RecordId) -> Option<Document>;
    /// A fresh record id.
    fn fresh_id(&self) -> RecordId;
}

/// Supplies fresh hosts.
pub trait Fixture: Sync {
    /// A new, empty host.
    fn fresh(&self) -> Box<dyn TestHost>;
}

/// The outcome of one case.
#[derive(Clone, Debug)]
pub struct CaseOutcome {
    /// Case name.
    pub case: &'static str,
    /// Its result.
    pub result: CaseResult,
}

/// Outcomes of a run.
#[derive(Clone, Debug, Default)]
pub struct Report {
    /// One per case.
    pub outcomes: Vec<CaseOutcome>,
}

impl Report {
    /// The failed cases.
    pub fn failures(&self) -> Vec<&CaseOutcome> {
        self.outcomes.iter().filter(|o| o.result.is_err()).collect()
    }

    /// Panics with every failure listed, unless all cases passed.
    pub fn assert_all_passed(&self) {
        let failed = self.failures();
        if !failed.is_empty() {
            let lines: Vec<String> = failed
                .iter()
                .map(|o| {
                    format!(
                        "  {}: {}",
                        o.case,
                        o.result.as_ref().err().map_or("", |f| f.0.as_str())
                    )
                })
                .collect();
            panic!(
                "{} of {} app API contract cases failed:\n{}",
                failed.len(),
                self.outcomes.len(),
                lines.join("\n")
            );
        }
    }
}

/// Checks a controller's declared methods against the contract's consistency rule
/// ([`MethodRight::contradicts`]): a `Read` method that changes documents fails, and so does a
/// `Write`, `Create`, `Submit` or `Cancel` method that does not; a `Permission` method may be
/// either. A host runs it when it registers a controller, so a controller whose declared right
/// contradicts `mutates` is refused at registration rather than authorised as something it did
/// not ask for.
pub fn check_method_rights(controller: &dyn Controller) -> CaseResult {
    for decl in controller.methods() {
        if decl.contradicts() {
            return Err(Failure(format!(
                "{}: method `{}` declares right {} but mutates = {} (see MethodRight)",
                controller.doctype(),
                decl.name,
                decl.right,
                decl.mutates
            )));
        }
    }
    Ok(())
}

// ------------------------------------------------------------------------------------------
// Fixture data
// ------------------------------------------------------------------------------------------

fn n(s: &str) -> Name {
    Name::new(s).unwrap_or_else(|e| panic!("{e}"))
}

fn field(name: &str, kind: FieldKind, required: bool) -> FieldDef {
    FieldDef {
        name: n(name),
        kind,
        required,
        allow_on_posted: false,
        label: name.to_owned(),
        read_only: false,
        default: None,
        lookup: None,
        hints: FieldHints::default(),
        security: FieldSecurity::default(),
        multiple: false,
        create_only: false,
    }
}

/// The suite's DocTypes: `customer` (not posted, title `customer_name`), `invoice` (posted,
/// with `items` rows of `invoice_item`; `remarks` may change after posting), and `note`.
pub fn catalog() -> Vec<DocTypeDef> {
    let mut remarks = field(
        "remarks",
        FieldKind::Text {
            max_chars: None,
            format: TextFormat::Plain,
        },
        false,
    );
    remarks.allow_on_posted = true;
    vec![
        DocTypeDef {
            name: n("customer"),
            label: "Customer".into(),
            is_posted: false,
            is_child: false,
            title_field: Some(n("customer_name")),
            fields: vec![
                field(
                    "customer_name",
                    FieldKind::Text {
                        max_chars: Some(140),
                        format: TextFormat::Plain,
                    },
                    true,
                ),
                field(
                    "credit_limit",
                    FieldKind::Decimal {
                        scale: 2,
                        precision: 18,
                        format: DecimalFormat::Plain,
                    },
                    false,
                ),
            ],
            protection_groups: Vec::new(),
            version: 1,
        },
        DocTypeDef {
            name: n("invoice"),
            label: "Invoice".into(),
            is_posted: true,
            is_child: false,
            title_field: Some(n("number")),
            fields: vec![
                field(
                    "number",
                    FieldKind::Text {
                        max_chars: Some(40),
                        format: TextFormat::Plain,
                    },
                    false,
                ),
                field(
                    "customer",
                    FieldKind::Link {
                        doctype: n("customer"),
                    },
                    true,
                ),
                remarks,
                field(
                    "total",
                    FieldKind::Decimal {
                        scale: 2,
                        precision: 18,
                        format: DecimalFormat::Plain,
                    },
                    false,
                ),
                field(
                    "items",
                    FieldKind::Table {
                        doctype: n("invoice_item"),
                    },
                    false,
                ),
                field(
                    "amended_from",
                    FieldKind::Link {
                        doctype: n("invoice"),
                    },
                    false,
                ),
            ],
            protection_groups: Vec::new(),
            version: 1,
        },
        DocTypeDef {
            name: n("invoice_item"),
            label: "Invoice Item".into(),
            is_posted: false,
            is_child: true,
            title_field: None,
            fields: vec![
                field(
                    "item",
                    FieldKind::Text {
                        max_chars: None,
                        format: TextFormat::Plain,
                    },
                    true,
                ),
                field(
                    "qty",
                    FieldKind::Decimal {
                        scale: 3,
                        precision: 18,
                        format: DecimalFormat::Plain,
                    },
                    true,
                ),
            ],
            protection_groups: Vec::new(),
            version: 1,
        },
        DocTypeDef {
            name: n("note"),
            label: "Note".into(),
            is_posted: false,
            is_child: false,
            title_field: None,
            fields: vec![field(
                "text",
                FieldKind::Text {
                    max_chars: None,
                    format: TextFormat::Plain,
                },
                false,
            )],
            protection_groups: Vec::new(),
            version: 1,
        },
    ]
}

fn inv() -> Invocation {
    Invocation {
        site: SiteId::new("stub.test").unwrap_or_else(|e| panic!("{e}")),
        actor: Actor::System {
            component: n("contract_tests"),
        },
        command_id: CommandId::new("cmd-1").ok(),
        language: None,
        now: OffsetDateTime::UNIX_EPOCH,
    }
}

fn dec(s: &str) -> Value {
    Value::Decimal(s.parse::<Decimal>().unwrap_or_else(|e| panic!("{e}")))
}

fn d(s: &str) -> rust_decimal::Decimal {
    s.parse::<Decimal>().unwrap_or_else(|e| panic!("{e}"))
}

fn text(s: &str) -> Value {
    Value::Text(s.to_owned())
}

// ------------------------------------------------------------------------------------------
// Probe controllers
// ------------------------------------------------------------------------------------------

type StepFn =
    Arc<dyn for<'a> Fn(&'a mut dyn HookContext) -> BoxFuture<'a, AppResult<()>> + Send + Sync>;
type MethodFn = Arc<
    dyn for<'a> Fn(Json, &'a mut dyn MethodContext) -> BoxFuture<'a, AppResult<Json>> + Send + Sync,
>;

/// A controller built from closures.
struct Probe {
    doctype: String,
    steps: Vec<(StepDecl, StepFn)>,
    methods: Vec<(MethodDecl, MethodFn)>,
}

impl Probe {
    fn new(doctype: &str) -> Self {
        Self {
            doctype: doctype.to_owned(),
            steps: Vec::new(),
            methods: Vec::new(),
        }
    }

    fn step(mut self, name: &str, point: HookPoint, priority: i32, f: StepFn) -> Self {
        let decl = StepDecl {
            name: StepName::new(name).unwrap_or_else(|e| panic!("{e}")),
            point,
            priority,
            budget: Duration::from_secs(5),
        };
        self.steps.push((decl, f));
        self
    }

    fn method(mut self, name: &str, mutates: bool, f: MethodFn) -> Self {
        let decl = MethodDecl::new(n(name), mutates);
        self.methods.push((decl, f));
        self
    }

    fn method_with_right(
        mut self,
        name: &str,
        mutates: bool,
        right: MethodRight,
        f: MethodFn,
    ) -> Self {
        let decl = MethodDecl::new(n(name), mutates).with_right(right);
        self.methods.push((decl, f));
        self
    }
}

impl Controller for Probe {
    fn doctype(&self) -> &str {
        &self.doctype
    }
    fn steps(&self) -> Vec<StepDecl> {
        self.steps.iter().map(|(d, _)| d.clone()).collect()
    }
    fn methods(&self) -> Vec<MethodDecl> {
        self.methods.iter().map(|(d, _)| d.clone()).collect()
    }
    fn run_step<'a>(
        &'a self,
        step: &'a StepName,
        ctx: &'a mut dyn HookContext,
    ) -> BoxFuture<'a, AppResult<()>> {
        match self.steps.iter().find(|(d, _)| &d.name == step) {
            Some((_, f)) => f(ctx),
            None => Box::pin(async move {
                Err(AppError::new(
                    ErrorClass::Internal,
                    "platform.unknown_step",
                    step.as_str(),
                ))
            }),
        }
    }
    fn call_method<'a>(
        &'a self,
        method: &'a str,
        args: Json,
        ctx: &'a mut dyn MethodContext,
    ) -> BoxFuture<'a, AppResult<Json>> {
        match self.methods.iter().find(|(d, _)| d.name.as_str() == method) {
            Some((_, f)) => f(args, ctx),
            None => Box::pin(async move {
                Err(AppError::new(
                    ErrorClass::MethodNotFound,
                    "method.not_found",
                    method,
                ))
            }),
        }
    }
}

fn step_fn<F>(f: F) -> StepFn
where
    F: for<'a> Fn(&'a mut dyn HookContext) -> BoxFuture<'a, AppResult<()>> + Send + Sync + 'static,
{
    Arc::new(f)
}

fn method_fn<F>(f: F) -> MethodFn
where
    F: for<'a> Fn(Json, &'a mut dyn MethodContext) -> BoxFuture<'a, AppResult<Json>>
        + Send
        + Sync
        + 'static,
{
    Arc::new(f)
}

type Log = Arc<Mutex<Vec<String>>>;

fn push(log: &Log, s: String) {
    log.lock().unwrap_or_else(|p| p.into_inner()).push(s);
}

fn read(log: &Log) -> Vec<String> {
    log.lock().unwrap_or_else(|p| p.into_inner()).clone()
}

/// Every hook point that can fire for these operations.
fn all_points() -> Vec<HookPoint> {
    let mut out = Vec::new();
    for op in Operation::ALL {
        for s in sequence(op) {
            if let Stage::Hook(p) = s
                && !out.contains(&p)
            {
                out.push(p);
            }
        }
    }
    out
}

/// A probe that logs `<doctype>:<hook point>` at every hook point, plus the version it sees.
fn recorder(doctype: &str, log: &Log) -> Probe {
    let mut p = Probe::new(doctype);
    for (i, point) in all_points().into_iter().enumerate() {
        let log = log.clone();
        let label = format!("{doctype}:{}", point.name());
        p = p.step(
            &format!("probe:rec_{i}"),
            point,
            0,
            step_fn(move |ctx| {
                let entry = format!("{label}@{}", ctx.doc().version().map_or(0, |v| v.0));
                push(&log, entry);
                Box::pin(async { Ok(()) })
            }),
        );
    }
    p
}

fn expected_names(doctype: &str, op: Operation) -> Vec<String> {
    sequence(op)
        .into_iter()
        .filter_map(|s| match s {
            Stage::Hook(p) => Some(format!("{doctype}:{}", p.name())),
            _ => None,
        })
        .collect()
}

fn strip_versions(v: Vec<String>) -> Vec<String> {
    v.into_iter()
        .map(|s| s.split('@').next().unwrap_or("").to_owned())
        .collect()
}

// ------------------------------------------------------------------------------------------
// Helpers
// ------------------------------------------------------------------------------------------

fn check(cond: bool, what: impl Into<String>) -> CaseResult {
    if cond {
        Ok(())
    } else {
        Err(Failure(what.into()))
    }
}

/// The `AppError` of a `validate()` that must have failed, failing the case when the
/// definition was accepted (the definition-level twin of [`expect_err`]).
fn refusal_of(r: AppResult<()>, what: &str) -> Result<AppError, Failure> {
    match r {
        Ok(()) => Err(Failure(format!(
            "expected {what} to be refused, got success"
        ))),
        Err(e) => Ok(e),
    }
}

fn expect_err<T: fmt::Debug>(r: AppResult<T>, code: &str) -> Result<AppError, Failure> {
    match r {
        Ok(v) => Err(Failure(format!("expected {code}, got success: {v:?}"))),
        Err(e) if e.code == code => Ok(e),
        Err(e) => Err(Failure(format!(
            "expected {code}, got {} ({})",
            e.code, e.message
        ))),
    }
}

async fn new_customer(h: &dyn TestHost, name: &str) -> Result<Document, Failure> {
    let mut d = Document::new(n("customer"), h.fresh_id());
    d.set(n("customer_name"), text(name));
    Ok(h.create(&inv(), d).await?.value)
}

async fn new_invoice(
    h: &dyn TestHost,
    customer: RecordId,
    items: &[(&str, &str)],
) -> Result<Document, Failure> {
    let mut d = Document::new(n("invoice"), h.fresh_id());
    d.set(n("customer"), Value::Id(customer));
    d.set(n("number"), text("INV-1"));
    let rows = d.rows_mut(n("items"));
    for (item, qty) in items {
        let mut f = BTreeMap::new();
        f.insert(n("item"), text(item));
        f.insert(n("qty"), dec(qty));
        rows.push(crate::document::ChildRow {
            id: h.fresh_id(),
            fields: f,
        });
    }
    Ok(h.create(&inv(), d).await?.value)
}

type Ids = Arc<Mutex<Vec<RecordId>>>;

/// Pre-generated ids that probe steps take one at a time (steps cannot ask the host for ids).
fn ids(h: &dyn TestHost, k: usize) -> Ids {
    Arc::new(Mutex::new((0..k).map(|_| h.fresh_id()).collect()))
}

fn take(ids: &Ids) -> AppResult<RecordId> {
    ids.lock()
        .unwrap_or_else(|p| p.into_inner())
        .pop()
        .ok_or_else(|| {
            AppError::new(
                ErrorClass::Internal,
                "probe.out_of_ids",
                "probe ran more often than expected",
            )
        })
}

fn version(d: &Document) -> Version {
    d.version().unwrap_or(Version(0))
}

fn events_of(h: &dyn TestHost, topic: &str) -> Vec<OutboxEvent> {
    h.outbox()
        .into_iter()
        .filter(|e| e.topic.as_str() == topic)
        .collect()
}

// ------------------------------------------------------------------------------------------
// Cases
// ------------------------------------------------------------------------------------------

async fn order_of_every_operation(h: &dyn TestHost) -> CaseResult {
    let log: Log = Arc::default();
    h.register(Arc::new(recorder("invoice", &log)));
    h.register(Arc::new(recorder("customer", &log)));
    let c = new_customer(h, "Acme").await?;
    check(
        strip_versions(read(&log)) == expected_names("customer", Operation::Create),
        format!("create order: {:?}", read(&log)),
    )?;
    // The write happens between validate_* and after_*: versions seen are 0 then 1.
    let seen = read(&log);
    check(
        seen.iter().take(4).all(|s| s.ends_with("@0"))
            && seen.iter().skip(4).all(|s| s.ends_with("@1")),
        format!("write position: {seen:?}"),
    )?;

    log.lock().unwrap_or_else(|p| p.into_inner()).clear();
    let patch = DocumentPatch {
        fields: [(n("credit_limit"), dec("10.00"))].into(),
        ..Default::default()
    };
    h.update(&inv(), "customer", c.id(), patch, version(&c))
        .await?;
    check(
        strip_versions(read(&log)) == expected_names("customer", Operation::Update),
        format!("update order: {:?}", read(&log)),
    )?;

    let i = new_invoice(h, c.id(), &[("A", "1")]).await?;
    for (op, f) in [(Operation::Post, "post"), (Operation::Cancel, "cancel")] {
        log.lock().unwrap_or_else(|p| p.into_inner()).clear();
        let cur = h
            .stored("invoice", i.id())
            .map(|d| version(&d))
            .unwrap_or(Version(0));
        let r = if f == "post" {
            h.post(&inv(), "invoice", i.id(), cur).await
        } else {
            h.cancel(&inv(), "invoice", i.id(), cur).await
        };
        r?;
        check(
            strip_versions(read(&log)) == expected_names("invoice", op),
            format!("{f} order: {:?}", read(&log)),
        )?;
    }

    log.lock().unwrap_or_else(|p| p.into_inner()).clear();
    let c2 = new_customer(h, "Beta").await?;
    log.lock().unwrap_or_else(|p| p.into_inner()).clear();
    h.rename(&inv(), "customer", c2.id(), "Beta Ltd", version(&c2))
        .await?;
    check(
        strip_versions(read(&log)) == expected_names("customer", Operation::Rename),
        format!("rename order: {:?}", read(&log)),
    )?;

    log.lock().unwrap_or_else(|p| p.into_inner()).clear();
    let c3 = h
        .stored("customer", c2.id())
        .ok_or_else(|| Failure("renamed customer missing".into()))?;
    h.delete(&inv(), "customer", c3.id(), version(&c3)).await?;
    check(
        strip_versions(read(&log)) == expected_names("customer", Operation::Delete),
        format!("delete order: {:?}", read(&log)),
    )
}

async fn order_of_correct(h: &dyn TestHost) -> CaseResult {
    let log: Log = Arc::default();
    let c = new_customer(h, "Acme").await?;
    let i = new_invoice(h, c.id(), &[("A", "1")]).await?;
    let i = h.post(&inv(), "invoice", i.id(), version(&i)).await?.value;
    h.register(Arc::new(recorder("invoice", &log)));
    let draft = h
        .correct(&inv(), "invoice", i.id(), version(&i))
        .await?
        .value;
    let expected: Vec<String> = correct_sequence()
        .into_iter()
        .filter_map(|(_, s)| match s {
            Stage::Hook(p) => Some(format!("invoice:{}", p.name())),
            _ => None,
        })
        .collect();
    check(
        strip_versions(read(&log)) == expected,
        format!("correct order: {:?}", read(&log)),
    )?;
    check(
        draft.get("amended_from") == &Value::Id(i.id()),
        "amended draft links the original",
    )?;
    check(
        draft.state() == Some(DocState::Draft),
        "amendment is a draft",
    )?;
    check(
        h.stored("invoice", i.id()).and_then(|d| d.state()) == Some(DocState::Cancelled),
        "original cancelled",
    )?;
    check(
        correct_sequence()
            .iter()
            .filter(|(p, s)| *p == CorrectPart::Both && *s == Stage::Commit)
            .count()
            == 1,
        "one commit",
    )
}

async fn before_derives_own_document(h: &dyn TestHost) -> CaseResult {
    h.register(Arc::new(Probe::new("customer").step(
        "probe:limit",
        HookPoint::of(Phase::Before, Target::Save),
        0,
        step_fn(|ctx| {
            Box::pin(async move {
                ctx.doc_mut()?.set(n("credit_limit"), dec("500.00"));
                Ok(())
            })
        }),
    )));
    let c = new_customer(h, "Acme").await?;
    check(
        h.stored("customer", c.id())
            .map(|d| d.get("credit_limit").clone())
            == Some(dec("500.00")),
        "derived value saved",
    )
}

async fn validate_may_not_change_or_write(h: &dyn TestHost) -> CaseResult {
    h.register(Arc::new(Probe::new("customer").step(
        "probe:bad",
        HookPoint::of(Phase::Validate, Target::Save),
        0,
        step_fn(|ctx| {
            Box::pin(async move {
                ctx.doc_mut()?.set(n("credit_limit"), dec("1.00"));
                Ok(())
            })
        }),
    )));
    let mut d = Document::new(n("customer"), h.fresh_id());
    d.set(n("customer_name"), text("X"));
    let id = d.id();
    expect_err(h.create(&inv(), d).await, "platform.phase_violation")?;
    check(h.stored("customer", id).is_none(), "nothing stored")
}

async fn validate_may_not_write_other_documents(h: &dyn TestHost) -> CaseResult {
    let q = ids(h, 1);
    h.register(Arc::new(Probe::new("customer").step(
        "probe:bad",
        HookPoint::of(Phase::Validate, Target::Op(Operation::Create)),
        0,
        step_fn(move |ctx| {
            let q = q.clone();
            Box::pin(async move {
                let note = Document::new(n("note"), take(&q)?);
                ctx.create(note).await.map(|_| ())
            })
        }),
    )));
    let mut d = Document::new(n("customer"), h.fresh_id());
    d.set(n("customer_name"), text("X"));
    expect_err(h.create(&inv(), d).await, "platform.phase_violation").map(|_| ())
}

async fn before_may_not_write_other_documents(h: &dyn TestHost) -> CaseResult {
    let q = ids(h, 1);
    h.register(Arc::new(Probe::new("customer").step(
        "probe:bad",
        HookPoint::of(Phase::Before, Target::Op(Operation::Create)),
        0,
        step_fn(move |ctx| {
            let q = q.clone();
            Box::pin(async move {
                let mut note = Document::new(n("note"), take(&q)?);
                note.set(n("text"), text("x"));
                ctx.create(note).await.map(|_| ())
            })
        }),
    )));
    let mut d = Document::new(n("customer"), h.fresh_id());
    d.set(n("customer_name"), text("X"));
    expect_err(h.create(&inv(), d).await, "platform.phase_violation").map(|_| ())
}

async fn after_writes_others_and_everything_rolls_back(h: &dyn TestHost) -> CaseResult {
    let q = ids(h, 2);
    let (fail_note, ok_note) = {
        let v = q.lock().unwrap_or_else(|p| p.into_inner());
        (v[0], v[1])
    };
    h.register(Arc::new(
        Probe::new("customer")
            .step(
                "probe:make_note",
                HookPoint::of(Phase::After, Target::Op(Operation::Create)),
                0,
                step_fn(move |ctx| {
                    let q = q.clone();
                    Box::pin(async move {
                        let mut note = Document::new(n("note"), take(&q)?);
                        note.set(n("text"), text("created by after_create"));
                        ctx.create(note).await?;
                        ctx.enqueue_job(JobRequest {
                            job: "probe:follow_up".into(),
                            args: json!({}),
                            idempotency_key: None,
                            not_before: None,
                        })
                    })
                }),
            )
            .step(
                "probe:fail",
                HookPoint::of(Phase::After, Target::Save),
                0,
                step_fn(|ctx| {
                    Box::pin(async move {
                        match ctx.doc().text("customer_name")? {
                            Some("Fail") => Err(AppError::new(
                                ErrorClass::Validation,
                                "probe.failed",
                                "failing on purpose",
                            )),
                            _ => Ok(()),
                        }
                    })
                }),
            ),
    ));
    new_customer(h, "Fine").await?;
    check(
        h.stored("note", ok_note).is_some(),
        "after_create wrote the other document",
    )?;
    check(h.jobs().len() == 1, "job delivered after commit")?;
    let events_before = h.outbox().len();
    let mut d = Document::new(n("customer"), h.fresh_id());
    d.set(n("customer_name"), text("Fail"));
    let id = d.id();
    expect_err(h.create(&inv(), d).await, "probe.failed")?;
    check(
        h.stored("customer", id).is_none(),
        "failed document not stored",
    )?;
    check(
        h.stored("note", fail_note).is_none(),
        "the other document written by after_create rolled back too",
    )?;
    check(
        h.jobs().len() == 1,
        "no job from the rolled-back transaction",
    )?;
    check(
        h.outbox().len() == events_before,
        "no events from the rolled-back transaction",
    )
}

async fn after_may_not_change_own_document(h: &dyn TestHost) -> CaseResult {
    h.register(Arc::new(
        Probe::new("customer")
            .step(
                "probe:mut",
                HookPoint::of(Phase::After, Target::Op(Operation::Create)),
                0,
                step_fn(|ctx| {
                    Box::pin(async move {
                        if ctx.doc().text("customer_name")? == Some("Mut") {
                            ctx.doc_mut()?;
                        }
                        Ok(())
                    })
                }),
            )
            .step(
                "probe:self_write",
                HookPoint::of(Phase::After, Target::Op(Operation::Create)),
                1,
                step_fn(|ctx| {
                    Box::pin(async move {
                        if ctx.doc().text("customer_name")? == Some("Self") {
                            let (id, v) = (ctx.doc().id(), version(ctx.doc()));
                            ctx.update("customer", id, DocumentPatch::default(), v)
                                .await?;
                        }
                        Ok(())
                    })
                }),
            ),
    ));
    for who in ["Mut", "Self"] {
        let mut d = Document::new(n("customer"), h.fresh_id());
        d.set(n("customer_name"), text(who));
        expect_err(h.create(&inv(), d).await, "platform.phase_violation")?;
    }
    Ok(())
}

async fn external_calls_only_after_commit(h: &dyn TestHost) -> CaseResult {
    let log: Log = Arc::default();
    let mut p = Probe::new("customer");
    for (i, point) in all_points().into_iter().enumerate() {
        let log = log.clone();
        p = p.step(
            &format!("probe:ext_{i}"),
            point.clone(),
            0,
            step_fn(move |ctx| {
                let allowed = ctx.external_allowed().is_ok();
                push(&log, format!("{}={allowed}", ctx.point().name()));
                Box::pin(async { Ok(()) })
            }),
        );
    }
    h.register(Arc::new(p));
    new_customer(h, "Acme").await?;
    for entry in read(&log) {
        let committed = entry.contains("_committed=");
        check(
            entry.ends_with("=true") == committed,
            format!("external allowed only in *_committed: {entry}"),
        )?;
    }
    Ok(())
}

async fn committed_failure_does_not_roll_back(h: &dyn TestHost) -> CaseResult {
    let note_id = h.fresh_id();
    h.register(Arc::new(
        Probe::new("customer")
            .step(
                "probe:boom",
                HookPoint::of(Phase::Committed, Target::Op(Operation::Create)),
                0,
                step_fn(|_ctx| {
                    Box::pin(async move {
                        Err(AppError::new(
                            ErrorClass::Unavailable,
                            "probe.smtp_down",
                            "mail server down",
                        ))
                    })
                }),
            )
            .step(
                "probe:notify_note",
                HookPoint::of(Phase::Committed, Target::Save),
                0,
                step_fn(move |ctx| {
                    Box::pin(async move {
                        let mut note = Document::new(n("note"), note_id);
                        note.set(n("text"), text("written after commit"));
                        ctx.create(note).await.map(|_| ())
                    })
                }),
            ),
    ));
    let c = new_customer(h, "Acme").await?;
    check(
        h.stored("customer", c.id()).is_some(),
        "document committed despite failing committed step",
    )?;
    check(
        h.committed_failures()
            .iter()
            .any(|(p, e)| p == "create_committed" && e.code == "probe.smtp_down"),
        "failure recorded for retry",
    )?;
    check(
        h.stored("note", note_id).is_some(),
        "committed step wrote in its own transaction",
    )
}

async fn jobs_only_from_phase_3_and_4(h: &dyn TestHost) -> CaseResult {
    h.register(Arc::new(Probe::new("customer").step(
        "probe:job",
        HookPoint::of(Phase::Validate, Target::Save),
        0,
        step_fn(|ctx| {
            Box::pin(async move {
                ctx.enqueue_job(JobRequest {
                    job: "probe:x".into(),
                    args: json!({}),
                    idempotency_key: None,
                    not_before: None,
                })
            })
        }),
    )));
    let mut d = Document::new(n("customer"), h.fresh_id());
    d.set(n("customer_name"), text("X"));
    expect_err(h.create(&inv(), d).await, "platform.phase_violation")?;
    check(h.jobs().is_empty(), "no job delivered")
}

async fn stale_version_is_refused(h: &dyn TestHost) -> CaseResult {
    let c = new_customer(h, "Acme").await?;
    let patch = DocumentPatch {
        fields: [(n("credit_limit"), dec("1.00"))].into(),
        ..Default::default()
    };
    h.update(&inv(), "customer", c.id(), patch.clone(), version(&c))
        .await?;
    let e = expect_err(
        h.update(&inv(), "customer", c.id(), patch, version(&c))
            .await,
        "document.stale_version",
    )?;
    check(
        e.class == ErrorClass::StaleVersion && e.details.get("current_version") == Some(&json!(2)),
        format!("details: {:?}", e.details),
    )
}

async fn merge_patch_semantics(h: &dyn TestHost) -> CaseResult {
    let c = new_customer(h, "Acme").await?;
    let i = new_invoice(h, c.id(), &[("A", "1"), ("B", "2"), ("C", "3")]).await?;
    let rows = i.rows("items").to_vec();
    let (a, c_row) = (rows[0].id, rows[2].id);
    let chosen = h.fresh_id();
    let mut patch = DocumentPatch::default();
    patch.fields.insert(n("remarks"), text("urgent"));
    patch.tables.insert(
        n("items"),
        vec![
            RowPatch {
                id: Some(c_row),
                fields: BTreeMap::new(),
            },
            RowPatch {
                id: Some(a),
                fields: [(n("qty"), dec("5"))].into(),
            },
            RowPatch {
                id: None,
                fields: [(n("item"), text("D")), (n("qty"), dec("4"))].into(),
            },
            RowPatch {
                id: Some(chosen),
                fields: [(n("item"), text("E")), (n("qty"), dec("1"))].into(),
            },
        ],
    );
    let out = h
        .update(&inv(), "invoice", i.id(), patch, version(&i))
        .await?
        .value;
    let items: Vec<(String, Value)> = out
        .rows("items")
        .iter()
        .map(|r| {
            (
                match r.fields.get(&n("item")) {
                    Some(Value::Text(t)) => t.clone(),
                    _ => String::new(),
                },
                r.fields.get(&n("qty")).cloned().unwrap_or(Value::Null),
            )
        })
        .collect();
    check(
        items
            == vec![
                ("C".into(), dec("3")),
                ("A".into(), dec("5")),
                ("D".into(), dec("4")),
                ("E".into(), dec("1")),
            ],
        format!("rows: {items:?}"),
    )?;
    check(
        out.rows("items")[0].id == c_row
            && out.rows("items")[1].id == a
            && out.rows("items")[3].id == chosen,
        "kept and client-chosen ids",
    )?;
    check(
        out.get("remarks") == &text("urgent") && out.get("number") == &text("INV-1"),
        "given field set, absent field kept",
    )?;
    let mut clear = DocumentPatch::default();
    clear.fields.insert(n("remarks"), Value::Null);
    let out = h
        .update(&inv(), "invoice", i.id(), clear, version(&out))
        .await?
        .value;
    check(
        out.get("remarks") == &Value::Null && out.rows("items").len() == 4,
        "null clears; untouched table kept",
    )?;
    let mut dup = DocumentPatch::default();
    dup.tables.insert(
        n("items"),
        vec![
            RowPatch {
                id: Some(a),
                fields: BTreeMap::new(),
            },
            RowPatch {
                id: Some(a),
                fields: BTreeMap::new(),
            },
        ],
    );
    expect_err(
        h.update(&inv(), "invoice", i.id(), dup, version(&out))
            .await,
        "document.validation_failed",
    )
    .map(|_| ())
}

async fn typed_field_errors_with_paths(h: &dyn TestHost) -> CaseResult {
    let c = new_customer(h, "Acme").await?;
    let mut d = Document::new(n("invoice"), h.fresh_id());
    d.set(n("customer"), Value::Id(c.id()));
    d.set(n("total"), text("not a number"));
    d.set(n("colour"), text("red"));
    let rows = d.rows_mut(n("items"));
    rows.push(crate::document::ChildRow {
        id: h.fresh_id(),
        fields: [(n("item"), text("A")), (n("qty"), dec("1"))].into(),
    });
    rows.push(crate::document::ChildRow {
        id: h.fresh_id(),
        fields: [(n("item"), text("B"))].into(),
    });
    let e = expect_err(h.create(&inv(), d).await, "document.validation_failed")?;
    let got: Vec<(String, String)> = e
        .fields
        .iter()
        .map(|f| (f.field.to_string(), f.code.clone()))
        .collect();
    for want in [
        ("total", "document.invalid_value"),
        ("colour", "document.unknown_field"),
        ("items[2].qty", "document.required"),
    ] {
        check(
            got.iter().any(|(p, c)| p == want.0 && c == want.1),
            format!("missing {want:?} in {got:?}"),
        )?;
    }
    check(
        e.api_code() == "document.validation_failed" && !e.retryable(),
        "reported as a validation failure",
    )?;
    let mut bad_link = Document::new(n("invoice"), h.fresh_id());
    bad_link.set(n("customer"), Value::Id(h.fresh_id()));
    let e = expect_err(
        h.create(&inv(), bad_link).await,
        "document.validation_failed",
    )?;
    check(
        e.fields
            .iter()
            .any(|f| f.field.as_str() == "customer" && f.code == "document.invalid_link"),
        "invalid link reported",
    )
}

async fn state_rules(h: &dyn TestHost) -> CaseResult {
    let log: Log = Arc::default();
    h.register(Arc::new(recorder("invoice", &log)));
    let c = new_customer(h, "Acme").await?;
    let i = new_invoice(h, c.id(), &[("A", "1")]).await?;
    expect_err(
        h.cancel(&inv(), "invoice", i.id(), version(&i)).await,
        "document.invalid_state",
    )?;
    let i = h.post(&inv(), "invoice", i.id(), version(&i)).await?.value;
    check(i.state() == Some(DocState::Posted), "posted")?;
    expect_err(
        h.post(&inv(), "invoice", i.id(), version(&i)).await,
        "document.invalid_state",
    )?;
    expect_err(
        h.delete(&inv(), "invoice", i.id(), version(&i)).await,
        "document.invalid_state",
    )?;
    let mut bad = DocumentPatch::default();
    bad.fields.insert(n("total"), dec("1.00"));
    bad.fields.insert(n("number"), text("X"));
    bad.fields.insert(n("remarks"), text("ok"));
    let e = expect_err(
        h.update(&inv(), "invoice", i.id(), bad, version(&i)).await,
        "document.validation_failed",
    )?;
    let fields: Vec<&str> = e.fields.iter().map(|f| f.field.as_str()).collect();
    check(
        fields.contains(&"total")
            && fields.contains(&"number")
            && !fields.contains(&"remarks")
            && e.fields
                .iter()
                .all(|f| f.code == "document.read_only_field"),
        format!("read-only fields: {fields:?}"),
    )?;
    log.lock().unwrap_or_else(|p| p.into_inner()).clear();
    let mut ok = DocumentPatch::default();
    ok.fields.insert(n("remarks"), text("ok"));
    let out = h
        .update(&inv(), "invoice", i.id(), ok, version(&i))
        .await?
        .value;
    check(
        strip_versions(read(&log)) == expected_names("invoice", Operation::UpdatePosted),
        format!("update_posted order: {:?}", read(&log)),
    )?;
    check(out.state() == Some(DocState::Posted), "still posted")
}

async fn delete_refused_while_linked(h: &dyn TestHost) -> CaseResult {
    let c = new_customer(h, "Acme").await?;
    new_invoice(h, c.id(), &[]).await?;
    let e = expect_err(
        h.delete(&inv(), "customer", c.id(), version(&c)).await,
        "document.linked",
    )?;
    check(
        e.class == ErrorClass::Linked && h.stored("customer", c.id()).is_some(),
        "customer kept",
    )
}

async fn lifecycle_events(h: &dyn TestHost) -> CaseResult {
    let c = new_customer(h, "Acme").await?;
    let patch = DocumentPatch {
        fields: [(n("credit_limit"), dec("9.00"))].into(),
        ..Default::default()
    };
    h.update(&inv(), "customer", c.id(), patch, version(&c))
        .await?;
    for (topic, op, ver) in [
        ("doc_created", Operation::Create, 1),
        ("doc_updated", Operation::Update, 2),
    ] {
        let evs = events_of(h, topic);
        check(
            evs.len() == 1,
            format!("one {topic} event, got {}", evs.len()),
        )?;
        let e = &evs[0];
        check(
            e.subject.id == c.id()
                && e.subject.entity.as_str() == "customer"
                && e.subject_version == Some(Version(ver)),
            format!("{topic} subject"),
        )?;
        let p = PayloadDocument::from_outbox_event(e).map_err(|x| Failure(x.to_string()))?;
        check(
            p.meta.command_id.as_ref().map(|c| c.as_str()) == Some("cmd-1"),
            "command id carried",
        )?;
        op.schema()
            .validate(&p.data)
            .map_err(|x| Failure(format!("{topic} data: {x:?}")))?;
        if op == Operation::Update {
            check(
                p.data["changed_fields"] == json!(["credit_limit"]),
                format!("changed_fields: {}", p.data),
            )?;
        }
    }
    let c2 = h
        .stored("customer", c.id())
        .ok_or_else(|| Failure("missing".into()))?;
    h.rename(&inv(), "customer", c.id(), "Acme Ltd", version(&c2))
        .await?;
    let p = PayloadDocument::from_outbox_event(&events_of(h, "doc_renamed")[0])
        .map_err(|x| Failure(x.to_string()))?;
    check(
        p.data == json!({"old_name": "Acme", "new_name": "Acme Ltd"}),
        format!("rename data: {}", p.data),
    )
}

async fn messages_are_collected(h: &dyn TestHost) -> CaseResult {
    h.register(Arc::new(
        Probe::new("customer")
            .step(
                "probe:m1",
                HookPoint::of(Phase::Validate, Target::Save),
                0,
                step_fn(|ctx| {
                    Box::pin(async move {
                        ctx.message(Message::warning("probe.first", "first"));
                        Ok(())
                    })
                }),
            )
            .step(
                "probe:m2",
                HookPoint::of(Phase::Committed, Target::Save),
                0,
                step_fn(|ctx| {
                    Box::pin(async move {
                        ctx.message(Message::info("probe.second", "second"));
                        Ok(())
                    })
                }),
            ),
    ));
    let mut d = Document::new(n("customer"), h.fresh_id());
    d.set(n("customer_name"), text("A"));
    let out: Outcome<Document> = h.create(&inv(), d).await?;
    let codes: Vec<&str> = out.messages.iter().map(|m| m.code.as_str()).collect();
    check(
        codes == ["probe.first", "probe.second"],
        format!("messages: {codes:?}"),
    )
}

async fn priority_orders_steps_across_controllers(h: &dyn TestHost) -> CaseResult {
    let log: Log = Arc::default();
    let at = HookPoint::of(Phase::Before, Target::Save);
    for (app, prio) in [("zeta", 100), ("alpha", 200), ("beta", 100)] {
        let log = log.clone();
        let label = format!("{app}:{prio}");
        h.register(Arc::new(Probe::new("customer").step(
            &format!("{app}:step"),
            at.clone(),
            prio,
            step_fn(move |_ctx| {
                push(&log, label.clone());
                Box::pin(async { Ok(()) })
            }),
        )));
    }
    new_customer(h, "A").await?;
    check(
        read(&log) == ["beta:100", "zeta:100", "alpha:200"],
        format!("order: {:?}", read(&log)),
    )
}

async fn overrides(h: &dyn TestHost) -> CaseResult {
    let log: Log = Arc::default();
    let at = HookPoint::of(Phase::Before, Target::Save);
    let mut p = Probe::new("customer");
    for s in [
        "t_disable",
        "t_replace",
        "t_around",
        "t_wrap",
        "by_replace",
        "by_before",
        "by_after",
        "by_wrap",
    ] {
        let log = log.clone();
        let is_target = s.starts_with("t_");
        let label = s.to_owned();
        let wraps = s == "by_wrap";
        // Targets run at the hook point; `by_*` steps are declared on a point that never fires
        // for this test (load), so they run only through overrides.
        let point = if is_target {
            at.clone()
        } else {
            HookPoint::Load
        };
        p = p.step(
            &format!("app:{s}"),
            point,
            0,
            step_fn(move |ctx| {
                let log = log.clone();
                let label = label.clone();
                Box::pin(async move {
                    if wraps {
                        push(&log, "wrap_in".into());
                        ctx.run_wrapped().await?;
                        push(&log, "wrap_out".into());
                    } else {
                        push(&log, label);
                    }
                    Ok(())
                })
            }),
        );
    }
    h.register(Arc::new(p));
    let sn = |s: &str| StepName::new(&format!("app:{s}")).unwrap_or_else(|e| panic!("{e}"));
    for (target, kind, by) in [
        ("t_disable", OverrideKind::Disable, None),
        ("t_replace", OverrideKind::Replace, Some("by_replace")),
        ("t_around", OverrideKind::Before, Some("by_before")),
        ("t_around", OverrideKind::After, Some("by_after")),
        ("t_wrap", OverrideKind::Wrap, Some("by_wrap")),
    ] {
        h.add_override(StepOverride {
            target: sn(target),
            kind,
            by: by.map(sn),
            reason: "contract test".into(),
        });
    }
    new_customer(h, "A").await?;
    // Targets sort by name: t_around, t_disable, t_replace, t_wrap.
    let want = [
        "by_before",
        "t_around",
        "by_after",
        "by_replace",
        "wrap_in",
        "t_wrap",
        "wrap_out",
    ];
    check(
        read(&log) == want,
        format!("override order: {:?}", read(&log)),
    )
}

async fn default_controller_for_runtime_doctype(h: &dyn TestHost) -> CaseResult {
    h.define(DocTypeDef {
        name: n("visit_log"),
        label: "Visit Log".into(),
        is_posted: false,
        is_child: false,
        title_field: None,
        fields: vec![field(
            "visitor",
            FieldKind::Text {
                max_chars: None,
                format: TextFormat::Plain,
            },
            true,
        )],
        protection_groups: Vec::new(),
        version: 1,
    });
    let mut d = Document::new(n("visit_log"), h.fresh_id());
    d.set(n("visitor"), text("Sara"));
    let out = h.create(&inv(), d).await?.value;
    check(
        out.version() == Some(Version::FIRST) && h.stored("visit_log", out.id()).is_some(),
        "saved through the default controller",
    )
}

async fn whitelisted_methods(h: &dyn TestHost) -> CaseResult {
    let q = ids(h, 1);
    let note_id = q.lock().unwrap_or_else(|p| p.into_inner())[0];
    h.register(Arc::new(Probe::new("customer").method(
        "add_note",
        true,
        method_fn(move |args, ctx| {
            let q = q.clone();
            Box::pin(async move {
                let mut note = Document::new(n("note"), take(&q)?);
                note.set(
                    n("text"),
                    Value::Text(args["text"].as_str().unwrap_or("").to_owned()),
                );
                ctx.create(note).await?;
                let (id, v) = (ctx.doc().id(), version(ctx.doc()));
                let patch = DocumentPatch {
                    fields: [(n("credit_limit"), dec("7.00"))].into(),
                    ..Default::default()
                };
                ctx.update("customer", id, patch, v).await?;
                Ok(json!({"noted": true}))
            })
        }),
    )));
    let c = new_customer(h, "Acme").await?;
    let out = h
        .call_method(
            &inv(),
            "customer",
            c.id(),
            "add_note",
            json!({"text": "call back"}),
            Some(version(&c)),
        )
        .await?
        .value;
    check(out.1 == json!({"noted": true}), "method result")?;
    check(
        out.0.get("credit_limit") == &dec("7.00") && out.0.version() == Some(Version(2)),
        "document returned as it is after the method",
    )?;
    check(
        h.stored("note", note_id).is_some(),
        "method wrote another document",
    )?;
    expect_err(
        h.call_method(&inv(), "customer", c.id(), "nope", json!({}), None)
            .await,
        "method.not_found",
    )
    .map(|_| ())
}

async fn form_and_load_events(h: &dyn TestHost) -> CaseResult {
    h.register(Arc::new(
        Probe::new("customer")
            .step(
                "probe:fc",
                HookPoint::FieldChanged("customer_name".into()),
                0,
                step_fn(|ctx| {
                    Box::pin(async move {
                        ctx.doc_mut()?.set(n("credit_limit"), dec("100.00"));
                        Ok(())
                    })
                }),
            )
            .step(
                "probe:load",
                HookPoint::Load,
                0,
                step_fn(|ctx| {
                    Box::pin(async move {
                        if ctx.doc_mut().is_ok() {
                            return Err(AppError::new(
                                ErrorClass::Internal,
                                "probe.load_mutable",
                                "load must be read-only",
                            ));
                        }
                        ctx.message(Message::info("probe.loaded", "loaded"));
                        Ok(())
                    })
                }),
            ),
    ));
    let mut draft = Document::new(n("customer"), h.fresh_id());
    draft.set(n("customer_name"), text("A"));
    let id = draft.id();
    let out = h.field_changed(&inv(), draft, "customer_name").await?.value;
    check(
        out.get("credit_limit") == &dec("100.00") && h.stored("customer", id).is_none(),
        "draft adjusted, nothing stored",
    )?;
    let c = new_customer(h, "B").await?;
    let loaded = h.load(&inv(), "customer", c.id()).await?;
    check(
        loaded.messages.iter().any(|m| m.code == "probe.loaded"),
        "load step ran",
    )
}

struct Doubler;
impl SeamProvider for Doubler {
    fn call(&self, input: Json) -> BoxFuture<'_, AppResult<Json>> {
        Box::pin(async move { Ok(json!({"out": input["x"].as_i64().unwrap_or(0) * 2})) })
    }
}

/// A lookup provider: echoes a fixed page; the round-trip test asserts the wire shapes.
struct LookupSeam {
    page: Mutex<Option<LookupPageWire>>,
    seen: Mutex<Option<Json>>,
}

impl LookupSeam {
    fn answering(page: LookupPageWire) -> Self {
        Self {
            page: Mutex::new(Some(page)),
            seen: Mutex::new(None),
        }
    }
}

impl SeamProvider for LookupSeam {
    fn call(&self, input: Json) -> BoxFuture<'_, AppResult<Json>> {
        *lock(&self.seen) = Some(input);
        Box::pin(async move {
            let page = lock(&self.page).take().ok_or_else(|| {
                AppError::new(
                    ErrorClass::Unavailable,
                    "lookup.seam_failed",
                    "the lookup seam has no page left",
                )
            })?;
            serde_json::to_value(page).map_err(|e| internal_json("lookup.page_encoding", &e))
        })
    }
}

fn internal_json(code: &str, e: &serde_json::Error) -> AppError {
    AppError::new(ErrorClass::Internal, code, e.to_string())
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

// ------------------------------------------------------------------------------------------
// The v0.2 field model (ADR-068 §6): listed select values, dynamic links, secrets and lookups
// ------------------------------------------------------------------------------------------

/// The DocType the field-model cases run against, built on top of the fixture catalog.
fn field_kinds_catalog() -> Vec<DocTypeDef> {
    let field = |name: &str, kind: FieldKind| FieldDef::new(n(name), kind, false, false);
    vec![
        DocTypeDef::new(
            n("pick_list"),
            "Pick List".into(),
            false,
            false,
            Some(n("title")),
            vec![
                field(
                    "title",
                    FieldKind::Text {
                        max_chars: None,
                        format: TextFormat::Plain,
                    },
                ),
                field(
                    "stage",
                    FieldKind::Select {
                        values: ["Open", "Paid", "Overdue"]
                            .iter()
                            .map(|s| s.to_string())
                            .collect(),
                    },
                ),
                field(
                    "long_notes",
                    FieldKind::LongText {
                        format: LongTextFormat::Plain,
                    },
                ),
                field(
                    "target",
                    FieldKind::DynamicLink {
                        type_field: n("target_type"),
                    },
                ),
                field(
                    "target_type",
                    FieldKind::Text {
                        max_chars: None,
                        format: TextFormat::Plain,
                    },
                ),
                field(
                    "attachment",
                    FieldKind::Attach {
                        format: AttachFormat::File,
                    },
                ),
                field("credentials", FieldKind::Secret),
                field(
                    "extra",
                    FieldKind::Json {
                        format: JsonFormat::Plain,
                    },
                ),
            ],
        ),
        DocTypeDef::new(
            n("picks"),
            "Picks".into(),
            false,
            true,
            None,
            vec![field(
                "picked",
                FieldKind::Link {
                    doctype: n("pick_list"),
                },
            )],
        ),
        DocTypeDef::new(
            n("multi_doc"),
            "Multi Doc".into(),
            false,
            false,
            None,
            vec![field(
                "multi",
                FieldKind::TableMultiselect {
                    doctype: n("picks"),
                    link_field: n("parent_link"),
                },
            )],
        ),
    ]
}

async fn select_values_are_enforced_on_write(h: &dyn TestHost) -> CaseResult {
    for def in field_kinds_catalog() {
        h.define(def);
    }
    let mut d = Document::new(n("pick_list"), h.fresh_id());
    d.set(n("title"), text("x"));
    d.set(n("stage"), text("Paid"));
    h.create(&inv(), d).await?;
    let mut d = Document::new(n("pick_list"), h.fresh_id());
    d.set(n("title"), text("x"));
    d.set(n("stage"), text("Cancelled"));
    let e = expect_err(h.create(&inv(), d).await, "document.validation_failed")?;
    check(
        e.fields
            .iter()
            .any(|f| f.field.as_str() == "stage" && f.code == "document.value_not_listed"),
        format!("value_not_listed: {:?}", e.fields),
    )
}

async fn dynamic_link_is_in_the_view_and_accepts_an_id(h: &dyn TestHost) -> CaseResult {
    for def in field_kinds_catalog() {
        h.define(def);
    }
    let def = h.meta("pick_list").await?;
    let kind = def
        .field("target")
        .map(|f| f.kind.clone())
        .ok_or_else(|| Failure("dynamic link missing from the view".into()))?;
    check(
        matches!(&kind, FieldKind::DynamicLink { type_field } if type_field.as_str() == "target_type"),
        format!("dynamic link kind: {kind:?}"),
    )?;
    check(kind.accepts(&Value::Id(h.fresh_id())), "accepts an Id")?;
    check(!kind.accepts(&text("x")), "refuses text")?;
    let mut d = Document::new(n("pick_list"), h.fresh_id());
    d.set(n("target"), Value::Id(h.fresh_id()));
    d.set(n("target_type"), text("note"));
    h.create(&inv(), d).await?;
    // The value does not fit any other kind's rules either.
    check(
        FieldKind::Link {
            doctype: n("customer"),
        }
        .accepts(&Value::Id(h.fresh_id()))
            && !FieldKind::Bool.accepts(&Value::Id(h.fresh_id())),
        "no cross-kind acceptance",
    )
}

async fn secret_refuses_a_value(h: &dyn TestHost) -> CaseResult {
    for def in field_kinds_catalog() {
        h.define(def);
    }
    let mut d = Document::new(n("pick_list"), h.fresh_id());
    d.set(n("title"), text("x"));
    d.set(n("credentials"), text("hunter2"));
    let e = expect_err(h.create(&inv(), d).await, "document.validation_failed")?;
    check(
        e.fields
            .iter()
            .any(|f| f.field.as_str() == "credentials" && f.code == "document.secret_not_writable"),
        format!("secret refused: {:?}", e.fields),
    )
}

async fn field_kinds_accept_their_values(h: &dyn TestHost) -> CaseResult {
    let id = h.fresh_id();
    let now = OffsetDateTime::UNIX_EPOCH;
    let cases: [(FieldKind, Value, bool); 45] = [
        (FieldKind::Bool, Value::Bool(true), true),
        (FieldKind::Bool, Value::Int(1), false),
        (FieldKind::Bool, Value::Null, true),
        (
            FieldKind::Int {
                format: crate::meta::IntFormat::Plain,
            },
            Value::Int(3),
            true,
        ),
        (
            FieldKind::Int {
                format: crate::meta::IntFormat::Rating { max: 5 },
            },
            Value::Int(3),
            true,
        ),
        (
            FieldKind::Int {
                format: crate::meta::IntFormat::Plain,
            },
            Value::Decimal(d("1")),
            false,
        ),
        (
            FieldKind::Decimal {
                scale: 2,
                precision: 18,
                format: DecimalFormat::Currency {
                    currency_field: n("currency"),
                },
            },
            Value::Decimal(d("1.00")),
            true,
        ),
        (
            FieldKind::Decimal {
                scale: 2,
                precision: 18,
                format: DecimalFormat::Percent,
            },
            Value::Int(1),
            false,
        ),
        (
            FieldKind::Text {
                max_chars: None,
                format: TextFormat::Email,
            },
            text("a@b.co"),
            true,
        ),
        (
            FieldKind::Text {
                max_chars: None,
                format: TextFormat::Barcode { symbology: None },
            },
            Value::Int(1),
            false,
        ),
        (
            FieldKind::LongText {
                format: LongTextFormat::Code {
                    syntax: "sql".into(),
                },
            },
            text("select 1"),
            true,
        ),
        (
            FieldKind::LongText {
                format: LongTextFormat::Markdown,
            },
            Value::Int(1),
            false,
        ),
        (
            FieldKind::Select {
                values: vec!["A".into()],
            },
            text("A"),
            true,
        ),
        (
            FieldKind::Select {
                values: vec!["A".into()],
            },
            Value::Date(
                time::Date::from_calendar_date(2026, time::Month::January, 1)
                    .unwrap_or_else(|e| panic!("{e}")),
            ),
            false,
        ),
        (
            FieldKind::Date,
            Value::Date(
                time::Date::from_calendar_date(2026, time::Month::January, 1)
                    .unwrap_or_else(|e| panic!("{e}")),
            ),
            true,
        ),
        (FieldKind::Time, Value::Time(time::Time::MIDNIGHT), true),
        (FieldKind::DateTime, Value::DateTime(now), true),
        (FieldKind::Date, Value::DateTime(now), false),
        (FieldKind::Link { doctype: n("x") }, Value::Id(id), true),
        (FieldKind::Link { doctype: n("x") }, text("x"), false),
        (
            FieldKind::DynamicLink { type_field: n("t") },
            Value::Id(id),
            true,
        ),
        (
            FieldKind::DynamicLink { type_field: n("t") },
            text("x"),
            false,
        ),
        (
            FieldKind::Json {
                format: JsonFormat::Geolocation,
            },
            Value::Json("{}".into()),
            true,
        ),
        (
            FieldKind::Json {
                format: JsonFormat::Plain,
            },
            text("{}"),
            false,
        ),
        (FieldKind::Bytes, Value::Bytes(vec![1]), true),
        (FieldKind::Bytes, text("x"), false),
        (
            FieldKind::Attach {
                format: AttachFormat::Image,
            },
            Value::Id(id),
            true,
        ),
        (
            FieldKind::Attach {
                format: AttachFormat::Signature,
            },
            Value::Bytes(vec![]),
            false,
        ),
        (FieldKind::Secret, Value::Null, true),
        (FieldKind::Secret, text("s"), false),
        (FieldKind::Secret, Value::Id(id), false),
        (
            FieldKind::Table { doctype: n("t") },
            Value::Json("[]".into()),
            false,
        ),
        (FieldKind::Table { doctype: n("t") }, Value::Null, false),
        (
            FieldKind::TableMultiselect {
                doctype: n("t"),
                link_field: n("p"),
            },
            Value::Null,
            false,
        ),
        (
            FieldKind::TableMultiselect {
                doctype: n("t"),
                link_field: n("p"),
            },
            Value::Id(id),
            false,
        ),
        // Non-matching pairs not covered above.
        (FieldKind::Bool, text("true"), false),
        (FieldKind::Bool, Value::Null, true),
        (FieldKind::Date, Value::Null, true),
        (FieldKind::Time, Value::Null, true),
        (FieldKind::DateTime, Value::Null, true),
        (FieldKind::Link { doctype: n("x") }, Value::Null, true),
        (
            FieldKind::Json {
                format: JsonFormat::Plain,
            },
            Value::Null,
            true,
        ),
        (FieldKind::Bytes, Value::Null, true),
        (FieldKind::Secret, Value::Null, true),
        (FieldKind::Bool, Value::Bool(false), true),
    ];
    for (i, (kind, v, want)) in cases.iter().enumerate() {
        check(
            &kind.accepts(v) == want,
            format!("case {i}: {kind:?}.accepts({v:?}) != {want}"),
        )?;
    }
    Ok(())
}

async fn lookup_round_trips_through_seams(h: &dyn TestHost) -> CaseResult {
    for def in field_kinds_catalog() {
        h.define(def);
    }
    const LOOKUP: &str = "probe:lookup";
    let seen: Arc<Mutex<Option<Json>>> = Arc::default();
    let answer = Arc::new(LookupSeam::answering(LookupPageWire {
        rows: vec![LookupRowWire {
            id: "0192f3c4-5b6a-7c8d-9e0f-1a2b3c4d5e6f".to_owned(),
            label: "Acme Ltd".into(),
            description: Some("a customer".into()),
            extras: BTreeMap::from([("credit_limit".to_owned(), json!("500.00"))]),
        }],
        has_more: true,
        cursor: Some("next".into()),
    }));
    h.provide_seam(LOOKUP, 1, answer);
    let seen2 = seen.clone();
    h.register(Arc::new(Probe::new("pick_list").step(
        "probe:lookup_step",
        HookPoint::of(Phase::Before, Target::Save),
        0,
        step_fn(move |ctx| {
            let seen = seen2.clone();
            Box::pin(async move {
                let request = LookupRequest {
                    doctype: ctx.doc().doctype().clone(),
                    field: n("stage"),
                    text: "Ac".into(),
                    cursor: None,
                    doc: Some(json!({"title": "x"})),
                    parent: Some(json!({"parent_of": "child"})),
                    params: BTreeMap::from([("only_active".to_owned(), "true".to_owned())]),
                };
                let out = ctx
                    .seams()
                    .call_json(LOOKUP, 1, request_wire(&request))
                    .await?;
                *seen.lock().unwrap_or_else(|p| p.into_inner()) = Some(request_wire(&request));
                let back = crate::lookup::page_from_wire(&out)
                    .map_err(|e| AppError::new(ErrorClass::Internal, "probe.lookup", e))?;
                let row = back.rows.first().ok_or_else(|| {
                    AppError::new(
                        ErrorClass::Internal,
                        "probe.lookup",
                        "the seam answered no row",
                    )
                })?;
                // The wire round trip is lossless for id, label, description, extras,
                // has_more and the cursor.
                let extra = row
                    .extras
                    .get("credit_limit")
                    .and_then(|j| j.as_str())
                    .unwrap_or("");
                let shape = format!(
                    "{}|{}|{}|{}|{has_more}|{cursor}",
                    row.id,
                    row.label,
                    row.description.clone().unwrap_or_default(),
                    extra,
                    has_more = back.has_more,
                    cursor = back.cursor.clone().unwrap_or_default(),
                );
                let want = format!("{}|Acme Ltd|a customer|500.00|true|next", row.id);
                if shape != want {
                    return Err(AppError::new(
                        ErrorClass::Internal,
                        "probe.lookup",
                        format!("the wire page lost data: `{shape}` != `{want}`"),
                    ));
                }
                // A request that names nothing usable is refused, not guessed.
                let bad = json!({"doctype": "not a name", "field": "stage", "text": ""});
                if request_from_wire(&bad).is_ok() {
                    return Err(AppError::new(
                        ErrorClass::Internal,
                        "probe.lookup",
                        "an invalid wire name parsed",
                    ));
                }
                Ok(())
            })
        }),
    )));
    let mut d = Document::new(n("pick_list"), h.fresh_id());
    d.set(n("title"), text("x"));
    h.create(&inv(), d).await?;
    let sent = seen
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone()
        .ok_or_else(|| Failure("the lookup step never ran".into()))?;
    check(
        sent["doctype"] == json!("pick_list")
            && sent["field"] == json!("stage")
            && sent["text"] == json!("Ac")
            && sent["params"] == json!({"only_active": "true"}),
        format!("the wire request: {sent}"),
    )
}

async fn seams_are_typed(h: &dyn TestHost) -> CaseResult {
    #[derive(serde::Serialize)]
    struct In {
        x: i64,
    }
    #[derive(serde::Deserialize)]
    struct Out {
        out: i64,
    }
    const DOUBLE: crate::Seam<In, Out> = crate::Seam::new("probe:double", 1);
    h.provide_seam("probe:double", 1, Arc::new(Doubler));
    let got: Arc<Mutex<i64>> = Arc::default();
    let g = got.clone();
    h.register(Arc::new(Probe::new("customer").step(
        "probe:seam",
        HookPoint::of(Phase::Validate, Target::Save),
        0,
        step_fn(move |ctx| {
            let g = g.clone();
            Box::pin(async move {
                let out = DOUBLE.call(ctx.seams(), &In { x: 21 }).await?;
                *g.lock().unwrap_or_else(|p| p.into_inner()) = out.out;
                let missing = ctx.seams().call_json("probe:missing", 1, json!({})).await;
                if missing.map_err(|e| e.class) != Err(ErrorClass::MethodNotFound) {
                    return Err(AppError::new(
                        ErrorClass::Internal,
                        "probe.seam",
                        "missing seam must be method.not_found",
                    ));
                }
                Ok(())
            })
        }),
    )));
    new_customer(h, "A").await?;
    check(
        *got.lock().unwrap_or_else(|p| p.into_inner()) == 42,
        "seam result",
    )
}

// -------------------------------------------------------------------------------------------
// Security declarations (ADR-068 amendment A1, ADR-070 section 12)
// -------------------------------------------------------------------------------------------

/// A pay-roll-like DocType with three protected fields and three declared groups, so each
/// security case can start from a realistic definition and reshape it.
fn salary_def() -> DocTypeDef {
    let mut def = DocTypeDef::new(
        n("salary"),
        "Salary".into(),
        false,
        false,
        Some(n("employee")),
        vec![
            field(
                "employee",
                FieldKind::Text {
                    max_chars: Some(140),
                    format: TextFormat::Plain,
                },
                true,
            ),
            field(
                "net_pay",
                FieldKind::Decimal {
                    scale: 2,
                    precision: 18,
                    format: DecimalFormat::Plain,
                },
                false,
            ),
            field(
                "bank_account",
                FieldKind::Text {
                    max_chars: Some(64),
                    format: TextFormat::Plain,
                },
                false,
            ),
            field(
                "remarks",
                FieldKind::LongText {
                    format: LongTextFormat::Plain,
                },
                false,
            ),
        ],
    );
    def.protection_groups = vec![
        group(
            "compensation",
            "Compensation",
            false,
            SyncClassHint::Encrypted,
        ),
        group("bank_details", "Bank Details", true, SyncClassHint::Sealed),
        group("personal", "Personal", false, SyncClassHint::HomeOnly),
    ];
    def.version = 1;
    def
}

/// A declared protection group.
fn group(name: &str, label: &str, trace: bool, hint: SyncClassHint) -> ProtectionGroupDef {
    let mut g = ProtectionGroupDef::new(
        crate::meta::GroupName::new(name).unwrap_or_else(|e| panic!("{e}")),
        label.to_owned(),
    );
    g.trace = trace;
    g.sync_class_hint = hint;
    g
}

/// A field security block: the group, a mask, and one classification tag.
fn secured(group_name: &str, mask: Option<MaskFormat>, tag: &str) -> FieldSecurity {
    FieldSecurity {
        protection_group: Some(
            crate::meta::GroupName::new(group_name).unwrap_or_else(|e| panic!("{e}")),
        ),
        classification: vec![Tag::new(tag).unwrap_or_else(|e| panic!("{e}"))],
        masking: mask,
        declassified: None,
        // Never authored in production; zero here is the "unset" the contract allows a
        // definition to carry.
        inherited_groups: Vec::new(),
    }
}

/// Sets one field's security in place.
fn set_security(def: &mut DocTypeDef, field_name: &str, security: FieldSecurity) {
    let f = def
        .fields
        .iter_mut()
        .find(|f| f.name.as_str() == field_name)
        .unwrap_or_else(|| panic!("{field_name} is not a field of the definition"));
    f.security = security;
}

/// A definition with a select field the next three cases reshape.
fn tagged_def(multiple: bool, kind: Option<FieldKind>) -> DocTypeDef {
    let mut def = salary_def();
    let mut f = field(
        "benefits",
        kind.unwrap_or(FieldKind::Select {
            values: vec!["Medical".into(), "Transport".into(), "Housing".into()],
        }),
        false,
    );
    f.multiple = multiple;
    def.fields.push(f);
    def
}

/// A `multiple` select survives the host's `meta` view (ADR-068 A2, native API 0.5.0).
async fn multiple_select_round_trips(h: &dyn TestHost) -> CaseResult {
    let def = tagged_def(true, None);
    def.validate()
        .map_err(|e| Failure(format!("a multiple select is valid: {e}")))?;
    h.define(def);
    let view = h.meta("salary").await?;
    let f = view
        .field("benefits")
        .ok_or_else(|| Failure("benefits missing from the view".into()))?;
    check(f.multiple, "multiple lost in the view")?;
    check(
        !view.field("employee").map(|f| f.multiple).unwrap_or(true),
        "a plain field became multiple",
    )
}

/// `multiple` on a field that is not a select is refused (`meta.multiple_not_select`).
async fn multiple_on_a_non_select_is_refused(_h: &dyn TestHost) -> CaseResult {
    let text_kind = FieldKind::Text {
        max_chars: Some(20),
        format: TextFormat::Plain,
    };
    let e = refusal_of(
        tagged_def(true, Some(text_kind)).validate(),
        "multiple on text",
    )?;
    check(
        e.fields
            .iter()
            .any(|f| f.field.as_str() == "benefits" && f.code == "meta.multiple_not_select"),
        format!("multiple_not_select: {e:?}"),
    )
}

/// Each control hint's string form is the native API enum value; a field built with the 0.3.0
/// constructor is single-valued and carries no hint.
async fn control_hints_match_the_native_api(_h: &dyn TestHost) -> CaseResult {
    use crate::meta::ControlHint;
    let wire = [
        (ControlHint::Switch, "switch"),
        (ControlHint::Checkbox, "checkbox"),
        (ControlHint::Dropdown, "dropdown"),
        (ControlHint::Radio, "radio"),
        (ControlHint::Segmented, "segmented"),
        (ControlHint::Autocomplete, "autocomplete"),
        (ControlHint::Chips, "chips"),
        (ControlHint::Textarea, "textarea"),
    ];
    for (hint, text) in wire {
        check(
            hint.as_str() == text && ControlHint::parse(text) == Some(hint),
            format!("control hint {text}"),
        )?;
    }
    check(
        ControlHint::parse("slider").is_none(),
        "an unknown hint parses to None",
    )?;
    let plain = field(
        "plain",
        FieldKind::Text {
            max_chars: None,
            format: TextFormat::Plain,
        },
        false,
    );
    check(
        !plain.multiple && plain.hints.control.is_none(),
        "a default field is single-valued without a hint",
    )
}

/// A protected definition round-trips: the security survives the host's `meta` view bit for
/// bit (group, tags, mask, hint) — the UI and the engine read protection from the app view —
/// and the untagged fields stay unprotected in the same view.
async fn protected_field_round_trips(h: &dyn TestHost) -> CaseResult {
    let mut def = salary_def();
    set_security(
        &mut def,
        "net_pay",
        secured("compensation", Some(MaskFormat::LastChars(4)), "financial"),
    );
    set_security(
        &mut def,
        "bank_account",
        secured("bank_details", Some(MaskFormat::Placeholder), "secret"),
    );
    set_security(&mut def, "remarks", secured("personal", None, "pii"));
    def.validate()
        .map_err(|e| Failure(format!("valid definition refused: {e}")))?;
    h.define(def);

    let view = h.meta("salary").await?;
    let net_pay = view
        .field("net_pay")
        .ok_or_else(|| Failure("net_pay missing from the view".into()))?;
    let bank = view
        .field("bank_account")
        .ok_or_else(|| Failure("bank_account missing from the view".into()))?;
    let remarks = view
        .field("remarks")
        .ok_or_else(|| Failure("remarks missing from the view".into()))?;
    let group_ok = |f: &crate::meta::FieldDef, name: &str| {
        f.security
            .protection_group
            .as_ref()
            .map(|g| g.as_str() == name)
            .unwrap_or(false)
    };
    check(
        group_ok(net_pay, "compensation")
            && net_pay.security.masking == Some(MaskFormat::LastChars(4))
            && net_pay
                .security
                .classification
                .iter()
                .any(|t| t.as_str() == "financial"),
        format!("net_pay security: {:?}", net_pay.security),
    )?;
    check(
        group_ok(bank, "bank_details") && bank.security.masking == Some(MaskFormat::Placeholder),
        format!("bank_account security: {:?}", bank.security),
    )?;
    check(
        group_ok(remarks, "personal") && remarks.security.masking.is_none(),
        format!("remarks security: {:?}", remarks.security),
    )?;
    // The declared groups carry their trace flag and sync hint through too.
    let traced = view.protection_group("bank_details");
    check(
        traced.is_some_and(|g| g.trace && g.sync_class_hint == SyncClassHint::Sealed),
        format!("bank_details group: {traced:?}"),
    )?;
    check(
        view.protection_groups.len() == 3,
        "all three groups are declared",
    )?;
    // An unprotected definition is still valid, validates clean, and stays unprotected.
    let plain = DocTypeDef::new(
        n("note_public"),
        "Note".into(),
        false,
        false,
        None,
        vec![field(
            "text",
            FieldKind::Text {
                max_chars: None,
                format: TextFormat::Plain,
            },
            false,
        )],
    );
    plain
        .validate()
        .map_err(|e| Failure(format!("plain definition refused: {e}")))?;
    check(
        plain.fields.iter().all(|f| f.security.is_empty()),
        "a definition without security stays empty",
    )
}

/// A field naming a group the DocType does not declare is refused (`meta.unknown_group`): a
/// closed-by-default field whose group nobody declares would silently protect nothing. Pure
/// definition validation, so no host is needed; the signature keeps the suite's shape.
async fn undeclared_group_is_refused(_h: &dyn TestHost) -> CaseResult {
    let mut def = salary_def();
    def.protection_groups.clear();
    set_security(
        &mut def,
        "net_pay",
        secured("compensation", None, "financial"),
    );
    let e = refusal_of(def.validate(), "an undeclared group")?;
    check(
        e.code == "meta.validation_failed"
            && e.fields
                .iter()
                .any(|f| f.field.as_str() == "net_pay" && f.code == "meta.unknown_group"),
        format!("unknown_group: {e:?}"),
    )?;
    // Every violation comes out at once. net_pay's mask is malformed (`meta.mask_invalid`)
    // and makes `compensation` a masked group; bank_account joins that group with no mask at
    // all (`meta.mask_missing`); employee carries a declassification with no reason
    // (`meta.declassify_reason`). One `validate` call reports all three field errors.
    let mut def = salary_def();
    set_security(
        &mut def,
        "net_pay",
        secured("compensation", Some(MaskFormat::LastChars(0)), "financial"),
    );
    set_security(
        &mut def,
        "bank_account",
        secured("compensation", None, "pii"),
    );
    set_security(
        &mut def,
        "employee",
        FieldSecurity {
            declassified: Some("   ".to_owned()),
            ..FieldSecurity::default()
        },
    );
    let e = refusal_of(def.validate(), "every error at once")?;
    let codes: Vec<&str> = e.fields.iter().map(|f| f.code.as_str()).collect();
    check(
        e.code == "meta.validation_failed"
            && codes.contains(&"meta.mask_invalid")
            && codes.contains(&"meta.mask_missing")
            && codes.contains(&"meta.declassify_reason"),
        format!("every error at once: {codes:?}"),
    )?;
    // And a malformed mask is refused on its own, with the field named.
    let mut def = salary_def();
    set_security(
        &mut def,
        "net_pay",
        secured("compensation", Some(MaskFormat::LastChars(9)), "financial"),
    );
    let e = refusal_of(def.validate(), "LastChars(9)")?;
    check(
        e.fields
            .iter()
            .any(|f| f.field.as_str() == "net_pay" && f.code == "meta.mask_invalid"),
        format!("mask_invalid: {e:?}"),
    )
}

/// `declassified` without a reason is refused (`meta.declassify_reason`): a declassification
/// is a reviewed decision, and the review needs the reason on record (ADR-068 A1, item 5).
/// Pure definition validation, so no host is needed; the signature keeps the suite's shape.
async fn declassified_without_reason_is_refused(_h: &dyn TestHost) -> CaseResult {
    for empty in ["", "   "] {
        let mut def = salary_def();
        set_security(
            &mut def,
            "net_pay",
            secured("compensation", None, "financial"),
        );
        let f = def
            .fields
            .iter_mut()
            .find(|f| f.name.as_str() == "net_pay")
            .unwrap_or_else(|| panic!("net_pay"));
        f.security.declassified = Some(empty.to_owned());
        let e = refusal_of(def.validate(), "an empty reason")?;
        check(
            e.fields
                .iter()
                .any(|f| f.field.as_str() == "net_pay" && f.code == "meta.declassify_reason"),
            format!("declassify_reason for {empty:?}: {e:?}"),
        )?;
    }
    // A real reason passes — and an authored inherited_groups lists is refused by hand-set
    // inheritance (spot-meta computes it, never the definition).
    let mut def = salary_def();
    set_security(
        &mut def,
        "net_pay",
        secured("compensation", None, "financial"),
    );
    let f = def
        .fields
        .iter_mut()
        .find(|f| f.name.as_str() == "net_pay")
        .unwrap_or_else(|| panic!("net_pay"));
    f.security.declassified = Some("Approved by the access review, ticket 2026-4471".to_owned());
    def.validate()
        .map_err(|e| Failure(format!("a reasoned declassification is valid: {e}")))?;
    let mut def = salary_def();
    let f = def
        .fields
        .iter_mut()
        .find(|f| f.name.as_str() == "net_pay")
        .unwrap_or_else(|| panic!("net_pay"));
    f.security.inherited_groups =
        vec![crate::meta::GroupName::new("compensation").unwrap_or_else(|e| panic!("{e}"))];
    let e = refusal_of(def.validate(), "hand-authored inherited_groups")?;
    check(
        e.fields
            .iter()
            .any(|f| f.field.as_str() == "net_pay" && f.code == "meta.inherited_authored"),
        format!("inherited_authored: {e:?}"),
    )
}

fn class(s: &str) -> EffectClassName {
    EffectClassName::new(s).unwrap_or_else(|e| panic!("{e}"))
}

fn store(s: &str) -> StoreName {
    StoreName::new(s).unwrap_or_else(|e| panic!("{e}"))
}

fn internal(reversal: Reversal) -> EffectDecl {
    EffectDecl::new(
        EffectKind::Internal,
        class("balance_movement"),
        vec![store("ledger_entry")],
        reversal,
    )
}

fn external(reversal: Reversal) -> EffectDecl {
    EffectDecl::new(
        EffectKind::External,
        class("mail"),
        vec![store("outbound_mail")],
        reversal,
    )
}

fn sample_receipt() -> EffectReceipt {
    EffectReceipt {
        step: StepName::new("ledger:write_entries").unwrap_or_else(|e| panic!("{e}")),
        class: class("balance_movement"),
        kind: EffectKind::Internal,
        store_refs: vec![OpaqueRef::new("row-1").unwrap_or_else(|e| panic!("{e}"))],
        count: 1,
        digest: Hex32::new(&"ab".repeat(32)).unwrap_or_else(|e| panic!("{e}")),
        state: ReceiptState::Applied,
    }
}

/// An internal effect must be reversible by a lifecycle operation; an external one may be
/// anything; an irreversible one needs a reason; names are validated (ADR-080 6.3).
/// Pure declaration checks, so no host is needed.
async fn effect_declarations_are_validated(_h: &dyn TestHost) -> CaseResult {
    internal(Reversal::ReversibleBy(Operation::Cancel))
        .validate()
        .map_err(|e| Failure(format!("a reversible internal effect is valid: {e}")))?;
    for (what, reversal) in [
        (
            "internal compensable",
            Reversal::CompensableBy(
                ActionRef::new("ledger:send_correction").unwrap_or_else(|e| panic!("{e}")),
            ),
        ),
        (
            "internal irreversible",
            Reversal::Irreversible {
                reason: "no way back".to_owned(),
            },
        ),
    ] {
        let e = refusal_of(internal(reversal).validate(), what)?;
        check(
            e.code == "effects.internal_not_reversible",
            format!("{what}: {}", e.code),
        )?;
    }
    let mut no_store = internal(Reversal::ReversibleBy(Operation::Cancel));
    no_store.writes.clear();
    let e = refusal_of(no_store.validate(), "an internal effect with no store")?;
    check(
        e.code == "effects.internal_writes_nothing",
        format!("no store: {}", e.code),
    )?;
    for reversal in [
        Reversal::ReversibleBy(Operation::Cancel),
        Reversal::CompensableBy(
            ActionRef::new("mail:send_correction").unwrap_or_else(|e| panic!("{e}")),
        ),
        Reversal::Irreversible {
            reason: "a fiscal transmission cannot be recalled".to_owned(),
        },
    ] {
        external(reversal.clone())
            .validate()
            .map_err(|e| Failure(format!("external {reversal:?} is valid: {e}")))?;
    }
    let e = refusal_of(
        external(Reversal::Irreversible {
            reason: "  ".to_owned(),
        })
        .validate(),
        "an irreversible effect with no reason",
    )?;
    check(
        e.code == "effects.irreversible_reason",
        format!("reason: {}", e.code),
    )?;
    check(
        Reversal::ReversibleBy(Operation::Cancel).reversibility() == Reversibility::Reversible
            && Reversal::Irreversible {
                reason: "x".to_owned(),
            }
            .reversibility()
                == Reversibility::Irreversible,
        "the reversibility summary follows the reversal",
    )?;
    for bad in ["", "Mail", "1x", "a-b"] {
        check(EffectClassName::new(bad).is_err(), format!("class {bad:?}"))?;
        check(StoreName::new(bad).is_err(), format!("store {bad:?}"))?;
    }
    for bad in ["nocolon", "a:", ":b", "A:b"] {
        check(ActionRef::new(bad).is_err(), format!("action {bad:?}"))?;
    }
    check(
        OpaqueRef::new("").is_err()
            && OpaqueRef::new(&"x".repeat(OpaqueRef::MAX_LEN + 1)).is_err()
            && OpaqueRef::new("a\nb").is_err()
            && OpaqueRef::new(&"x".repeat(OpaqueRef::MAX_LEN)).is_ok(),
        "opaque references are 1 to 128 bytes with no control characters",
    )?;
    check(
        Hex32::new("abc").is_err() && Hex32::new(&"AB".repeat(32)).is_err(),
        "a digest is 64 lowercase hex characters",
    )
}

/// The placement rules: an internal effect runs in phase 3 and, on `post`, is reversed by
/// `cancel`; an external effect runs in phase 4; declarations name real steps, once.
async fn effects_are_checked_against_the_step_point(_h: &dyn TestHost) -> CaseResult {
    let post_after = HookPoint::of(Phase::After, Target::Op(Operation::Post));
    let post_committed = HookPoint::of(Phase::Committed, Target::Op(Operation::Post));
    let by_cancel = internal(Reversal::ReversibleBy(Operation::Cancel));
    by_cancel
        .validate_at(&post_after)
        .map_err(|e| Failure(format!("post effect reversed by cancel: {e}")))?;
    let e = refusal_of(
        internal(Reversal::ReversibleBy(Operation::Delete)).validate_at(&post_after),
        "a post effect reversed by delete",
    )?;
    check(e.code == "effects.wrong_reversal", e.code.clone())?;
    let e = refusal_of(
        by_cancel.validate_at(&post_committed),
        "internal in phase 4",
    )?;
    check(e.code == "effects.wrong_phase", e.code.clone())?;
    let mail = external(Reversal::Irreversible {
        reason: "sent".to_owned(),
    });
    mail.validate_at(&post_committed)
        .map_err(|e| Failure(format!("external in phase 4: {e}")))?;
    let e = refusal_of(mail.validate_at(&post_after), "external in phase 3")?;
    check(e.code == "effects.wrong_phase", e.code.clone())?;

    let decl = |name: &str, point: HookPoint| StepDecl {
        name: StepName::new(name).unwrap_or_else(|e| panic!("{e}")),
        point,
        priority: 0,
        budget: Duration::from_secs(5),
    };
    let steps = vec![
        decl("ledger:write_entries", post_after),
        decl("mail:notify", post_committed),
    ];
    let se = |name: &str, e: EffectDecl| {
        StepEffects::new(
            StepName::new(name).unwrap_or_else(|e| panic!("{e}")),
            vec![e],
        )
    };
    let good = vec![
        se("ledger:write_entries", by_cancel.clone()),
        se("mail:notify", mail),
    ];
    validate_declarations(&steps, &good)
        .map_err(|e| Failure(format!("matching declarations: {e}")))?;
    let e = refusal_of(
        validate_declarations(&steps, &[se("ledger:missing", by_cancel.clone())]),
        "effects on a step that does not exist",
    )?;
    check(e.code == "effects.unknown_step", e.code.clone())?;
    let e = refusal_of(
        validate_declarations(
            &steps,
            &[
                se("ledger:write_entries", by_cancel.clone()),
                se("ledger:write_entries", by_cancel),
            ],
        ),
        "the same step twice",
    )?;
    check(e.code == "effects.duplicate_step", e.code.clone())?;
    // A report is conformant only when its class and kind were declared by that step.
    let r = sample_receipt();
    check(
        r.matches_declaration(&good),
        "a declared effect may be reported",
    )?;
    let mut other_class = r.clone();
    other_class.class = class("mail");
    check(
        !other_class.matches_declaration(&good),
        "an undeclared class is a conformance failure",
    )?;
    check(
        !r.matches_declaration(&[]),
        "a step with no declaration reports nothing",
    )
}

/// Receipts keep stable wire names and round-trip; the state names are fixed.
async fn receipts_round_trip(_h: &dyn TestHost) -> CaseResult {
    let receipt = OperationReceipt {
        operation: Operation::Post,
        doctype: "sales_invoice".to_owned(),
        name: "SINV-0001".to_owned(),
        version: Version(3),
        effects: vec![
            sample_receipt(),
            EffectReceipt {
                kind: EffectKind::External,
                class: class("mail"),
                step: StepName::new("mail:notify").unwrap_or_else(|e| panic!("{e}")),
                state: ReceiptState::Failed {
                    code: "mail.refused".to_owned(),
                },
                ..sample_receipt()
            },
        ],
        committed_at: OffsetDateTime::UNIX_EPOCH + Duration::from_secs(86_400),
    };
    let wire = serde_json::to_value(&receipt)
        .map_err(|e| Failure(format!("receipt does not encode: {e}")))?;
    check(
        wire["operation"] == json!("post")
            && wire["version"] == json!(3)
            && wire["committed_at"] == json!(86_400_000_000_i64)
            && wire["effects"][0]["kind"] == json!("internal")
            && wire["effects"][0]["state"] == json!("applied")
            && wire["effects"][0]["step"] == json!("ledger:write_entries")
            && wire["effects"][1]["state"] == json!({"failed": {"code": "mail.refused"}}),
        format!("wire form: {wire}"),
    )?;
    let back: OperationReceipt = serde_json::from_value(wire)
        .map_err(|e| Failure(format!("receipt does not decode: {e}")))?;
    check(back == receipt, "a receipt round-trips unchanged")?;
    for (state, text) in [
        (ReceiptState::Applied, "applied"),
        (ReceiptState::Held, "held"),
        (ReceiptState::Released, "released"),
        (ReceiptState::Delivered, "delivered"),
    ] {
        check(
            serde_json::to_value(&state).ok() == Some(json!(text)),
            format!("state name {text}"),
        )?;
    }
    let bad = json!({"operation": "explode", "doctype": "d", "name": "n", "version": 1,
        "effects": [], "committed_at": 0});
    check(
        serde_json::from_value::<OperationReceipt>(bad).is_err(),
        "an unknown operation is refused on decode",
    )?;
    let empty = OperationReceipt {
        effects: Vec::new(),
        ..receipt
    };
    let again: OperationReceipt =
        serde_json::from_value(serde_json::to_value(&empty).map_err(|e| Failure(format!("{e}")))?)
            .map_err(|e| Failure(format!("{e}")))?;
    check(again == empty, "an empty receipt is valid")
}

/// `report_effect` has a default that does nothing: a step may call it and the operation still
/// succeeds, for hosts that do not collect receipts.
async fn report_effect_defaults_to_a_no_op(h: &dyn TestHost) -> CaseResult {
    h.register(Arc::new(Probe::new("customer").step(
        "probe:report",
        HookPoint::of(Phase::After, Target::Save),
        0,
        step_fn(|ctx| {
            Box::pin(async move {
                ctx.report_effect(sample_receipt());
                Ok(())
            })
        }),
    )));
    let mut d = Document::new(n("customer"), h.fresh_id());
    d.set(n("customer_name"), text("A"));
    let out = h.create(&inv(), d).await?;
    check(
        out.messages.is_empty(),
        "reporting an effect through the default changes nothing",
    )
}

/// A controller written against 0.4.0 (no effect declarations at all) registers and runs, and
/// its declarations check out as "no declared effects".
async fn a_040_controller_still_passes(h: &dyn TestHost) -> CaseResult {
    let probe = Probe::new("customer").step(
        "probe:plain",
        HookPoint::of(Phase::Validate, Target::Save),
        0,
        step_fn(|_ctx| Box::pin(async { Ok(()) })),
    );
    check(
        probe.step_effects().is_empty(),
        "the default step_effects is empty",
    )?;
    validate_declarations(&probe.steps(), &probe.step_effects())
        .map_err(|e| Failure(format!("no declarations is valid: {e}")))?;
    h.register(Arc::new(probe));
    let mut d = Document::new(n("customer"), h.fresh_id());
    d.set(n("customer_name"), text("A"));
    h.create(&inv(), d).await?;
    Ok(())
}

/// A declaration derives its right from `mutates`, `with_right` states a finer one, and the wire
/// form is the native API's right names (`{"permission": "<name>"}` for an app permission).
async fn method_rights_derive_and_round_trip(_h: &dyn TestHost) -> CaseResult {
    let read = MethodDecl::new(n("ping"), false);
    check(
        read.right == MethodRight::Read && !read.right.implies_mutates(),
        "a method that does not mutate is Read",
    )?;
    let write = MethodDecl::new(n("bump"), true);
    check(
        write.right == MethodRight::Write && write.right.implies_mutates(),
        "a mutating method is Write",
    )?;
    let submit = MethodDecl::new(n("post_it"), true).with_right(MethodRight::Submit);
    check(
        submit.name == n("post_it") && submit.mutates && submit.right == MethodRight::Submit,
        "with_right keeps the name and mutates",
    )?;
    for (right, text) in [
        (MethodRight::Read, "read"),
        (MethodRight::Write, "write"),
        (MethodRight::Create, "create"),
        (MethodRight::Submit, "submit"),
        (MethodRight::Cancel, "cancel"),
    ] {
        check(right.as_str() == text, format!("{text} as text"))?;
        let wire = serde_json::to_value(&right).map_err(|e| Failure(format!("{e}")))?;
        check(wire == json!(text), format!("{text} on the wire: {wire}"))?;
        let back: MethodRight =
            serde_json::from_value(wire).map_err(|e| Failure(format!("{text} decodes: {e}")))?;
        check(back == right, format!("{text} round-trips"))?;
    }
    let permission = MethodRight::Permission(n("run_reports"));
    check(
        permission.as_str() == "run_reports"
            && permission.permission() == Some(&n("run_reports"))
            && permission.implies_mutates(),
        "a permission carries its name and mutates by default",
    )?;
    let wire = serde_json::to_value(&permission).map_err(|e| Failure(format!("{e}")))?;
    check(
        wire == json!({ "permission": "run_reports" }),
        format!("a permission on the wire: {wire}"),
    )?;
    let back: MethodRight =
        serde_json::from_value(wire).map_err(|e| Failure(format!("a permission decodes: {e}")))?;
    check(back == permission, "a permission round-trips")?;
    check(
        serde_json::from_value::<MethodRight>(json!("explode")).is_err(),
        "an unknown right name is refused",
    )?;
    check(
        serde_json::from_value::<MethodRight>(json!({ "permission": "Not A Name" })).is_err(),
        "a malformed permission name is refused",
    )
}

/// A right that contradicts `mutates` is refused: `MethodDecl::contradicts` says so for one
/// declaration and `check_method_rights` fails the controller that declares it. A `Permission`
/// method is accepted whether it mutates or not.
async fn a_method_right_that_contradicts_mutates_is_refused(h: &dyn TestHost) -> CaseResult {
    check(
        MethodDecl::new(n("x"), true)
            .with_right(MethodRight::Read)
            .contradicts(),
        "Read on a method that mutates",
    )?;
    check(
        MethodDecl::new(n("x"), false)
            .with_right(MethodRight::Write)
            .contradicts(),
        "Write on a method that does not mutate",
    )?;
    check(
        MethodDecl::new(n("x"), false)
            .with_right(MethodRight::Submit)
            .contradicts(),
        "Submit on a method that does not mutate",
    )?;
    let permission = MethodRight::Permission(n("approve_credit"));
    check(
        !MethodDecl::new(n("x"), true)
            .with_right(permission.clone())
            .contradicts(),
        "a permission may guard a method that mutates",
    )?;
    check(
        !MethodDecl::new(n("x"), false)
            .with_right(permission)
            .contradicts(),
        "a permission may guard a read-only method when the declaration says so",
    )?;
    let bad = Probe::new("customer").method_with_right(
        "bad",
        false,
        MethodRight::Submit,
        method_fn(|_args, _ctx| Box::pin(async { Ok(json!(null)) })),
    );
    let failure = check_method_rights(&bad)
        .err()
        .ok_or_else(|| Failure("a contradicting controller was accepted".to_owned()))?;
    check(
        failure.to_string().contains("submit") && failure.to_string().contains("customer"),
        format!("the failure names the right and the DocType: {failure}"),
    )?;
    let good = Probe::new("customer").method_with_right(
        "good",
        true,
        MethodRight::Submit,
        method_fn(|_args, _ctx| Box::pin(async { Ok(json!(null)) })),
    );
    check_method_rights(&good)?;
    h.register(Arc::new(good));
    Ok(())
}

/// A controller written against 0.5.0 (its declarations set no right) still registers: the right
/// defaults to `Read` or `Write` from `mutates`, the conformance helper accepts it, and its
/// methods still run.
async fn a_050_controller_still_passes(h: &dyn TestHost) -> CaseResult {
    let probe = Probe::new("customer")
        .method(
            "ping",
            false,
            method_fn(|_args, _ctx| Box::pin(async { Ok(json!({ "pong": true })) })),
        )
        .method(
            "bump",
            true,
            method_fn(|_args, ctx| {
                Box::pin(async move {
                    let (id, v) = (ctx.doc().id(), version(ctx.doc()));
                    let patch = DocumentPatch {
                        fields: [(n("credit_limit"), dec("9.00"))].into(),
                        ..Default::default()
                    };
                    ctx.update("customer", id, patch, v).await?;
                    Ok(json!({ "bumped": true }))
                })
            }),
        );
    let rights: Vec<MethodRight> = probe.methods().into_iter().map(|d| d.right).collect();
    check(
        rights == vec![MethodRight::Read, MethodRight::Write],
        format!("the defaults follow mutates: {rights:?}"),
    )?;
    check_method_rights(&probe)?;
    h.register(Arc::new(probe));
    let c = new_customer(h, "Acme").await?;
    let out = h
        .call_method(
            &inv(),
            "customer",
            c.id(),
            "ping",
            json!({}),
            Some(version(&c)),
        )
        .await?
        .value;
    check(
        out.1 == json!({ "pong": true }),
        "a defaulted method still runs",
    )
}

/// Runs every case, each on a fresh host.
pub async fn run_all(fixture: &dyn Fixture) -> Report {
    run_matching(fixture, |_| true).await
}

/// Runs the cases whose names pass `include`, each on a fresh host. A host under construction
/// runs a subset first; a finished host runs them all with [`run_all`].
pub async fn run_matching(fixture: &dyn Fixture, include: impl Fn(&str) -> bool) -> Report {
    type Case = for<'a> fn(&'a dyn TestHost) -> BoxFuture<'a, CaseResult>;
    macro_rules! cases {
        ($($f:ident),* $(,)?) => { vec![$((stringify!($f), (|h| Box::pin($f(h))) as Case)),*] };
    }
    let all: Vec<(&'static str, Case)> = cases![
        order_of_every_operation,
        order_of_correct,
        before_derives_own_document,
        validate_may_not_change_or_write,
        validate_may_not_write_other_documents,
        before_may_not_write_other_documents,
        after_writes_others_and_everything_rolls_back,
        after_may_not_change_own_document,
        external_calls_only_after_commit,
        committed_failure_does_not_roll_back,
        jobs_only_from_phase_3_and_4,
        stale_version_is_refused,
        merge_patch_semantics,
        typed_field_errors_with_paths,
        state_rules,
        delete_refused_while_linked,
        lifecycle_events,
        messages_are_collected,
        priority_orders_steps_across_controllers,
        overrides,
        default_controller_for_runtime_doctype,
        whitelisted_methods,
        form_and_load_events,
        seams_are_typed,
        select_values_are_enforced_on_write,
        dynamic_link_is_in_the_view_and_accepts_an_id,
        secret_refuses_a_value,
        field_kinds_accept_their_values,
        lookup_round_trips_through_seams,
        protected_field_round_trips,
        undeclared_group_is_refused,
        declassified_without_reason_is_refused,
        multiple_select_round_trips,
        multiple_on_a_non_select_is_refused,
        control_hints_match_the_native_api,
        effect_declarations_are_validated,
        effects_are_checked_against_the_step_point,
        receipts_round_trip,
        report_effect_defaults_to_a_no_op,
        a_040_controller_still_passes,
        method_rights_derive_and_round_trip,
        a_method_right_that_contradicts_mutates_is_refused,
        a_050_controller_still_passes,
        create_only_field_is_set_at_creation_and_never_after,
        create_only_declarations_are_validated,
        consumer_ref_is_an_id_field_set_once,
        a_consumer_actor_runs_an_operation_and_is_audited_as_one,
        a_060_definition_still_passes,
    ];
    let mut report = Report::default();
    for (case, f) in all {
        if !include(case) {
            continue;
        }
        let host = fixture.fresh();
        for def in catalog() {
            host.define(def);
        }
        let result = f(&*host).await;
        report.outcomes.push(CaseOutcome { case, result });
    }
    report
}

// ------------------------------------------------------------------------------------------
// App API 0.7.0 (ADR-083 section 2.3 barrier 4 and section 2.2 item 4; ADR-068 amendment A4)
// ------------------------------------------------------------------------------------------

/// A DocType whose `kind` field may be set only when the document is created, the shape of
/// `User.user_type` (ADR-083 barrier 4), plus one ordinary field.
fn account_def() -> DocTypeDef {
    let mut kind = field(
        "account_kind",
        FieldKind::Select {
            values: vec!["internal".into(), "portal".into()],
        },
        true,
    );
    kind.create_only = true;
    DocTypeDef::new(
        n("account"),
        "Account".into(),
        false,
        false,
        Some(n("account_name")),
        vec![
            field(
                "account_name",
                FieldKind::Text {
                    max_chars: Some(80),
                    format: TextFormat::Plain,
                },
                true,
            ),
            kind,
        ],
    )
}

fn account_patch(name: Option<&str>, kind: Option<Value>) -> DocumentPatch {
    let mut p = DocumentPatch::default();
    if let Some(v) = name {
        p.fields.insert(n("account_name"), text(v));
    }
    if let Some(v) = kind {
        p.fields.insert(n("account_kind"), v);
    }
    p
}

/// `create_only` (settable only at creation): the value given at create stays; a later write
/// that would change it is refused with `document.read_only_field`, whatever the operation
/// path; a write that leaves it as it is is not an error; the field shows in the host's view.
async fn create_only_field_is_set_at_creation_and_never_after(h: &dyn TestHost) -> CaseResult {
    h.define(account_def());
    let view = h.meta("account").await?;
    check(
        view.field("account_kind").is_some_and(|f| f.create_only)
            && view.field("account_name").is_some_and(|f| !f.create_only),
        "create_only is in the host's view, and only on the field that declares it",
    )?;
    let mut d = Document::new(n("account"), h.fresh_id());
    d.set(n("account_name"), text("Asha"));
    d.set(n("account_kind"), text("portal"));
    let a = h.create(&inv(), d).await?.value;

    // An ordinary field still changes.
    let a = h
        .update(
            &inv(),
            "account",
            a.id(),
            account_patch(Some("Asha K"), None),
            version(&a),
        )
        .await?
        .value;
    // Writing the value it already has is not a change.
    let a = h
        .update(
            &inv(),
            "account",
            a.id(),
            account_patch(Some("Asha K."), Some(text("portal"))),
            version(&a),
        )
        .await?
        .value;
    // A different value, or a clear, is refused and names the field; nothing is written.
    for bad in [text("internal"), Value::Null] {
        let e = expect_err(
            h.update(
                &inv(),
                "account",
                a.id(),
                account_patch(Some("Mallory"), Some(bad)),
                version(&a),
            )
            .await,
            "document.validation_failed",
        )?;
        check(
            e.fields.iter().any(|f| {
                f.field.as_str() == "account_kind" && f.code == "document.read_only_field"
            }),
            format!("the refusal names account_kind: {e:?}"),
        )?;
    }
    let stored = h
        .stored("account", a.id())
        .ok_or_else(|| Failure("account missing".into()))?;
    check(
        stored.get("account_kind") == &text("portal")
            && stored.get("account_name") == &text("Asha K."),
        "a refused write changed nothing",
    )?;
    // Creating with a different value on a *new* document is of course fine.
    let mut d = Document::new(n("account"), h.fresh_id());
    d.set(n("account_name"), text("Ops"));
    d.set(n("account_kind"), text("internal"));
    h.create(&inv(), d).await?;
    Ok(())
}

/// A `create_only` field makes sense only on a stored value that is not a child table and that
/// a posted document is not meant to change: the two contradictions are refused.
async fn create_only_declarations_are_validated(_h: &dyn TestHost) -> CaseResult {
    check(
        account_def().validate().is_ok(),
        "the plain declaration is valid",
    )?;

    let mut def = account_def();
    let mut rows = field(
        "rows",
        FieldKind::Table {
            doctype: n("picks"),
        },
        false,
    );
    rows.create_only = true;
    def.fields.push(rows);
    let e = refusal_of(def.validate(), "create_only on a table")?;
    check(
        e.fields
            .iter()
            .any(|f| f.field.as_str() == "rows" && f.code == "meta.create_only_table"),
        format!("create_only_table: {e:?}"),
    )?;

    let mut def = account_def();
    for f in &mut def.fields {
        if f.name.as_str() == "account_kind" {
            f.allow_on_posted = true;
        }
    }
    let e = refusal_of(def.validate(), "create_only with allow_on_posted")?;
    check(
        e.fields.iter().any(|f| {
            f.field.as_str() == "account_kind" && f.code == "meta.create_only_allow_on_posted"
        }),
        format!("create_only_allow_on_posted: {e:?}"),
    )
}

/// A definition with one `ConsumerRef` field, the owner scope of a consumer-caused record
/// (ADR-083 section 2.2 item 4).
fn consumer_order_def(set_once: bool) -> DocTypeDef {
    let mut owner = field("consumer_ref", FieldKind::ConsumerRef, false);
    owner.create_only = set_once;
    DocTypeDef::new(
        n("web_order"),
        "Web Order".into(),
        false,
        false,
        None,
        vec![
            field(
                "note",
                FieldKind::Text {
                    max_chars: Some(80),
                    format: TextFormat::Plain,
                },
                false,
            ),
            owner,
        ],
    )
}

/// `ConsumerRef` holds a consumer's id (not a link: no target DocType, no referential check),
/// accepts only an id, must be `create_only`, and is declared at most once per DocType.
async fn consumer_ref_is_an_id_field_set_once(h: &dyn TestHost) -> CaseResult {
    check(
        FieldKind::ConsumerRef.accepts(&Value::Id(h.fresh_id()))
            && FieldKind::ConsumerRef.accepts(&Value::Null)
            && !FieldKind::ConsumerRef.accepts(&text("consumer:x"))
            && !FieldKind::ConsumerRef.accepts(&Value::Int(1)),
        "a consumer reference is an id or empty",
    )?;
    check(
        consumer_order_def(true).validate().is_ok(),
        "a create_only consumer_ref is valid",
    )?;
    let e = refusal_of(
        consumer_order_def(false).validate(),
        "a consumer_ref that can be changed",
    )?;
    check(
        e.fields.iter().any(|f| {
            f.field.as_str() == "consumer_ref" && f.code == "meta.consumer_ref_not_create_only"
        }),
        format!("consumer_ref_not_create_only: {e:?}"),
    )?;
    let mut two = consumer_order_def(true);
    let mut second = field("buyer_ref", FieldKind::ConsumerRef, false);
    second.create_only = true;
    two.fields.push(second);
    let e = refusal_of(two.validate(), "two consumer_ref fields")?;
    check(
        e.fields
            .iter()
            .any(|f| f.code == "meta.consumer_ref_duplicate"),
        format!("consumer_ref_duplicate: {e:?}"),
    )?;

    h.define(consumer_order_def(true));
    let (mine, other) = (h.fresh_id(), h.fresh_id());
    let mut d = Document::new(n("web_order"), h.fresh_id());
    d.set(n("consumer_ref"), Value::Id(mine));
    let o = h.create(&inv(), d).await?.value;
    let mut p = DocumentPatch::default();
    p.fields.insert(n("consumer_ref"), Value::Id(other));
    let e = expect_err(
        h.update(&inv(), "web_order", o.id(), p, version(&o)).await,
        "document.validation_failed",
    )?;
    check(
        e.fields
            .iter()
            .any(|f| f.field.as_str() == "consumer_ref" && f.code == "document.read_only_field"),
        format!("a record cannot be moved to another consumer: {e:?}"),
    )
}

/// An invocation whose actor is a consumer is an ordinary invocation: the host runs it, and the
/// audit trail names the actor as `consumer:<id>`, never as a user (ADR-083 2.8 item 4).
async fn a_consumer_actor_runs_an_operation_and_is_audited_as_one(h: &dyn TestHost) -> CaseResult {
    let consumer = h.fresh_id();
    let mut i = inv();
    i.actor = Actor::Consumer { id: consumer };
    let mut d = Document::new(n("customer"), h.fresh_id());
    d.set(n("customer_name"), text("Acme"));
    h.create(&i, d).await?;
    let evs = events_of(h, "doc_created");
    check(evs.len() == 1, "one doc_created event")?;
    let p = PayloadDocument::from_outbox_event(&evs[0]).map_err(|x| Failure(x.to_string()))?;
    check(
        p.meta.actor == spot_events_contract::Actor::Consumer { id: consumer },
        format!("the event names the consumer: {:?}", p.meta.actor),
    )?;
    check(
        p.meta.actor.principal_text() == format!("consumer:{consumer}"),
        "the actor's text is typed",
    )
}

/// A 0.6.0 definition, written with the constructor and no knowledge of `create_only`, is valid
/// and behaves as before: nothing it declares is create-only and every field can be updated.
async fn a_060_definition_still_passes(h: &dyn TestHost) -> CaseResult {
    for def in catalog() {
        check(
            def.fields.iter().all(|f| !f.create_only),
            format!("{} declares no create_only field", def.name),
        )?;
        check(def.validate().is_ok(), format!("{} is valid", def.name))?;
    }
    let c = new_customer(h, "Acme").await?;
    let mut p = DocumentPatch::default();
    p.fields.insert(n("customer_name"), text("Acme Ltd"));
    let c = h
        .update(&inv(), "customer", c.id(), p, version(&c))
        .await?
        .value;
    check(
        c.get("customer_name") == &text("Acme Ltd"),
        "an ordinary field still updates",
    )
}
