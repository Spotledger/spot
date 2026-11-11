//! The shared controller conformance suite.
//!
//! [`run`] exercises a controller through the **real document engine** on the fake backend and
//! reports what it found. It is function-based and lives in this crate so any adapter — the
//! TypeScript one here, and a WASM adapter later — can run the same checks and prove its
//! controllers behave like a native one. The tests run it for a native reference controller and
//! the TypeScript equivalent and print a table of any behavioural difference.
//!
//! The suite is parameterised by a [`ConformanceFixture`] describing the DocType and the steps and
//! methods the controller under test must implement; [`ConformanceFixture::note`] is the fixture
//! the tests use.

use std::sync::Arc;

use serde_json::{Value as Json, json};
use spot_app_api::context::DocumentService;
use spot_app_api::prelude::{Name, RecordId, Value};
use spot_app_api::{Actor, AppError, AppResult, BoxFuture, Document, ErrorClass, Invocation};
use spot_controllers::{ControllerFactory, ControllerRef, ControllerRegistry, ControllerSource};
use spot_core::{SiteId, SystemClock};
use spot_document::engine::{
    AllowAllAuthorizer, Authorizer, Engine, QueryPlan, V7Ids, WriteChange,
};
use spot_meta::{Definition, FieldSpec, FieldType, Registry};
use spot_permissions::{Action, Decision, DocFacts, Principal};
use spot_storage_fake::FakeBackend;

/// The DocType, fields, steps and methods the controller under test must implement.
#[derive(Clone, Debug, PartialEq)]
pub struct ConformanceFixture {
    /// The DocType the controller serves.
    pub doctype: String,
    /// The text field the `before_save` step fills.
    pub title_field: String,
    /// The decimal field a money method echoes.
    pub total_field: String,
    /// The boolean field that turns the `validate` step's patch on.
    pub flag_field: String,
    /// What the `before_save` step sets the title to.
    pub filled_title: String,
    /// The `before_save` step's name.
    pub fill_step: String,
    /// The `validate` step's name (it returns a patch when the flag is set).
    pub check_step: String,
    /// The read-only method that returns [`ConformanceFixture::ping_result`].
    pub ping: String,
    /// The mutating method taking `{ amount: decimal, fail?: bool }` and returning `{ total }`.
    pub bump: String,
    /// What [`ConformanceFixture::ping`] returns.
    pub ping_result: Json,
    /// The code [`ConformanceFixture::bump`] throws when `fail` is true.
    pub fail_code: String,
}

impl ConformanceFixture {
    /// The fixture the tests use: a `note` DocType.
    pub fn note() -> ConformanceFixture {
        ConformanceFixture {
            doctype: "note".to_owned(),
            title_field: "title".to_owned(),
            total_field: "total".to_owned(),
            flag_field: "flag".to_owned(),
            filled_title: "conformed".to_owned(),
            fill_step: "fill_title".to_owned(),
            check_step: "peek".to_owned(),
            ping: "ping".to_owned(),
            bump: "bump".to_owned(),
            ping_result: json!({ "pong": true }),
            fail_code: "tscontroller.conformance_fail".to_owned(),
        }
    }
}

/// One check the suite ran.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    /// What the check proves.
    pub name: String,
    /// Whether it held.
    pub ok: bool,
    /// What happened, for the table.
    pub detail: String,
}

/// What the suite found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConformanceReport {
    /// Every check, in the order the suite ran them.
    pub checks: Vec<Check>,
}

impl ConformanceReport {
    /// Whether every check held.
    pub fn passed(&self) -> bool {
        self.checks.iter().all(|check| check.ok)
    }

    /// The checks whose outcome differs from `other`, as one line each.
    pub fn differences(&self, other: &ConformanceReport) -> Vec<String> {
        let mut out = Vec::new();
        for (mine, theirs) in self.checks.iter().zip(&other.checks) {
            if mine != theirs {
                out.push(format!(
                    "{}: {} vs {}",
                    mine.name,
                    if mine.ok { "ok" } else { "failed" },
                    if theirs.ok { "ok" } else { "failed" }
                ));
            }
        }
        out
    }

    /// The report as a table, one row per check.
    pub fn table(&self) -> String {
        let mut out = String::from("check\tresult\tdetail\n");
        for check in &self.checks {
            out.push_str(&format!(
                "{}\t{}\t{}\n",
                check.name,
                if check.ok { "ok" } else { "FAILED" },
                check.detail
            ));
        }
        out
    }

    fn record(&mut self, name: &str, result: AppResult<String>) {
        match result {
            Ok(detail) => self.checks.push(Check {
                name: name.to_owned(),
                ok: true,
                detail,
            }),
            Err(error) => self.checks.push(Check {
                name: name.to_owned(),
                ok: false,
                detail: format!("{}: {}", error.code, error.message),
            }),
        }
    }
}

/// Runs the conformance suite for the controller `reference` names, built by `factory`, against
/// `fixture`.
///
/// # Errors
///
/// A report is always returned; a controller the factory cannot build is a single failed check.
pub async fn run(
    reference: &ControllerRef,
    factory: &dyn ControllerFactory,
    fixture: &ConformanceFixture,
) -> ConformanceReport {
    let mut report = ConformanceReport::default();
    let controller = match factory.create(reference) {
        Ok(controller) => controller,
        Err(error) => {
            report.checks.push(Check {
                name: "load".to_owned(),
                ok: false,
                detail: format!("{}: {}", error.code, error.message),
            });
            return report;
        }
    };
    let app = match reference.source() {
        ControllerSource::Native => "native".to_owned(),
        _ => "ts_controllers".to_owned(),
    };
    let mut registry = ControllerRegistry::new();
    registry.register(&app, Arc::clone(&controller), reference.source());
    let seams = match registry.install() {
        Ok(seams) => seams,
        Err(conflicts) => {
            report.checks.push(Check {
                name: "install".to_owned(),
                ok: false,
                detail: format!("{} step conflicts", conflicts.len()),
            });
            return report;
        }
    };
    let meta = match note_meta() {
        Ok(meta) => meta,
        Err(error) => {
            report.checks.push(Check {
                name: "meta".to_owned(),
                ok: false,
                detail: error.message.clone(),
            });
            return report;
        }
    };
    let allowed = match build_engine(Arc::clone(&seams), &meta, Arc::new(AllowAllAuthorizer)) {
        Ok(engine) => engine,
        Err(error) => {
            report.checks.push(Check {
                name: "engine".to_owned(),
                ok: false,
                detail: error.message.clone(),
            });
            return report;
        }
    };
    allowed.set_controllers(vec![Arc::clone(&controller)]);
    let inv = match system_inv() {
        Ok(inv) => inv,
        Err(error) => {
            report.checks.push(Check {
                name: "invocation".to_owned(),
                ok: false,
                detail: error.message.clone(),
            });
            return report;
        }
    };

    report.record(
        "before_save step mutates its document",
        check_fill_step(&allowed, &inv, fixture).await,
    );
    report.record(
        "validate may not mutate its document",
        check_validate_immutable(&allowed, &inv, fixture).await,
    );
    report.record(
        "method dispatch for an allowed principal",
        check_ping(&allowed, &inv, fixture).await,
    );
    report.record(
        "method dispatch for a denied principal",
        check_denied(&controller, &seams, fixture, &inv).await,
    );
    report.record(
        "arguments are validated before the handler runs",
        check_bad_args(&allowed, &inv, fixture).await,
    );
    report.record(
        "a decimal crosses the bridge as its exact string",
        check_decimal(&allowed, &inv, fixture).await,
    );
    report.record(
        "a typed throw fails the operation and rolls back",
        check_rollback(&allowed, &inv, fixture).await,
    );
    report.record(
        "the same call gives the same result over 100 runs",
        check_determinism(&allowed, &inv, fixture).await,
    );
    report
}

/// The `note` DocType the suite uses.
fn note_meta() -> AppResult<Arc<spot_meta::EffectiveMeta>> {
    let mut note = Definition::new("note");
    note.fields.push(FieldSpec::new("title", FieldType::Text));
    let mut total = FieldSpec::new("total", FieldType::Decimal);
    total.scale = Some(2);
    note.fields.push(total);
    note.fields.push(FieldSpec::new("flag", FieldType::Bool));
    let registry = Registry::new(vec![note]).map_err(|e| {
        AppError::new(
            ErrorClass::Internal,
            "meta.validation_failed",
            format!("the conformance metadata does not compile: {e:?}"),
        )
    })?;
    Ok(registry.effective())
}

/// Builds a fresh engine on the fake backend with `authorizer`.
fn build_engine(
    seams: Arc<spot_seams::Registry>,
    meta: &Arc<spot_meta::EffectiveMeta>,
    authorizer: Arc<dyn Authorizer>,
) -> AppResult<Engine<FakeBackend>> {
    let engine = Engine::new(
        FakeBackend::new(),
        Arc::clone(meta),
        seams,
        Arc::new(SystemClock::new()),
        Arc::new(V7Ids),
    )?
    .with_authorizer(authorizer);
    Ok(engine)
}

fn system_inv() -> AppResult<Invocation> {
    Ok(Invocation {
        site: SiteId::new("conformance.test").map_err(|e| internal(&e.to_string()))?,
        actor: Actor::System {
            component: Name::new("conformance").map_err(|e| internal(&e.to_string()))?,
        },
        command_id: None,
        language: None,
        now: time::OffsetDateTime::UNIX_EPOCH,
    })
}

fn user_inv(base: &Invocation) -> Invocation {
    Invocation {
        actor: Actor::User {
            id: RecordId::new_v7(),
            via: None,
        },
        ..base.clone()
    }
}

fn internal(why: &str) -> AppError {
    AppError::new(
        ErrorClass::Internal,
        "tscontroller.conformance_failed",
        format!("the conformance fixture is invalid: {why}"),
    )
}

/// Creates a note with `flag` set, returning it.
async fn create_note(
    engine: &Engine<FakeBackend>,
    inv: &Invocation,
    fixture: &ConformanceFixture,
    flag: bool,
) -> AppResult<Document> {
    let mut draft = Document::new(
        Name::new(&fixture.doctype).map_err(|e| internal(&e.to_string()))?,
        RecordId::new_v7(),
    );
    draft.set(
        Name::new(&fixture.title_field).map_err(|e| internal(&e.to_string()))?,
        Value::Text("draft".to_owned()),
    );
    draft.set(
        Name::new(&fixture.flag_field).map_err(|e| internal(&e.to_string()))?,
        Value::Bool(flag),
    );
    Ok(engine.create(inv, draft).await?.value)
}

async fn check_fill_step(
    engine: &Engine<FakeBackend>,
    inv: &Invocation,
    fixture: &ConformanceFixture,
) -> AppResult<String> {
    let created = create_note(engine, inv, fixture, false).await?;
    let stored = engine
        .peek(&fixture.doctype, created.id())
        .await?
        .ok_or_else(not_found)?;
    let title = match stored.get(&fixture.title_field) {
        Value::Text(text) => text.clone(),
        other => format!("{other:?}"),
    };
    if title == fixture.filled_title {
        Ok(format!("title = {}", fixture.filled_title))
    } else {
        Err(AppError::new(
            ErrorClass::Internal,
            "tscontroller.conformance_wrong",
            format!("the before_save step left the title as `{title}`"),
        ))
    }
}

async fn check_validate_immutable(
    engine: &Engine<FakeBackend>,
    inv: &Invocation,
    fixture: &ConformanceFixture,
) -> AppResult<String> {
    match create_note(engine, inv, fixture, true).await {
        Err(error) if error.code == "platform.phase_violation" => Ok(error.code.clone()),
        Err(error) => Err(AppError::new(
            ErrorClass::Internal,
            "tscontroller.conformance_wrong",
            format!("a validate step's change was refused with `{}`", error.code),
        )),
        Ok(_) => Err(AppError::new(
            ErrorClass::Internal,
            "tscontroller.conformance_wrong",
            "a validate step changed the document and the operation succeeded".to_owned(),
        )),
    }
}

async fn check_ping(
    engine: &Engine<FakeBackend>,
    inv: &Invocation,
    fixture: &ConformanceFixture,
) -> AppResult<String> {
    let created = create_note(engine, inv, fixture, false).await?;
    let (_, result) = engine
        .call_method(
            inv,
            &fixture.doctype,
            created.id(),
            &fixture.ping,
            json!({}),
            None,
        )
        .await?
        .value;
    if result == fixture.ping_result {
        Ok("ping returned the declared result".to_owned())
    } else {
        Err(AppError::new(
            ErrorClass::Internal,
            "tscontroller.conformance_wrong",
            format!("ping returned {result}"),
        ))
    }
}

// The denied user may read the document but holds no write right, so the mutating `bump`
// reaches the right check and is refused with `permission.denied` (never `method.not_found`).
async fn check_denied(
    controller: &Arc<dyn spot_app_api::Controller>,
    seams: &Arc<spot_seams::Registry>,
    fixture: &ConformanceFixture,
    inv: &Invocation,
) -> AppResult<String> {
    let meta = note_meta()?;
    let engine = build_engine(Arc::clone(seams), &meta, Arc::new(DenyUsers))?;
    engine.set_controllers(vec![Arc::clone(controller)]);
    let created = create_note(&engine, inv, fixture, false).await?;
    match engine
        .call_method(
            &user_inv(inv),
            &fixture.doctype,
            created.id(),
            &fixture.bump,
            json!({ "amount": "1" }),
            None,
        )
        .await
    {
        Err(error) if error.class == ErrorClass::PermissionDenied => Ok(error.code.clone()),
        Err(error) => Err(AppError::new(
            ErrorClass::Internal,
            "tscontroller.conformance_wrong",
            format!("a denied principal was refused with `{}`", error.code),
        )),
        Ok(_) => Err(AppError::new(
            ErrorClass::Internal,
            "tscontroller.conformance_wrong",
            "a denied principal ran the method".to_owned(),
        )),
    }
}

async fn check_bad_args(
    engine: &Engine<FakeBackend>,
    inv: &Invocation,
    fixture: &ConformanceFixture,
) -> AppResult<String> {
    let created = create_note(engine, inv, fixture, false).await?;
    match engine
        .call_method(
            inv,
            &fixture.doctype,
            created.id(),
            &fixture.bump,
            json!({ "amount": 1.5 }),
            None,
        )
        .await
    {
        Err(error)
            if error.class == ErrorClass::Validation
                && error
                    .fields
                    .iter()
                    .any(|field| field.code == "whitelist.bad_arguments") =>
        {
            Ok("whitelist.bad_arguments".to_owned())
        }
        Err(error) => Err(AppError::new(
            ErrorClass::Internal,
            "tscontroller.conformance_wrong",
            format!(
                "a JavaScript number argument was refused with `{}`",
                error.code
            ),
        )),
        Ok(_) => Err(AppError::new(
            ErrorClass::Internal,
            "tscontroller.conformance_wrong",
            "a JavaScript number argument was accepted".to_owned(),
        )),
    }
}

async fn check_decimal(
    engine: &Engine<FakeBackend>,
    inv: &Invocation,
    fixture: &ConformanceFixture,
) -> AppResult<String> {
    let created = create_note(engine, inv, fixture, false).await?;
    let (_, result) = engine
        .call_method(
            inv,
            &fixture.doctype,
            created.id(),
            &fixture.bump,
            json!({ "amount": "1.50" }),
            None,
        )
        .await?
        .value;
    if result == json!({ "total": "1.50" }) {
        Ok("the decimal string survived the bridge".to_owned())
    } else {
        Err(AppError::new(
            ErrorClass::Internal,
            "tscontroller.conformance_wrong",
            format!("bump returned {result}"),
        ))
    }
}

async fn check_rollback(
    engine: &Engine<FakeBackend>,
    inv: &Invocation,
    fixture: &ConformanceFixture,
) -> AppResult<String> {
    let created = create_note(engine, inv, fixture, false).await?;
    match engine
        .call_method(
            inv,
            &fixture.doctype,
            created.id(),
            &fixture.bump,
            json!({ "amount": "1.00", "fail": true }),
            None,
        )
        .await
    {
        Err(error) if error.code == fixture.fail_code => Ok(error.code.clone()),
        Err(error) => Err(AppError::new(
            ErrorClass::Internal,
            "tscontroller.conformance_wrong",
            format!("the typed throw surfaced as `{}`", error.code),
        )),
        Ok(_) => Err(AppError::new(
            ErrorClass::Internal,
            "tscontroller.conformance_wrong",
            "the typed throw did not fail the method".to_owned(),
        )),
    }
}

async fn check_determinism(
    engine: &Engine<FakeBackend>,
    inv: &Invocation,
    fixture: &ConformanceFixture,
) -> AppResult<String> {
    let created = create_note(engine, inv, fixture, false).await?;
    let mut first: Option<Json> = None;
    for run in 0..100 {
        let (_, result) = engine
            .call_method(
                inv,
                &fixture.doctype,
                created.id(),
                &fixture.ping,
                json!({}),
                None,
            )
            .await?
            .value;
        match &first {
            None => first = Some(result),
            Some(expected) if *expected != result => {
                return Err(AppError::new(
                    ErrorClass::Internal,
                    "tscontroller.conformance_wrong",
                    format!("run {run} differed: {result}"),
                ));
            }
            Some(_) => {}
        }
    }
    Ok("100 runs, one result".to_owned())
}

fn not_found() -> AppError {
    AppError::new(
        ErrorClass::NotFound,
        "tscontroller.conformance_not_found",
        "the document was not stored".to_owned(),
    )
}

/// An authorizer that denies every non-system principal, so the suite can prove a denied caller
/// cannot run a method.
#[derive(Clone, Copy, Debug, Default)]
pub struct DenyUsers;

impl Authorizer for DenyUsers {
    fn principal_for<'a>(&'a self, inv: &'a Invocation) -> BoxFuture<'a, Principal> {
        Box::pin(async move {
            match &inv.actor {
                Actor::System { .. } => Principal::system(),
                Actor::User { .. } | Actor::Consumer { .. } => {
                    Principal::user("denied", Vec::new())
                }
            }
        })
    }

    fn decide<'a>(
        &'a self,
        principal: &'a Principal,
        action: Action,
        _facts: &'a DocFacts,
    ) -> BoxFuture<'a, Decision> {
        Box::pin(async move {
            Decision {
                allowed: principal.is_system || action == Action::Read,
                reasons: Vec::new(),
            }
        })
    }

    fn authorize<'a>(
        &'a self,
        principal: &'a Principal,
        action: Action,
        _doc: &'a Document,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move { allow(principal, action) })
    }

    fn authorize_write<'a>(
        &'a self,
        principal: &'a Principal,
        action: Action,
        _doc: &'a Document,
        _changes: &'a [WriteChange],
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move { allow(principal, action) })
    }

    fn readable<'a>(
        &'a self,
        _principal: &'a Principal,
        doc: Document,
        _def: &'a spot_app_api::DocTypeDef,
    ) -> BoxFuture<'a, AppResult<Document>> {
        Box::pin(async move { Ok(doc) })
    }

    fn readable_query<'a>(
        &'a self,
        _principal: &'a Principal,
        query: &'a spot_app_api::prelude::Query,
    ) -> BoxFuture<'a, AppResult<Option<QueryPlan>>> {
        Box::pin(async move {
            Ok(Some(QueryPlan {
                query: query.clone(),
                masks: Vec::new(),
            }))
        })
    }
}

/// A denied user may still read (so a method call reaches the right check and is refused
/// with `permission.denied`, not `method.not_found`); every other action is refused.
fn allow(principal: &Principal, action: Action) -> AppResult<()> {
    if principal.is_system || action == Action::Read {
        Ok(())
    } else {
        Err(AppError::new(
            ErrorClass::PermissionDenied,
            "permission.denied",
            "the acting user may not run this operation".to_owned(),
        ))
    }
}
