//! The controller tests (WP-DX-05.T1): the Time Entry roll-up and the Task owner
//! derivation, on the app API's stub host, with the DocTypes the app ships.
//!
//! The metadata here is the compiled app view of the five definitions — built by
//! hand into `DocTypeDef`s the stub host validates against, so this crate needs
//! nothing but `spot-app-api` (G4). The detached validation crate
//! (`tests/app-validate/`) proves the same shapes load from the TOML text with
//! `spot_meta` and that the app view of that load matches these definitions.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use rust_decimal::Decimal;
use spot_app_api::Controller as _;
use spot_app_api::context::{Actor, DocumentService, Invocation};
use spot_app_api::document::DocState;
use spot_app_api::prelude::{CommandId, Name, RecordId, SiteId, Value};
use spot_app_api::stub::StubHost;
use spot_app_api::{
    DecimalFormat, DocTypeDef, Document, FieldDef, FieldKind, LongTextFormat, Operation, TextFormat,
};
use time::OffsetDateTime;

use spot_sample_app::constants::{
    PROJECT_OWNER, TASK_LOGGED_HOURS, TASK_PROJECT_OWNER, TIME_ENTRY_HOURS,
};
use spot_sample_app::{TaskController, TimeEntryController};

/// `2026-10-10T09:00:00Z`, the tests' fixed `now` (every hook sees the same).
fn now() -> OffsetDateTime {
    OffsetDateTime::parse(
        "2026-10-10T09:00:00Z",
        &time::format_description::well_known::Rfc3339,
    )
    .unwrap()
}

/// A plain `def` helper: name, label, fields, posted flag.
fn doctype(
    name: &str,
    label: &str,
    is_posted: bool,
    fields: Vec<FieldDef>,
    title: Option<&str>,
) -> DocTypeDef {
    let mut def = DocTypeDef::new(
        Name::new(name).unwrap(),
        label.to_owned(),
        is_posted,
        false,
        title.map(|t| Name::new(t).unwrap()),
        fields,
    );
    def.version = 1;
    def
}

/// A field of kind `text`, required per `required`.
fn text(name: &str, required: bool, max_chars: u32) -> FieldDef {
    FieldDef::new(
        Name::new(name).unwrap(),
        FieldKind::Text {
            max_chars: Some(max_chars),
            format: TextFormat::Plain,
        },
        required,
        false,
    )
}

/// A field of kind `link` to `target`.
fn link(name: &str, target: &str, required: bool) -> FieldDef {
    FieldDef::new(
        Name::new(name).unwrap(),
        FieldKind::Link {
            doctype: Name::new(target).unwrap(),
        },
        required,
        false,
    )
}

/// A field of kind `decimal`, scale 2, optionally read-only.
fn decimal_field(name: &str, required: bool, read_only: bool) -> FieldDef {
    let mut f = FieldDef::new(
        Name::new(name).unwrap(),
        FieldKind::Decimal {
            scale: 2,
            precision: 18,
            format: DecimalFormat::Plain,
        },
        required,
        false,
    );
    f.read_only = read_only;
    f
}

/// A date field.
fn date(name: &str, required: bool) -> FieldDef {
    FieldDef::new(Name::new(name).unwrap(), FieldKind::Date, required, false)
}

/// A bool field.
fn bool_field(name: &str) -> FieldDef {
    FieldDef::new(Name::new(name).unwrap(), FieldKind::Bool, false, false)
}

/// A select field over `values`.
fn select(name: &str, values: &[&str], required: bool) -> FieldDef {
    FieldDef::new(
        Name::new(name).unwrap(),
        FieldKind::Select {
            values: values.iter().map(|s| (*s).to_owned()).collect(),
        },
        required,
        false,
    )
}

/// A long-text field.
fn long_text(name: &str) -> FieldDef {
    FieldDef::new(
        Name::new(name).unwrap(),
        FieldKind::LongText {
            format: LongTextFormat::Plain,
        },
        false,
        false,
    )
}

/// A set-field helper: `(name, value)` pairs into a fresh document.
fn set(doc: &mut Document, pairs: &[(&str, Value)]) {
    for (k, v) in pairs {
        doc.set(Name::new(k).unwrap(), v.clone());
    }
}

/// The five DocTypes as the stub host sees them (labels and options ours).
fn host_with_app() -> StubHost {
    let host = StubHost::new();
    // label: name, colour.
    host.define(doctype(
        "label",
        "Label",
        false,
        vec![text("name", true, 50), text("colour", false, 9)],
        Some("name"),
    ));
    // project: code, title, status, owner, dates, budget, currency, description.
    host.define(doctype(
        "project",
        "Project",
        false,
        vec![
            text("code", true, 140),
            text("title", true, 140),
            select("status", &["Planning", "Active", "On Hold", "Done"], true),
            link("owner", "user", true),
            date("start_date", false),
            date("end_date", false),
            decimal_field("budget", false, false),
            select("currency", &["USD", "EUR", "GBP", "PKR"], false),
            long_text("description"),
        ],
        Some("code"),
    ));
    // task with the controller-owned read-only roll-up field.
    host.define(doctype(
        "task",
        "Task",
        false,
        vec![
            text("subject", true, 140),
            link("project", "project", true),
            link("assignee", "user", false),
            link("project_owner", "user", false),
            select("priority", &["Low", "Medium", "High", "Urgent"], false),
            date("due_date", false),
            decimal_field("estimated_hours", false, false),
            decimal_field("logged_hours", false, true),
            select(
                "status",
                &["Open", "In Progress", "Blocked", "Review", "Done"],
                true,
            ),
            long_text("details"),
            long_text("blocked_reason"),
        ],
        Some("subject"),
    ));
    // time_entry: the posted DocType.
    let te = doctype(
        "time_entry",
        "Time Entry",
        true,
        vec![
            text("entry_no", true, 140),
            link("task", "task", true),
            link("person", "user", true),
            date("work_date", true),
            decimal_field("hours", true, false),
            bool_field("billable"),
            long_text("note"),
        ],
        Some("entry_no"),
    );
    host.define(te);
    // The platform `user` DocType, the link target (the platform app ships it;
    // the stub host needs the target to exist for link checks).
    host.define(doctype(
        "user",
        "User",
        false,
        vec![text("name", true, 140)],
        Some("name"),
    ));
    host.register(Arc::new(TimeEntryController));
    host.register(Arc::new(TaskController));
    host
}

/// An invocation as the system actor (the stub host checks no permissions).
fn inv() -> Invocation {
    Invocation {
        site: SiteId::new("sample.test").unwrap(),
        actor: Actor::System {
            component: Name::new("sample_tests").unwrap(),
        },
        command_id: CommandId::new("test-run").ok(),
        language: None,
        now: now(),
    }
}

/// A fresh record id from the host's generator (deterministic for the tests).
fn fresh(host: &StubHost, n: u64) -> RecordId {
    let _ = host;
    let mut b = [0u8; 16];
    b[6] = 0x70;
    b[8] = 0x80;
    b[8..16].copy_from_slice(&(0x8000_0000_0000_0000u64 | n).to_be_bytes());
    RecordId::from_bytes(b).unwrap()
}

/// A stored project with one owner, and the stored user it links to.
async fn seed_project(host: &StubHost, inv: &Invocation) -> RecordId {
    let user_id = fresh(host, 1);
    let mut user = Document::new(Name::new("user").unwrap(), user_id);
    set(&mut user, &[("name", Value::Text("ama".to_owned()))]);
    host.create(inv, user).await.unwrap();

    let project_id = fresh(host, 2);
    let mut project = Document::new(Name::new("project").unwrap(), project_id);
    set(
        &mut project,
        &[
            ("code", Value::Text("PRJ-2026.-.0001".to_owned())),
            ("title", Value::Text("Platform acceptance run".to_owned())),
            ("status", Value::Text("Active".to_owned())),
            ("owner", Value::Id(user_id)),
        ],
    );
    host.create(inv, project).await.unwrap();
    project_id
}

/// A stored task with the given logged hours (`None` = null) under `project`.
async fn seed_task(
    host: &StubHost,
    inv: &Invocation,
    project: RecordId,
    logged: Option<Decimal>,
) -> RecordId {
    let task_id = fresh(host, 3);
    let mut task = Document::new(Name::new("task").unwrap(), task_id);
    let mut pairs = vec![
        ("subject", Value::Text("Hours roll-up".to_owned())),
        ("project", Value::Id(project)),
        ("status", Value::Text("Open".to_owned())),
    ];
    if let Some(h) = logged {
        pairs.push(("logged_hours", Value::Decimal(h)));
    }
    set(&mut task, &pairs);
    host.create(inv, task).await.unwrap();
    task_id
}

/// A draft Time Entry over `task` with `hours` (when set); returns id and doc.
async fn seed_time_entry(
    host: &StubHost,
    inv: &Invocation,
    task: RecordId,
    hours: Option<Decimal>,
) -> (RecordId, Document) {
    let te_id = fresh(host, 4);
    let mut te = Document::new(Name::new("time_entry").unwrap(), te_id);
    let mut pairs = vec![
        ("entry_no", Value::Text("TE-2026.-.00001".to_owned())),
        ("task", Value::Id(task)),
        ("person", Value::Id(fresh(host, 1))),
        (
            "work_date",
            Value::Date(time::Date::from_ordinal_date(2026, 283).unwrap()),
        ),
    ];
    if let Some(h) = hours {
        pairs.push((TIME_ENTRY_HOURS, Value::Decimal(h)));
    }
    set(&mut te, &pairs);
    let saved = host.create(inv, te).await.unwrap().value;
    (te_id, saved)
}

/// 2.5 hours exactly.
fn h25() -> Decimal {
    Decimal::new(250, 2)
}

/// 24 hours exactly: the upper bound.
fn h24() -> Decimal {
    Decimal::new(2400, 2)
}

/// The stored version of a document, for the optimistic-update calls.
fn stored_version(host: &StubHost, doctype: &str, id: RecordId) -> spot_app_api::prelude::Version {
    host.stored(doctype, id)
        .unwrap_or_else(|| panic!("{doctype} {id} is stored"))
        .version()
        .unwrap_or_else(|| panic!("{doctype} {id} has a version"))
}

#[tokio::test]
async fn submit_adds_hours_to_the_task() {
    let host = host_with_app();
    let inv = inv();
    let project = seed_project(&host, &inv).await;
    let task = seed_task(&host, &inv, project, None).await;
    let (te_id, _) = seed_time_entry(&host, &inv, task, Some(h25())).await;

    let posted = host
        .post(
            &inv,
            "time_entry",
            te_id,
            stored_version(&host, "time_entry", te_id),
        )
        .await
        .unwrap();
    assert_eq!(posted.value.state(), Some(DocState::Posted));

    let task_doc = host.load(&inv, "task", task).await.unwrap().value;
    assert_eq!(
        task_doc.get(TASK_LOGGED_HOURS),
        &Value::Decimal(h25()),
        "submitting 2.50 h rolls the task up to 2.50 h"
    );
}

#[tokio::test]
async fn twenty_four_hours_post_and_two_entries_roll_up_to_sum() {
    // The upper bound is inclusive, and the roll-up adds across entries.
    let host = host_with_app();
    let inv = inv();
    let project = seed_project(&host, &inv).await;
    let task = seed_task(&host, &inv, project, None).await;
    let (one, _) = seed_time_entry(&host, &inv, task, Some(h24())).await;
    host.post(
        &inv,
        "time_entry",
        one,
        stored_version(&host, "time_entry", one),
    )
    .await
    .unwrap();
    // A second entry of the same task, same person, later day.
    let te_id = fresh(&host, 6);
    let mut te = Document::new(Name::new("time_entry").unwrap(), te_id);
    set(
        &mut te,
        &[
            ("entry_no", Value::Text("TE-2026.-.00002".to_owned())),
            ("task", Value::Id(task)),
            ("person", Value::Id(fresh(&host, 1))),
            (
                "work_date",
                Value::Date(time::Date::from_ordinal_date(2026, 284).unwrap()),
            ),
            (TIME_ENTRY_HOURS, Value::Decimal(h25())),
        ],
    );
    host.create(&inv, te).await.unwrap();
    host.post(
        &inv,
        "time_entry",
        te_id,
        stored_version(&host, "time_entry", te_id),
    )
    .await
    .unwrap();

    let task_doc = host.load(&inv, "task", task).await.unwrap().value;
    assert_eq!(
        task_doc.get(TASK_LOGGED_HOURS),
        &Value::Decimal(h24() + h25()),
        "the two submitted entries sum on the task"
    );
}

#[tokio::test]
async fn cancel_subtracts_the_hours_back() {
    let host = host_with_app();
    let inv = inv();
    let project = seed_project(&host, &inv).await;
    let task = seed_task(&host, &inv, project, None).await;
    let (te_id, _) = seed_time_entry(&host, &inv, task, Some(h25())).await;
    host.post(
        &inv,
        "time_entry",
        te_id,
        stored_version(&host, "time_entry", te_id),
    )
    .await
    .unwrap();
    host.cancel(
        &inv,
        "time_entry",
        te_id,
        stored_version(&host, "time_entry", te_id),
    )
    .await
    .unwrap();

    let task_doc = host.load(&inv, "task", task).await.unwrap().value;
    assert_eq!(
        task_doc.get(TASK_LOGGED_HOURS),
        &Value::Decimal(Decimal::ZERO),
        "cancelling subtracts the hours back to zero"
    );
}

#[tokio::test]
async fn a_rejected_save_leaves_the_roll_up_unchanged() {
    let host = host_with_app();
    let inv = inv();
    let project = seed_project(&host, &inv).await;
    let task = seed_task(&host, &inv, project, Some(h25())).await;
    // 2.50 h already logged. A Time Entry pushed to 0.10 h must be refused, and
    // the refusal must leave the logged hours untouched.
    let (te_id, _) = seed_time_entry(&host, &inv, task, Some(h25())).await;
    let mut patch = spot_app_api::DocumentPatch::default();
    patch.fields.insert(
        Name::new(TIME_ENTRY_HOURS).unwrap(),
        Value::Decimal(Decimal::new(10, 2)),
    );
    let err = host
        .update(
            &inv,
            "time_entry",
            te_id,
            patch,
            stored_version(&host, "time_entry", te_id),
        )
        .await
        .unwrap_err();
    assert!(
        err.fields
            .iter()
            .any(|f| f.field.as_str() == TIME_ENTRY_HOURS
                && f.code == spot_sample_app::catalogue::HOURS_OUT_OF_RANGE),
        "the refusal names the hours field with the sample code: {err:?}"
    );
    // The write rolled back with the failure: the Time Entry keeps its saved
    // hours and the task's logged hours are exactly as before.
    let te_doc = host.stored("time_entry", te_id).unwrap();
    assert_eq!(te_doc.get(TIME_ENTRY_HOURS), &Value::Decimal(h25()));
    assert_eq!(te_doc.state(), Some(DocState::Draft));
    let task_doc = host.load(&inv, "task", task).await.unwrap().value;
    assert_eq!(task_doc.get(TASK_LOGGED_HOURS), &Value::Decimal(h25()));
}

#[tokio::test]
async fn zero_and_twenty_five_hours_are_refused() {
    let host = host_with_app();
    let inv = inv();
    let project = seed_project(&host, &inv).await;
    let task = seed_task(&host, &inv, project, None).await;

    // 0 h is refused (below the 0.25 lower bound): a valid draft pushed to 0.
    let (zero_id, _) = seed_time_entry(&host, &inv, task, Some(h25())).await;
    let mut zero = spot_app_api::DocumentPatch::default();
    zero.fields.insert(
        Name::new(TIME_ENTRY_HOURS).unwrap(),
        Value::Decimal(Decimal::ZERO),
    );
    let err = host
        .update(
            &inv,
            "time_entry",
            zero_id,
            zero,
            stored_version(&host, "time_entry", zero_id),
        )
        .await
        .unwrap_err();
    let on_hours = err
        .fields
        .iter()
        .find(|f| f.field.as_str() == TIME_ENTRY_HOURS)
        .unwrap_or_else(|| panic!("hours must be named: {err:?}"));
    assert_eq!(
        on_hours.code,
        spot_sample_app::catalogue::HOURS_OUT_OF_RANGE
    );

    // 25 h is refused (above the 24.00 upper bound): the same draft pushed high.
    let mut big = spot_app_api::DocumentPatch::default();
    big.fields.insert(
        Name::new(TIME_ENTRY_HOURS).unwrap(),
        Value::Decimal(Decimal::new(2500, 2)),
    );
    let err = host
        .update(
            &inv,
            "time_entry",
            zero_id,
            big,
            stored_version(&host, "time_entry", zero_id),
        )
        .await
        .unwrap_err();
    let on_hours = err
        .fields
        .iter()
        .find(|f| f.field.as_str() == TIME_ENTRY_HOURS)
        .unwrap_or_else(|| panic!("hours must be named: {err:?}"));
    assert_eq!(
        on_hours.code,
        spot_sample_app::catalogue::HOURS_OUT_OF_RANGE
    );
}

#[tokio::test]
async fn the_bounds_hold_on_draft_saves_too() {
    // The range check rides validate_save: a draft update outside the range is
    // refused, not only at post time.
    let host = host_with_app();
    let inv = inv();
    let project = seed_project(&host, &inv).await;
    let task = seed_task(&host, &inv, project, None).await;
    let (te_id, _) = seed_time_entry(&host, &inv, task, Some(h25())).await;
    let mut patch = spot_app_api::DocumentPatch::default();
    patch.fields.insert(
        Name::new(TIME_ENTRY_HOURS).unwrap(),
        Value::Decimal(Decimal::new(2401, 2)),
    );
    let err = host
        .update(
            &inv,
            "time_entry",
            te_id,
            patch,
            stored_version(&host, "time_entry", te_id),
        )
        .await
        .unwrap_err();
    assert!(
        err.fields
            .iter()
            .any(|f| f.field.as_str() == TIME_ENTRY_HOURS),
        "{err:?}"
    );
}

#[tokio::test]
async fn the_nested_write_shares_the_transaction() {
    // The roll-up write runs in the caller's transaction: a phase-3 failure after
    // the sample's step ran (a test controller's failing after_post step) rolls
    // back the whole transaction — the Time Entry stays a draft and the Task's
    // `logged_hours` keeps its pre-post value. The roll-up was a nested write in
    // the same transaction, so it rolled back with it.
    use spot_app_api::{
        AppError, AppResult, BoxFuture, Controller, ErrorClass, HookContext, HookPoint, Phase,
        StepDecl, StepName, Target,
    };
    use std::time::Duration;

    /// A test controller that fails the `after_post` point after the sample ran.
    struct FailAfterPost;
    impl Controller for FailAfterPost {
        fn doctype(&self) -> &str {
            "time_entry"
        }
        fn steps(&self) -> Vec<StepDecl> {
            vec![StepDecl {
                // Higher priority than the sample's 50: it runs after the roll-up.
                name: StepName::new("test:fail_after").unwrap(),
                point: HookPoint::of(Phase::After, Target::Op(Operation::Post)),
                priority: 60,
                budget: Duration::from_millis(200),
            }]
        }
        fn run_step<'a>(
            &'a self,
            _step: &'a StepName,
            _ctx: &'a mut dyn HookContext,
        ) -> BoxFuture<'a, AppResult<()>> {
            Box::pin(async {
                Err(AppError::new(
                    ErrorClass::Internal,
                    "test.stop_here",
                    "The test fails the transaction deliberately.",
                ))
            })
        }
    }

    let host = host_with_app();
    let inv = inv();
    host.register(Arc::new(FailAfterPost));
    let project = seed_project(&host, &inv).await;
    // The task carries 1.00 h from an earlier entry.
    let task = seed_task(&host, &inv, project, Some(Decimal::new(100, 2))).await;
    let (te_id, _) = seed_time_entry(&host, &inv, task, Some(h25())).await;
    let err = host
        .post(
            &inv,
            "time_entry",
            te_id,
            stored_version(&host, "time_entry", te_id),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, "test.stop_here", "{err:?}");
    // One transaction: both writes rolled back together.
    assert_eq!(
        host.stored("time_entry", te_id).unwrap().state(),
        Some(DocState::Draft)
    );
    let task_doc = host.load(&inv, "task", task).await.unwrap().value;
    assert_eq!(
        task_doc.get(TASK_LOGGED_HOURS),
        &Value::Decimal(Decimal::new(100, 2)),
        "the rolled-back roll-up left the logged hours untouched"
    );
}

#[tokio::test]
async fn the_task_owner_is_derived_from_the_project() {
    let host = host_with_app();
    let inv = inv();
    let project = seed_project(&host, &inv).await;
    // A task with no owner: the before-save step fills it from the project.
    let task_id = fresh(&host, 5);
    let mut task = Document::new(Name::new("task").unwrap(), task_id);
    set(
        &mut task,
        &[
            ("subject", Value::Text("Derive owner".to_owned())),
            ("project", Value::Id(project)),
            ("status", Value::Text("Open".to_owned())),
        ],
    );
    let outcome = host.create(&inv, task).await.unwrap();
    assert!(
        outcome
            .messages
            .iter()
            .any(|m| m.code == spot_sample_app::catalogue::OWNER_FROM_PROJECT),
        "the derivation reports the message: {:?}",
        outcome.messages
    );
    let stored = host.stored("task", task_id).unwrap();
    let project_doc = host.stored("project", project).unwrap();
    assert_eq!(
        stored.get(TASK_PROJECT_OWNER),
        project_doc.get(PROJECT_OWNER),
        "the task's project_owner equals the project owner"
    );

    // An owner the user set is never overwritten.
    let user2 = fresh(&host, 9);
    let mut user = Document::new(Name::new("user").unwrap(), user2);
    set(&mut user, &[("name", Value::Text("bilal".to_owned()))]);
    host.create(&inv, user).await.unwrap();
    let task_id2 = fresh(&host, 10);
    let mut task2 = Document::new(Name::new("task").unwrap(), task_id2);
    set(
        &mut task2,
        &[
            ("subject", Value::Text("Own owner".to_owned())),
            ("project", Value::Id(project)),
            ("status", Value::Text("Open".to_owned())),
            (TASK_PROJECT_OWNER, Value::Id(user2)),
        ],
    );
    host.create(&inv, task2).await.unwrap();
    let stored2 = host.stored("task", task_id2).unwrap();
    assert_eq!(stored2.get(TASK_PROJECT_OWNER), &Value::Id(user2));
}

/// The controllers declare exactly the steps the manifest names, at the right
/// points, and the crate's registration shape is what the brief asks for.
#[test]
fn the_controllers_declare_the_expected_steps() {
    let steps: Vec<(String, String)> = TimeEntryController
        .steps()
        .into_iter()
        .map(|s| (s.name.as_str().to_owned(), s.point.name()))
        .collect();
    assert_eq!(
        steps,
        vec![
            ("sample:check_hours".to_owned(), "validate_save".to_owned()),
            (
                "sample:roll_up_hours_add".to_owned(),
                "after_post".to_owned()
            ),
            (
                "sample:roll_up_hours_subtract".to_owned(),
                "after_cancel".to_owned()
            ),
        ]
    );
    assert_eq!(
        TaskController
            .steps()
            .into_iter()
            .map(|s| (s.name.as_str().to_owned(), s.point.name()))
            .collect::<Vec<_>>(),
        vec![(
            "sample:derive_project_owner".to_owned(),
            "before_save".to_owned()
        )],
    );
    assert_eq!(
        spot_app_api::Controller::doctype(&TimeEntryController),
        "time_entry"
    );
    assert_eq!(spot_app_api::Controller::doctype(&TaskController), "task");
    assert!(spot_sample_app::controller::method_decls().is_empty());
    assert_eq!(
        spot_sample_app::NATIVE_CONTROLLERS,
        &[("sample", "time_entry"), ("sample.task", "task")]
    );
    assert_eq!(spot_sample_app::controllers().len(), 2);
    assert!(
        Operation::Post == Operation::Post,
        "the operation re-export is reachable"
    );
}
