//! Report sources (WP-BPM-11.T3): a service registers a report over its own data and the one
//! report engine serves it like a shipped report.
//!
//! The tests drive the site's `SourceBook` (what the report service builds from the registry at the
//! start-up check) with a fake executor, so they need no database: the engine, the roles, the
//! compiler, the runner's permission and field-guard steps and the visibility hook are the real
//! ones. The hook's decisions are asserted through what the executor is handed and through the
//! answers (`404 report.not_found`, the one answer for a report that does not exist).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use serde_json::json;
use spot_cli::services::report::{
    ADMIT_TIMEOUT, Admission, AdmitRequest, Denied, ReportSource, ReportSources, SourceBook,
    SourceExecutor, SourceRegistry, SourceVisibility,
};
use spot_core::ManualClock;
use spot_meta::{Definition, FieldSpec, FieldType};
use spot_permissions::{AssignmentContext, Principal};
use spot_reports::{BoxFuture, ReportDef};
use spot_storage_contract::{Query, Rows, StorageError, Value};
use time::macros::datetime;

const BASE: &str = "insight_probe";

const DEF: &str = r#"
name = "probe_report"
label = "Probe"
base = "insight_probe"
roles = ["reader"]

[[columns]]
field = "memo"
"#;

/// Answers every query with one row per admitted scope key (two rows for `All`) and remembers
/// the admissions it was handed.
#[derive(Default)]
struct Rowset {
    seen: Mutex<Vec<Admission>>,
}

impl SourceExecutor for Rowset {
    fn execute<'a>(
        &'a self,
        admission: &'a Admission,
        query: &'a Query,
    ) -> BoxFuture<'a, Result<Rows, StorageError>> {
        self.seen.lock().unwrap().push(admission.clone());
        let count = match admission {
            Admission::All => 2,
            Admission::Rows(keys) => keys.len(),
        };
        let columns: Vec<_> = query.select.iter().map(|s| s.alias.clone()).collect();
        let row: Vec<Value> = columns
            .iter()
            .map(|_| Value::Text("note".to_owned()))
            .collect();
        Box::pin(async move {
            Ok(Rows {
                columns,
                rows: vec![row; count],
            })
        })
    }
}

/// Decides by the user's id.
struct ByUser;

impl SourceVisibility for ByUser {
    fn admit<'a>(&'a self, request: &'a AdmitRequest) -> BoxFuture<'a, Result<Admission, Denied>> {
        Box::pin(async move {
            match request.user.as_str() {
                "everyone" => Ok(Admission::All),
                "scoped" => Ok(Admission::Rows(BTreeSet::from(["k1".to_owned()]))),
                "denied" => Err(Denied("test.denied")),
                "panics" => panic!("the hook panics"),
                "hangs" => {
                    std::future::pending::<()>().await;
                    Ok(Admission::All)
                }
                _ => Err(Denied("test.unknown_user")),
            }
        })
    }
}

fn definition() -> Definition {
    let mut d = Definition::new(BASE);
    d.fields = vec![FieldSpec::new("memo", FieldType::Text)];
    d
}

fn source(service: &str, text: &str, executor: Arc<Rowset>) -> ReportSource {
    ReportSource {
        service: service.to_owned(),
        def: Arc::new(ReportDef::from_toml(text).unwrap()),
        schema: vec![definition()],
        executor,
        visibility: Arc::new(ByUser),
        required: false,
    }
}

fn clock() -> Arc<ManualClock> {
    Arc::new(ManualClock::new(datetime!(2026-11-12 00:00 UTC)))
}

fn who(user: &str, roles: &[&str]) -> AdmitRequest {
    AdmitRequest {
        site: "acme".to_owned(),
        user: user.to_owned(),
        principal: Principal::user(user, vec![AssignmentContext::unscoped("a1", roles)]),
    }
}

fn book_of(sources: Vec<ReportSource>) -> SourceBook {
    let registry = SourceRegistry::new();
    for s in sources {
        registry.register(s).unwrap();
    }
    SourceBook::compile(&registry, clock())
}

fn code_of<T>(answer: Result<T, spot_http::api::service::ApiError>) -> String {
    match answer {
        Ok(_) => "ok".to_owned(),
        Err(e) => e.code.as_str().to_owned(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_toy_source_is_listed_described_and_run_by_an_admitted_caller() {
    let executor = Arc::new(Rowset::default());
    let book = book_of(vec![source("toy", DEF, Arc::clone(&executor))]);
    assert_eq!(
        book.served(),
        vec![("probe_report".to_owned(), "toy".to_owned())]
    );
    assert!(book.fatal().is_empty());

    let caller = who("everyone", &["reader"]);
    let listed = book.list(&caller).await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].name, "probe_report");
    assert_eq!(listed[0].app, "toy");
    assert_eq!(listed[0].base_doctype, BASE);

    let described = book.describe(&caller, "probe_report").await.unwrap();
    assert_eq!(described.name, "probe_report");
    assert!(described.columns.iter().any(|c| c.name == "memo"));

    let (result, _) = book
        .run(&caller, "probe_report", &json!({}), None)
        .await
        .unwrap();
    assert_eq!(result.rows.len(), 2);
    assert!(executor.seen.lock().unwrap().contains(&Admission::All));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_admission_the_hook_returns_reaches_the_executor_as_a_row_filter() {
    let executor = Arc::new(Rowset::default());
    let book = book_of(vec![source("toy", DEF, Arc::clone(&executor))]);
    let (result, _) = book
        .run(
            &who("scoped", &["reader"]),
            "probe_report",
            &json!({}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(result.rows.len(), 1, "one admitted scope key, one row");
    assert!(
        executor
            .seen
            .lock()
            .unwrap()
            .contains(&Admission::Rows(BTreeSet::from(["k1".to_owned()])))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_denied_caller_gets_not_found_and_the_report_is_absent_not_empty() {
    let executor = Arc::new(Rowset::default());
    let book = book_of(vec![source("toy", DEF, Arc::clone(&executor))]);
    let missing = code_of(
        book.run(
            &who("everyone", &["reader"]),
            "no_such_report",
            &json!({}),
            None,
        )
        .await,
    );
    assert_eq!(missing, "report.not_found");

    let denied = who("denied", &["reader"]);
    assert_eq!(
        code_of(book.run(&denied, "probe_report", &json!({}), None).await),
        missing,
        "the same answer as a report that does not exist"
    );
    assert_eq!(
        code_of(book.describe(&denied, "probe_report").await),
        missing
    );
    assert!(book.list(&denied).await.is_empty(), "absent, not zero rows");
    assert!(
        executor.seen.lock().unwrap().is_empty(),
        "a denied caller never reaches the executor"
    );

    // A caller whose roles do not admit the report is refused before the hook is asked.
    let other = who("everyone", &["clerk"]);
    assert_eq!(
        code_of(book.run(&other, "probe_report", &json!({}), None).await),
        missing
    );
    assert!(book.list(&other).await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hook_that_fails_denies() {
    let book = book_of(vec![source("toy", DEF, Arc::new(Rowset::default()))]);
    // The hook answers an error for an id it does not know: the run is denied.
    let unknown = who("stranger", &["reader"]);
    assert_eq!(
        code_of(book.run(&unknown, "probe_report", &json!({}), None).await),
        "report.not_found"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hook_that_panics_denies() {
    let executor = Arc::new(Rowset::default());
    let book = book_of(vec![source("toy", DEF, Arc::clone(&executor))]);
    let caller = who("panics", &["reader"]);
    assert_eq!(
        code_of(book.run(&caller, "probe_report", &json!({}), None).await),
        "report.not_found"
    );
    assert!(book.list(&caller).await.is_empty());
    assert!(executor.seen.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hook_that_hangs_is_abandoned_at_the_timeout_and_denies() {
    let book = book_of(vec![source("toy", DEF, Arc::new(Rowset::default()))]);
    let started = std::time::Instant::now();
    let answer = book
        .run(&who("hangs", &["reader"]), "probe_report", &json!({}), None)
        .await;
    assert_eq!(code_of(answer), "report.not_found");
    assert!(started.elapsed() >= ADMIT_TIMEOUT);
}

#[test]
fn a_report_name_taken_by_another_source_is_refused_naming_both_services() {
    let registry = SourceRegistry::new();
    registry
        .register(source("first", DEF, Arc::new(Rowset::default())))
        .unwrap();
    let error = registry
        .register(source("second", DEF, Arc::new(Rowset::default())))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("first") && error.contains("second"),
        "{error}"
    );
    assert_eq!(registry.names(), vec!["probe_report".to_owned()]);
}

#[test]
fn malformed_names_and_a_late_registration_are_refused() {
    let registry = SourceRegistry::new();
    assert!(
        registry
            .register(source("Bad Service", DEF, Arc::new(Rowset::default())))
            .is_err()
    );
    let _book = SourceBook::compile(&registry, clock());
    assert!(
        registry
            .register(source("toy", DEF, Arc::new(Rowset::default())))
            .is_err(),
        "the registry closes at the start-up check"
    );
}

#[test]
fn a_source_that_does_not_compile_is_left_out_unless_it_is_required() {
    let broken = DEF.replace("\"memo\"", "\"missing\"");
    let book = book_of(vec![source("toy", &broken, Arc::new(Rowset::default()))]);
    assert!(book.served().is_empty());
    assert!(
        book.fatal().is_empty(),
        "named in the log, the site still starts"
    );

    let mut required = source("toy", &broken, Arc::new(Rowset::default()));
    required.required = true;
    let book = book_of(vec![required]);
    assert!(book.served().is_empty());
    assert!(!book.fatal().is_empty());
    assert!(
        book.fatal()[0].contains("probe_report"),
        "{:?}",
        book.fatal()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_source_of_site_a_is_unreachable_from_site_b() {
    let a = book_of(vec![source("toy", DEF, Arc::new(Rowset::default()))]);
    let b = book_of(Vec::new());
    let caller = who("everyone", &["reader"]);
    assert_eq!(a.list(&caller).await.len(), 1);

    assert!(b.list(&caller).await.is_empty(), "not by list");
    assert_eq!(
        code_of(b.run(&caller, "probe_report", &json!({}), None).await),
        "report.not_found",
        "not by name"
    );
    assert_eq!(
        code_of(b.describe(&caller, "probe_report").await),
        "report.not_found"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_source_schema_is_not_a_doctype_of_anything_else() {
    // The schema lives in the source's own compiled form: a second source with the same base
    // name but another schema compiles against its own, not against the first one's.
    let mut wide = definition();
    wide.fields.push(FieldSpec::new("extra", FieldType::Text));
    let second = DEF
        .replace("probe_report", "second_report")
        .replace("\"memo\"", "\"extra\"");
    let mut s2 = source("other", &second, Arc::new(Rowset::default()));
    s2.schema = vec![wide];
    let first = source("toy", DEF, Arc::new(Rowset::default()));
    let book = book_of(vec![first, s2]);
    assert_eq!(book.served().len(), 2);
    let caller = who("everyone", &["reader"]);
    assert!(
        book.run(&caller, "second_report", &json!({}), None)
            .await
            .is_ok()
    );
}
