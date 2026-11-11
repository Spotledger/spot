use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;
use std::time::Duration;

use spot_core::SiteId;
use spot_jobs::model::{Job, JobId, JobState};
use spot_jobs::worker::{HandlerResult, JobContext, JobHandlerRegistry};
use time::OffsetDateTime;

use super::fixture::{Journal, engine, runtime};
use crate::memory::{FnInputs, MemoryBook, SharedClock};
use crate::{AuditJob, Cadence, JOB_NAME, TenantOutcome, handler};

static THREAD: Mutex<Option<ThreadId>> = Mutex::new(None);

fn context(at: i64) -> JobContext {
    let now = OffsetDateTime::from_unix_timestamp(at).unwrap();
    let job = Job {
        id: JobId(1),
        site: SiteId::new("test-site").unwrap(),
        name: JOB_NAME.to_owned(),
        args: serde_json::Value::Null,
        queue: "default".to_owned(),
        state: JobState::Pending,
        attempt: 1,
        max_attempts: 3,
        not_before: now,
        idempotency_key: None,
        claim: None,
        last_error: None,
        created_at: now,
        context: serde_json::Value::Null,
    };
    JobContext::new(job, 1, now)
}

fn job(
    rt: &tokio::runtime::Runtime,
    book: &Arc<MemoryBook>,
) -> Arc<AuditJob<spot_conditions::MemorySink>> {
    let journal = Arc::new(Journal::new(rt, "acme", 5));
    let inputs = FnInputs::new(move |_| {
        // A slow provider: long enough for the async side to tick several times.
        *THREAD.lock().unwrap() = Some(std::thread::current().id());
        std::thread::sleep(Duration::from_millis(100));
        Ok(journal.inputs(journal.live(), false))
    });
    Arc::new(AuditJob::new(
        book.clone(),
        Arc::new(inputs),
        book.clone(),
        Arc::new(SharedClock::new(0, 1)),
        engine(),
    ))
}

#[test]
fn the_handler_runs_the_pass_on_a_blocking_thread_and_the_runtime_keeps_going() {
    let rt = runtime();
    let book = Arc::new(MemoryBook::new());
    book.add("acme", Cadence::Daily, None);
    let job = job(&rt, &book);
    let mut registry = JobHandlerRegistry::new();
    registry.register(JOB_NAME, handler(Arc::clone(&job)));
    let registered = registry.get(JOB_NAME).unwrap();

    // A current-thread runtime has one async thread. If the audit ran on it, the ticker task
    // could not make progress until the audit was over; on the blocking pool it can.
    let here = std::thread::current().id();
    let ticks = rt.block_on(async {
        let counter = Arc::new(AtomicU32::new(0));
        let seen = Arc::clone(&counter);
        let ticker = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(1)).await;
                seen.fetch_add(1, Ordering::Relaxed);
            }
        });
        let result = registered(context(1_800_000_000)).await.unwrap();
        assert_eq!(result, HandlerResult::Ok);
        ticker.abort();
        counter.load(Ordering::Relaxed)
    });
    // The audit slept 100 ms on the pool; the only async thread kept ticking meanwhile.
    assert!(ticks >= 10, "the async thread was blocked: {ticks} ticks");
    let audited_on = THREAD.lock().unwrap().unwrap();
    assert_ne!(audited_on, here);
    let results = book.results();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].outcome, TenantOutcome::Clean);
    // The context's time is the pass time, in milliseconds.
    assert_eq!(results[0].started_millis, 1_800_000_000_000);

    // The same period again: Ok, and nothing is audited.
    let again = rt.block_on(registered(context(1_800_000_100))).unwrap();
    assert_eq!(again, HandlerResult::Ok);
    assert_eq!(book.results().len(), 1);
}

#[test]
fn an_unreachable_directory_is_a_retry_not_a_failure() {
    let rt = runtime();
    let book = Arc::new(MemoryBook::new());
    book.set_directory_failing(true);
    let job = job(&rt, &book);
    let run = handler(job);
    let result = rt.block_on(run(context(1_800_000_000))).unwrap();
    assert_eq!(
        result,
        HandlerResult::Retry("journal_audit_job.provider_failed".to_owned())
    );
}
