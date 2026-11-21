//! The two adapters: a Case step becomes a durable `spot-jobs-pg` job, and a timer port acts only
//! while its scheduler lease is held.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use spot_app_api::prelude::SiteId;
use spot_bpm_engine::{CaseId, CaseJob, InMemoryTimerPort, TimerPort, TimerRegistration};
use spot_bpm_pg::{FencedTimerPort, PgCaseJobs};
use spot_jobs::model::InsertResult;
use spot_jobs::store::{JobFilter, JobStore};
use spot_jobs_pg::PgJobStore;
use spot_postgres::testing::TestDb;
use spot_scheduler::{InMemoryLeaseStore, LeaseStore};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_case_step_is_enqueued_once() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let job_store = Arc::new(PgJobStore::new(pool(&db, 4, "jobs-adapter")));
    job_store.prepare().await.unwrap();
    let jobs = PgCaseJobs::new(job_store.clone());

    let case = CaseId::new("case_job").unwrap();
    let site = SiteId::new("test-site").unwrap();
    let job = CaseJob::new(case.clone(), site, now());

    let first = jobs.enqueue(&job).await.unwrap();
    assert!(matches!(first, InsertResult::Inserted(_)));
    let second = jobs.enqueue(&job).await.unwrap();
    assert!(
        matches!(second, InsertResult::Duplicate(_)),
        "a second enqueue of the same Case is dropped"
    );

    let listed = job_store.list(JobFilter::default()).await.unwrap();
    assert_eq!(listed.len(), 1, "exactly one job");
    assert_eq!(listed[0].name, "bpm:case");
    assert_eq!(
        listed[0].idempotency_key.as_deref(),
        Some("bpm:case:case_job")
    );
}

#[test]
fn the_timer_port_acts_only_while_the_lease_is_held() {
    let leases: Arc<dyn LeaseStore> = Arc::new(InMemoryLeaseStore::new());
    let at = jiff::Timestamp::from_second(1_700_000_000).unwrap();
    let name = "scheduler:test-site";
    let ttl = Duration::from_secs(60);
    let lease = leases.acquire(name, "node-1", ttl, at).unwrap();

    let inner: Arc<dyn TimerPort> = Arc::new(InMemoryTimerPort::new());
    let port = FencedTimerPort::new(leases.clone(), lease.clone(), inner);
    let registration = TimerRegistration {
        case: CaseId::new("case_timer").unwrap(),
        node: nid("n0"),
        attempt: 1,
        due: Some(now()),
        event: None,
        human: None,
    };

    port.register(registration.clone());
    assert_eq!(port.due(now()).len(), 1, "the holder fires its wait");
    assert_eq!(port.fired_count(&registration.case, &registration.node), 1);
    assert_eq!(port.due(now()).len(), 0, "a wait fires exactly once");

    // The lease moves to another holder; the stale port can no longer act.
    leases.release(&lease).unwrap();
    let _new = leases.acquire(name, "node-2", ttl, at).unwrap();
    assert!(!port.holds());
    let later = TimerRegistration {
        attempt: 2,
        ..registration.clone()
    };
    port.register(later);
    assert_eq!(port.due(now()).len(), 0, "a stale holder fires nothing");
    assert_eq!(
        port.fired_count(&registration.case, &registration.node),
        1,
        "the stale port did not touch the fired count"
    );
}
