//! The import service core (WP-DOC-09.T5): the two-phase flow, digest, limits, rights, creator
//! only access, idempotency, cancel, resume after a crash and audit, over a scripted host and
//! the in-memory record store. The production host (files service, `spot-import`, PostgreSQL) is
//! exercised by the l3-http case `harness/rust/l3-http/src/cases/import.rs`.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use spot_cli::services::import::flow::{
    AuditEvent, Caller, EngineOutcome, EngineRequest, ImportAudit, ImportCore, ImportHost, Limits,
    Refusal, RunControl, StagedFile,
};
use spot_cli::services::import::records::{JobRecord, JobRecords, MemoryRecords, StoredError};
use spot_core::{ManualClock, SiteId};
use spot_http::api::ports::import::{ImportCreateIn, ImportModeIn, ImportPort, ImportStateOut};
use spot_http::api::registry::Code;
use spot_http::api::service::{ApiError, BoxFuture, Credential, ErrorCode, RequestCtx};
use spot_http::request::RequestId;

const SITE: &str = "acme";
const ALICE: &str = "0192f3c4-0000-7000-8000-00000000a001";
const BOB: &str = "0192f3c4-0000-7000-8000-00000000b001";
const FILE: &str = "0192f3c4-0000-7000-8000-00000000f001";

#[derive(Default)]
struct Script {
    /// Users that hold the import right on `customer`.
    importers: HashSet<String>,
    submitters: HashSet<String>,
    /// Users no longer active.
    inactive: HashSet<String>,
    /// file id to (owner, rows).
    files: HashMap<String, (String, u64)>,
    spool: HashSet<String>,
    runs: u64,
    /// When set, `run` waits until cancelled.
    wait_for_cancel: bool,
    dry_errors: Vec<StoredError>,
}

struct Host {
    script: Mutex<Script>,
}

impl Host {
    fn lock(&self) -> std::sync::MutexGuard<'_, Script> {
        self.script.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl ImportHost for Host {
    fn caller<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, Result<Caller, ApiError>> {
        Box::pin(async move {
            match ctx.credential.as_ref().map(|c| c.token.as_str()) {
                Some("alice") => Ok(Caller {
                    user: ALICE.to_owned(),
                }),
                Some("bob") => Ok(Caller {
                    user: BOB.to_owned(),
                }),
                _ => Err(ApiError::from_code(Code::AuthRequired)),
            }
        })
    }

    fn active<'a>(&'a self, _: &'a str, user: &'a str) -> BoxFuture<'a, bool> {
        Box::pin(async move { !self.lock().inactive.contains(user) })
    }

    fn may_import<'a>(
        &'a self,
        _: &'a str,
        user: &'a str,
        doctype: &'a str,
        submit: bool,
    ) -> BoxFuture<'a, Result<(), Refusal>> {
        Box::pin(async move {
            let script = self.lock();
            if doctype == "customer"
                && script.importers.contains(user)
                && (!submit || script.submitters.contains(user))
            {
                Ok(())
            } else {
                Err(Refusal::RightMissing)
            }
        })
    }

    fn stage<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        file_id: &'a str,
        job_id: &'a str,
        _: u64,
    ) -> BoxFuture<'a, Result<StagedFile, Refusal>> {
        Box::pin(async move {
            let user = self.caller(ctx).await.map_err(|_| Refusal::NotFound)?.user;
            let mut script = self.lock();
            match script.files.get(file_id) {
                Some((owner, rows)) if *owner == user => {
                    let rows = *rows;
                    script.spool.insert(job_id.to_owned());
                    Ok(StagedFile {
                        path: format!("/spool/{job_id}").into(),
                        bytes: rows * 10,
                        digest: "d".repeat(64),
                        owned: false,
                    })
                }
                _ => Err(Refusal::NotFound),
            }
        })
    }

    fn spool<'a>(&'a self, job_id: &'a str) -> BoxFuture<'a, Option<StagedFile>> {
        Box::pin(async move {
            self.lock().spool.contains(job_id).then(|| StagedFile {
                path: format!("/spool/{job_id}").into(),
                bytes: 0,
                digest: "d".repeat(64),
                owned: false,
            })
        })
    }

    fn discard<'a>(&'a self, job_id: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            self.lock().spool.remove(job_id);
        })
    }

    fn dry_run<'a>(
        &'a self,
        request: &'a EngineRequest<'a>,
    ) -> BoxFuture<'a, Result<EngineOutcome, Refusal>> {
        Box::pin(async move {
            let script = self.lock();
            let rows = script.files.get(&request.job.file_id).map_or(0, |f| f.1);
            let errors = script.dry_errors.clone();
            Ok(EngineOutcome {
                rows,
                ok: rows - errors.len() as u64,
                skipped: 0,
                errors,
                cancelled: false,
            })
        })
    }

    fn run<'a>(
        &'a self,
        request: &'a EngineRequest<'a>,
        control: &'a dyn RunControl,
    ) -> BoxFuture<'a, Result<EngineOutcome, Refusal>> {
        Box::pin(async move {
            let (rows, wait) = {
                let mut script = self.lock();
                script.runs += 1;
                (
                    script.files.get(&request.job.file_id).map_or(0, |f| f.1),
                    script.wait_for_cancel,
                )
            };
            if wait {
                for _ in 0..200 {
                    if control.cancelled() {
                        return Ok(EngineOutcome {
                            rows,
                            ok: 1,
                            skipped: rows - 1,
                            errors: vec![],
                            cancelled: true,
                        });
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
            control.progress(rows, Some(rows));
            Ok(EngineOutcome {
                rows,
                ok: rows,
                skipped: 0,
                errors: vec![],
                cancelled: false,
            })
        })
    }

    fn cells<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: &'a JobRecord,
        wanted: &'a [(u64, String)],
    ) -> BoxFuture<'a, Vec<(u64, String, String)>> {
        Box::pin(async move {
            // The cell of row 3 belongs to a field the caller may not write: absent.
            wanted
                .iter()
                .filter(|(row, _)| *row != 3)
                .map(|(row, column)| (*row, column.clone(), "x".repeat(600)))
                .collect()
        })
    }

    fn meta_version(&self) -> u64 {
        7
    }
}

#[derive(Default)]
struct Audit(Mutex<Vec<AuditEvent>>);

impl ImportAudit for Audit {
    fn record(&self, event: &AuditEvent) {
        self.0.lock().unwrap().push(event.clone());
    }
}

struct Fixture {
    core: Arc<ImportCore>,
    host: Arc<Host>,
    records: Arc<MemoryRecords>,
    audit: Arc<Audit>,
    clock: Arc<ManualClock>,
}

fn fixture_with(limits: Limits) -> Fixture {
    let host = Arc::new(Host {
        script: Mutex::new(Script::default()),
    });
    {
        let mut script = host.lock();
        script.importers.insert(ALICE.to_owned());
        script.importers.insert(BOB.to_owned());
        script.files.insert(FILE.to_owned(), (ALICE.to_owned(), 5));
        script.dry_errors = vec![
            StoredError {
                row: 2,
                column: Some("name".into()),
                code: "import.bad_value".into(),
                field: Some("name".into()),
            },
            StoredError {
                row: 3,
                column: Some("secret".into()),
                code: "import.bad_value".into(),
                field: Some("secret".into()),
            },
        ];
    }
    let records = Arc::new(MemoryRecords::new());
    let audit = Arc::new(Audit::default());
    let clock = Arc::new(ManualClock::new(
        time::macros::datetime!(2030-01-01 08:00:00 UTC),
    ));
    let core = ImportCore::new(
        SITE,
        host.clone(),
        records.clone(),
        audit.clone(),
        clock.clone(),
        limits,
        "worker-1",
    );
    Fixture {
        core,
        host,
        records,
        audit,
        clock,
    }
}

fn fixture() -> Fixture {
    fixture_with(Limits::default())
}

fn ctx(token: &str, key: Option<&str>) -> RequestCtx {
    RequestCtx {
        site: SiteId::new(SITE).unwrap(),
        credential: Some(Credential::new(token)),
        idempotency_key: key.map(str::to_owned),
        language: None,
        request_id: RequestId::new("req-1").unwrap(),
        peer: None,
    }
}

fn request() -> ImportCreateIn {
    ImportCreateIn {
        file_id: FILE.to_owned(),
        doctype: "customer".to_owned(),
        mode: ImportModeIn::Insert,
        mapping: None,
        submit: false,
        dry_run: true,
    }
}

fn code(error: &ApiError) -> Code {
    match &error.code {
        ErrorCode::Registry(code) => *code,
        ErrorCode::App(text) => panic!("app code {text}"),
    }
}

async fn wait_for(f: &Fixture, id: &str, state: ImportStateOut) {
    for _ in 0..300 {
        f.core.pump().await;
        let job = f.core.get(&ctx("alice", None), id).await.unwrap();
        if job.state == state {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the job never reached {state:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_runs_the_dry_run_and_leaves_a_ready_job_with_a_digest_and_no_business_write() {
    let f = fixture();
    let job = f.core.create(&ctx("alice", None), request()).await.unwrap();
    assert_eq!(job.state, ImportStateOut::Ready);
    assert_eq!(job.counts.rows, 5);
    assert_eq!(job.counts.errors, 2);
    assert_eq!(job.counts.ok, 3);
    assert_eq!(job.digest.as_ref().unwrap().len(), 64);
    assert_eq!(f.host.lock().runs, 0, "a dry run writes nothing");
    // The digest changes with the mapping.
    let mut other = request();
    other.submit = false;
    other.mapping = Some(spot_http::api::ports::import::ImportMappingIn {
        columns: vec![("Name".into(), "name".into())],
        child_tables: Default::default(),
    });
    let second = f.core.create(&ctx("alice", None), other).await.unwrap();
    assert_ne!(job.digest, second.digest);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_rights_files_and_limits_are_checked_before_work_and_every_refusal_is_audited() {
    let f = fixture();
    // No import right on the DocType.
    let mut wrong = request();
    wrong.doctype = "invoice".to_owned();
    let error = f.core.create(&ctx("alice", None), wrong).await.unwrap_err();
    assert_eq!(code(&error), Code::ImportRightMissing);
    // Submit without the submit right.
    let mut submit = request();
    submit.submit = true;
    let error = f
        .core
        .create(&ctx("alice", None), submit)
        .await
        .unwrap_err();
    assert_eq!(code(&error), Code::ImportRightMissing);
    // A file that is not the caller's is the one 404.
    let error = f
        .core
        .create(&ctx("bob", None), request())
        .await
        .unwrap_err();
    assert_eq!(
        code(&error),
        Code::ImportLimitExceeded
            .min(Code::DocumentNotFound)
            .max(Code::DocumentNotFound)
    );
    // A run without a dry run.
    let mut run = request();
    run.dry_run = false;
    let error = f.core.create(&ctx("alice", None), run).await.unwrap_err();
    assert_eq!(code(&error), Code::ImportNotReady);
    let events = f.audit.0.lock().unwrap().clone();
    assert!(
        events.iter().filter(|e| e.action == "refused").count() >= 4,
        "{events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| e.outcome == "right_missing" && e.user == ALICE)
    );
    // Concurrency: with one slot and one job working, a second create is refused.
    let tight = fixture_with(Limits {
        max_concurrent_per_site: 1,
        ..Limits::default()
    });
    tight.host.lock().importers.insert(ALICE.to_owned());
    tight
        .host
        .lock()
        .files
        .insert(FILE.to_owned(), (ALICE.to_owned(), 5));
    let first = tight
        .core
        .create(&ctx("alice", None), request())
        .await
        .unwrap();
    let digest = first.digest.unwrap();
    tight
        .core
        .run(&ctx("alice", None), &first.id, &digest)
        .await
        .unwrap();
    let error = tight
        .core
        .create(&ctx("alice", None), request())
        .await
        .unwrap_err();
    assert_eq!(code(&error), Code::ImportLimitExceeded);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_job_is_its_creators_and_everyone_elses_answer_is_the_same_404() {
    let f = fixture();
    let job = f.core.create(&ctx("alice", None), request()).await.unwrap();
    let missing = "0192f3c4-0000-7000-8000-0000000fffff";
    for id in [job.id.as_str(), missing] {
        let get = f.core.get(&ctx("bob", None), id).await.unwrap_err();
        let errors = f
            .core
            .errors(&ctx("bob", None), id, None, 10)
            .await
            .unwrap_err();
        let run = f
            .core
            .run(&ctx("bob", None), id, &"0".repeat(64))
            .await
            .unwrap_err();
        let cancel = f.core.cancel(&ctx("bob", None), id).await.unwrap_err();
        for error in [get, errors, run, cancel] {
            assert_eq!(code(&error), Code::DocumentNotFound);
        }
    }
    // A job of another site is not found through this site's core.
    let other = ImportCore::new(
        "other",
        f.host.clone(),
        f.records.clone(),
        f.audit.clone(),
        f.clock.clone(),
        Limits::default(),
        "w2",
    );
    let error = other.get(&ctx("alice", None), &job.id).await.unwrap_err();
    assert_eq!(code(&error), Code::DocumentNotFound);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn errors_page_cut_the_cell_and_hide_a_cell_the_caller_may_not_write() {
    let f = fixture();
    let job = f.core.create(&ctx("alice", None), request()).await.unwrap();
    let page = f
        .core
        .errors(&ctx("alice", None), &job.id, None, 1)
        .await
        .unwrap();
    assert_eq!(page.rows.len(), 1);
    assert_eq!(page.rows[0].cell_text.as_ref().unwrap().len(), 256);
    assert!(page.next_cursor.is_some());
    let second = f
        .core
        .errors(&ctx("alice", None), &job.id, page.next_cursor.as_deref(), 1)
        .await
        .unwrap();
    assert_eq!(second.rows[0].row, 3);
    assert!(
        second.rows[0].cell_text.is_none(),
        "the host did not release the cell"
    );
    assert!(second.next_cursor.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_needs_the_digest_then_writes_in_the_background_and_counts_are_final() {
    let f = fixture();
    let job = f.core.create(&ctx("alice", None), request()).await.unwrap();
    let wrong = f
        .core
        .run(&ctx("alice", None), &job.id, &"0".repeat(64))
        .await
        .unwrap_err();
    assert_eq!(code(&wrong), Code::ImportDigestChanged);
    let digest = job.digest.clone().unwrap();
    let started = f
        .core
        .run(&ctx("alice", Some("k-run")), &job.id, &digest)
        .await
        .unwrap();
    assert_eq!(started.state, ImportStateOut::Running);
    // The same key answers the first answer; the job is not started twice.
    let replay = f
        .core
        .run(&ctx("alice", Some("k-run")), &job.id, &digest)
        .await
        .unwrap();
    assert_eq!(replay.id, started.id);
    wait_for(&f, &job.id, ImportStateOut::Done).await;
    let done = f.core.get(&ctx("alice", None), &job.id).await.unwrap();
    assert_eq!(done.progress, 1000);
    assert_eq!(done.counts.rows, 5);
    assert_eq!(f.host.lock().runs, 1);
    assert!(
        !f.host.lock().spool.contains(&job.id),
        "the spool file is removed when the job ends"
    );
    let again = f
        .core
        .run(&ctx("alice", None), &job.id, &digest)
        .await
        .unwrap_err();
    assert_eq!(code(&again), Code::ImportNotReady);
    let events = f.audit.0.lock().unwrap().clone();
    for action in ["created", "dry_run", "run_started", "finished"] {
        assert!(events.iter().any(|e| e.action == action), "{action}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_is_idempotent_by_key_and_a_different_request_with_the_key_conflicts() {
    let f = fixture();
    let first = f
        .core
        .create(&ctx("alice", Some("k1")), request())
        .await
        .unwrap();
    let second = f
        .core
        .create(&ctx("alice", Some("k1")), request())
        .await
        .unwrap();
    assert_eq!(first.id, second.id);
    let mut other = request();
    other.mode = ImportModeIn::Upsert;
    let error = f
        .core
        .create(&ctx("alice", Some("k1")), other)
        .await
        .unwrap_err();
    assert_eq!(code(&error), Code::ApiIdempotencyConflict);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_before_the_run_discards_and_a_run_after_it_is_cancelled() {
    let f = fixture();
    let job = f.core.create(&ctx("alice", None), request()).await.unwrap();
    let cancelled = f.core.cancel(&ctx("alice", None), &job.id).await.unwrap();
    assert_eq!(cancelled.state, ImportStateOut::Cancelled);
    assert!(!f.host.lock().spool.contains(&job.id));
    let repeat = f.core.cancel(&ctx("alice", None), &job.id).await.unwrap();
    assert_eq!(repeat.state, ImportStateOut::Cancelled);
    let run = f
        .core
        .run(&ctx("alice", None), &job.id, job.digest.as_deref().unwrap())
        .await
        .unwrap_err();
    assert_eq!(code(&run), Code::ImportCancelled);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_while_running_stops_the_run_within_a_batch() {
    let f = fixture();
    f.host.lock().wait_for_cancel = true;
    let job = f.core.create(&ctx("alice", None), request()).await.unwrap();
    let digest = job.digest.clone().unwrap();
    f.core
        .run(&ctx("alice", None), &job.id, &digest)
        .await
        .unwrap();
    f.core.pump().await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let state = f.core.cancel(&ctx("alice", None), &job.id).await.unwrap();
    assert_eq!(
        state.state,
        ImportStateOut::Running,
        "the runner sets the final state"
    );
    wait_for(&f, &job.id, ImportStateOut::Cancelled).await;
    let end = f.core.get(&ctx("alice", None), &job.id).await.unwrap();
    assert_eq!(end.counts.ok, 1, "rows already written stay counted");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_user_revoked_after_the_dry_run_stops_with_right_missing() {
    let f = fixture();
    let job = f.core.create(&ctx("alice", None), request()).await.unwrap();
    let digest = job.digest.clone().unwrap();
    f.core
        .run(&ctx("alice", None), &job.id, &digest)
        .await
        .unwrap();
    f.host.lock().inactive.insert(ALICE.to_owned());
    f.core.pump().await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let failed = f.records.get(SITE, &job.id).await.unwrap().unwrap();
    assert_eq!(failed.state, ImportStateOut::Failed);
    assert_eq!(failed.failure.as_deref(), Some("right_missing"));
    assert_eq!(f.host.lock().runs, 0, "no row was written");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_running_job_with_an_ended_lease_is_taken_again_and_not_run_twice() {
    let f = fixture();
    let job = f.core.create(&ctx("alice", None), request()).await.unwrap();
    let digest = job.digest.clone().unwrap();
    f.core
        .run(&ctx("alice", None), &job.id, &digest)
        .await
        .unwrap();
    // A crashed worker's claim: running, with a lease that ended.
    let mut stale = f.records.get(SITE, &job.id).await.unwrap().unwrap();
    stale.lease = Some(("dead-worker".to_owned(), 1));
    f.records.put(stale).await.unwrap();
    assert_eq!(f.core.pump().await, 1, "the stale job is taken");
    wait_for(&f, &job.id, ImportStateOut::Done).await;
    assert_eq!(f.core.pump().await, 0, "a finished job is not taken again");
    // A live lease is not taken.
    let second = f.core.create(&ctx("alice", None), request()).await.unwrap();
    let digest = second.digest.clone().unwrap();
    f.core
        .run(&ctx("alice", None), &second.id, &digest)
        .await
        .unwrap();
    let mut live = f.records.get(SITE, &second.id).await.unwrap().unwrap();
    live.lease = Some(("other-node".to_owned(), 4_000_000_000));
    f.records.put(live).await.unwrap();
    assert_eq!(f.core.pump().await, 0);
}
