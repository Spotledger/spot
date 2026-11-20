//! A storage failure during a run is reported honestly (WP-PLT-14.T6): a timeout is a timeout,
//! a database that cannot serve the query is `server.unavailable`, a query the storage layer
//! refuses is an invalid definition with the stable storage code, and anything else is an
//! internal error with a fixed text. No storage text, SQL fragment, table or column name
//! reaches the client.
//!
//! The executor is a fake that fails once with a chosen [`StorageError`] and then answers from
//! the fake backend, so the same tests prove that a failed run does not poison the runner.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::{Arc, Mutex};

use spot_reports::runner::map_storage_error;
use spot_reports::{BoxFuture, NoTelemetry, QueryExecutor, ReportArgs, ReportRunner, codes};
use spot_storage_contract::{ErrorKind, Query, Rows, StorageError};
use support::{FakeExecutor, JOURNAL_LIST, compile, n, principals, world};

/// Plants text no client may ever see.
const SECRET_TEXT: &str = "SELECT secret_col FROM tbl_users_secret WHERE pw = 'hunter2'";

/// Fails once with `failure`, then answers from the real fake backend.
struct FailsOnce {
    failure: Mutex<Option<StorageError>>,
    inner: FakeExecutor,
}

impl QueryExecutor for FailsOnce {
    fn execute<'a>(&'a self, query: &'a Query) -> BoxFuture<'a, Result<Rows, StorageError>> {
        Box::pin(async move {
            let failure = self.failure.lock().unwrap().take();
            match failure {
                Some(error) => Err(error),
                None => self.inner.execute(query).await,
            }
        })
    }
}

fn planted(kind: ErrorKind) -> StorageError {
    StorageError::new(kind, SECRET_TEXT)
        .with_entity(&n("tbl_users_secret"))
        .with_field(&n("secret_col"))
        .with_constraint(&n("idx_users_secret"))
}

/// Every storage kind and the code a report run answers it with. A kind missing from this table
/// is not covered: add it here when the storage contract grows one.
fn table() -> Vec<(ErrorKind, &'static str)> {
    use ErrorKind::*;
    vec![
        (StatementTimeout, codes::TIMEOUT),
        (Unavailable, codes::UNAVAILABLE),
        (LockTimeout, codes::UNAVAILABLE),
        (LockNotAvailable, codes::UNAVAILABLE),
        (SerializationFailure, codes::UNAVAILABLE),
        (Deadlock, codes::UNAVAILABLE),
        (InvalidQuery, codes::INVALID_DEFINITION),
        (UnknownField, codes::INVALID_DEFINITION),
        (UnknownEntity, codes::INVALID_DEFINITION),
        (NotFound, codes::FAILED),
        (AlreadyExists, codes::FAILED),
        (StaleVersion, codes::FAILED),
        (UniqueViolation, codes::FAILED),
        (ExclusionViolation, codes::FAILED),
        (ReferenceViolation, codes::FAILED),
        (InvalidValue, codes::FAILED),
        (InvalidName, codes::FAILED),
        (InvalidSchema, codes::FAILED),
        (TransactionTooLarge, codes::FAILED),
        (ReadOnlyTransaction, codes::FAILED),
        (TraversalTooLarge, codes::FAILED),
        (ResyncRequired, codes::FAILED),
        (Unsupported, codes::FAILED),
        (CommitOutcomeUnknown, codes::FAILED),
        (Internal, codes::FAILED),
    ]
}

#[test]
fn every_storage_kind_maps_as_listed_and_keeps_its_storage_code_for_the_log() {
    for (kind, expected) in table() {
        let mapped = map_storage_error(planted(kind));
        assert_eq!(mapped.code(), expected, "{kind:?}");
        assert_eq!(mapped.cause(), Some(kind.code()), "{kind:?}");
    }
}

#[test]
fn the_table_covers_every_distinct_kind_once() {
    let mut kinds: Vec<&str> = table().iter().map(|(k, _)| k.code()).collect();
    let all = kinds.len();
    kinds.sort_unstable();
    kinds.dedup();
    assert_eq!(kinds.len(), all, "a kind is listed twice");
    assert_eq!(
        all, 25,
        "the storage contract has 25 kinds: classify a new one here"
    );
}

#[test]
fn no_storage_text_reaches_the_client_in_any_mapped_message() {
    for (kind, _) in table() {
        let mapped = map_storage_error(planted(kind));
        let api = mapped.to_api_error();
        let body = api.to_envelope_json("req-1").to_string();
        for leak in [
            SECRET_TEXT,
            "secret_col",
            "tbl_users_secret",
            "idx_users_secret",
            "hunter2",
            "SELECT",
        ] {
            assert!(!body.contains(leak), "{kind:?} leaked {leak:?} in {body}");
            assert!(
                !mapped.message().contains(leak),
                "{kind:?} leaked {leak:?} in the message"
            );
        }
    }
}

#[test]
fn the_client_sees_the_contract_code_for_an_unavailable_database() {
    let api = map_storage_error(planted(ErrorKind::LockTimeout)).to_api_error();
    assert_eq!(api.code.as_str(), "server.unavailable");
    assert!(api.retryable());
    assert_eq!(
        api.details.get("retry_after"),
        Some(&serde_json::Value::from(5))
    );
}

#[test]
fn a_refused_query_names_only_the_stable_storage_code() {
    let mapped = map_storage_error(planted(ErrorKind::InvalidQuery));
    assert_eq!(mapped.code(), codes::INVALID_DEFINITION);
    assert!(
        mapped.message().contains("storage.invalid_query"),
        "{}",
        mapped.message()
    );
    assert!(mapped.message().contains("refused by the storage layer"));
}

struct Fixture {
    runner: Arc<ReportRunner>,
    report: spot_reports::CompiledReport,
    alice: spot_permissions::Principal,
}

impl Fixture {
    async fn new(failure: Option<StorageError>) -> Self {
        let world = world().await;
        let report = compile(&world, JOURNAL_LIST).expect("compiles");
        let alice = principals()
            .into_iter()
            .find(|(name, _)| *name == "alice")
            .expect("alice")
            .1;
        let executor = Arc::new(FailsOnce {
            failure: Mutex::new(failure),
            inner: FakeExecutor {
                backend: world.backend.clone(),
                catalog: world.catalog.clone(),
            },
        });
        let runner = Arc::new(ReportRunner::new(
            world.engine.clone(),
            world.guard.clone(),
            executor,
            Arc::new(NoTelemetry),
        ));
        Self {
            runner,
            report,
            alice,
        }
    }

    async fn run(&self) -> Result<spot_reports::RunOutput, spot_reports::ReportError> {
        self.runner
            .run(&self.report, &self.alice, &ReportArgs::empty(), None)
            .await
    }
}

#[tokio::test]
async fn a_statement_timeout_through_the_runner_is_report_timeout_not_an_invalid_definition() {
    let f = Fixture::new(Some(planted(ErrorKind::StatementTimeout))).await;
    let error = f.run().await.expect_err("the run fails");
    assert_eq!(error.code(), codes::TIMEOUT);
    assert_ne!(error.code(), codes::INVALID_DEFINITION);
    assert!(!error.message().contains("SELECT"));
    assert_eq!(error.cause(), Some("storage.statement_timeout"));
}

#[tokio::test]
async fn a_timeout_does_not_poison_the_runner_the_next_run_works() {
    let f = Fixture::new(Some(planted(ErrorKind::StatementTimeout))).await;
    assert!(f.run().await.is_err());
    let second = f.run().await.expect("the next run succeeds");
    assert!(!second.result.rows.is_empty());
    // And a third, so a cached state is not what made the second pass.
    let third = f.run().await.expect("and the one after");
    assert_eq!(second.result.rows.len(), third.result.rows.len());
}

#[tokio::test]
async fn a_successful_run_is_unchanged() {
    let f = Fixture::new(None).await;
    assert!(!f.run().await.expect("runs").result.rows.is_empty());
}
