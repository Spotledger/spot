//! What the engine writes into the outbox is what the events contract and the job intake read
//! (WP-CON-04.T10).
//!
//! A hook enqueues jobs and a notice through the real engine on the in-memory backend; the
//! outbox records are taken as stored and proved three ways: the contract validator accepts the
//! `data` (`system::validate`), `spot-jobs`'s intake type decodes it, and the real intake turns
//! it into the right job. The secret-audit records the engine writes are validated the same
//! way, so the first strict check of the `sys_*` schemas finds nothing new in these producers.
//!
//! The engine is driven directly (as `engine.rs` does) rather than through `host`: the host
//! adds a contract-suite wrapper and no outbox access this test needs. `spot-jobs` does not
//! depend on `spot-document`, so it is a dev-dependency here and no cycle arises.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
#![allow(dead_code, unused_imports, unused_macros)]

mod common;
#[path = "secrets/support.rs"]
mod support;

use std::sync::Arc;
use std::time::Duration;

use common::{meta, n, now};
use serde_json::{Value as Json, json};
use spot_app_api::context::{Actor, DocumentService, HookContext, Invocation, JobRequest};
use spot_app_api::controller::{StepDecl, StepName};
use spot_app_api::prelude::{CommandId, RecordId};
use spot_app_api::{AppResult, BoxFuture, Controller, HookPoint, Phase, Target};
use spot_core::{ManualClock, SiteId};
use spot_document::engine::{Engine, V7Ids};
use spot_events_contract::lifecycle::Operation;
use spot_events_contract::{PayloadDocument, system};
use spot_jobs::store::{JobFilter, JobStore};
use spot_jobs::{InMemoryJobStore, JobEnqueued, process_batch};
use spot_seams::RegistryBuilder;
use spot_storage_contract::{ChangeFeed, FeedPosition, OutboxEvent};
use spot_storage_fake::FakeBackend;
use time::OffsetDateTime;

const SITE: &str = "engine.test";

/// A controller of `customer` whose one step (after create, inside the transaction) enqueues
/// the given jobs and, if given, one notice.
struct Enqueuer {
    jobs: Vec<JobRequest>,
    notice: Option<(RecordId, Json)>,
}

impl Controller for Enqueuer {
    fn doctype(&self) -> &str {
        "customer"
    }

    fn steps(&self) -> Vec<StepDecl> {
        vec![StepDecl {
            name: StepName::new("probe:enqueue").unwrap(),
            point: HookPoint::of(Phase::After, Target::Op(Operation::Create)),
            priority: 0,
            budget: Duration::from_secs(5),
        }]
    }

    fn run_step<'a>(
        &'a self,
        _step: &'a StepName,
        ctx: &'a mut dyn HookContext,
    ) -> BoxFuture<'a, AppResult<()>> {
        let mut result = Ok(());
        for job in &self.jobs {
            if let Err(e) = ctx.enqueue_job(job.clone()) {
                result = Err(e);
                break;
            }
        }
        if let Some((user, payload)) = &self.notice
            && result.is_ok()
        {
            result = ctx.notify_user(*user, payload.clone());
        }
        Box::pin(async move { result })
    }
}

fn inv() -> Invocation {
    Invocation {
        site: spot_app_api::prelude::SiteId::new(SITE).unwrap(),
        actor: Actor::System {
            component: n("job_payload_tests"),
        },
        command_id: CommandId::new("cmd-10").ok(),
        language: None,
        now: now(),
    }
}

/// An engine that has created one customer, so the outbox holds what `controller` enqueued.
async fn engine_after_create(controller: Enqueuer) -> Engine<FakeBackend> {
    let mut builder = RegistryBuilder::new();
    builder.add_controller("probe", Arc::new(controller));
    let engine = Engine::new(
        FakeBackend::new(),
        meta(),
        Arc::new(builder.build().unwrap()),
        Arc::new(ManualClock::new(now())),
        Arc::new(V7Ids),
    )
    .unwrap();
    engine.prepare().await.unwrap();
    let mut doc = spot_app_api::Document::new(n("customer"), RecordId::new_v7());
    doc.set(n("name"), "Acme");
    engine.create(&inv(), doc).await.unwrap();
    engine
}

async fn outbox(engine: &Engine<FakeBackend>) -> Vec<OutboxEvent> {
    engine
        .backend()
        .read_outbox(FeedPosition::START, 10_000)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.event)
        .collect()
}

fn data_of(event: &OutboxEvent) -> Json {
    PayloadDocument::from_outbox_event(event).unwrap().data
}

fn of_topic<'a>(events: &'a [OutboxEvent], topic: &str) -> Vec<&'a OutboxEvent> {
    events
        .iter()
        .filter(|e| e.topic.as_str() == topic)
        .collect()
}

fn not_before() -> OffsetDateTime {
    // 2026-11-03T06:00:00Z
    OffsetDateTime::from_unix_timestamp(1_793_685_600).unwrap()
}

fn full_job() -> JobRequest {
    JobRequest {
        job: "erp_selling:send_overdue_reminders".into(),
        args: json!({"customer": "0192f1c0-0000-7000-8000-000000000043", "limit": 20}),
        idempotency_key: Some("overdue-2026-11-02".into()),
        not_before: Some(not_before()),
    }
}

fn bare_job() -> JobRequest {
    JobRequest {
        job: "erp_selling:rebuild_totals".into(),
        args: Json::Null,
        idempotency_key: None,
        not_before: None,
    }
}

fn no_args_job() -> JobRequest {
    JobRequest {
        job: "erp_selling:close_day".into(),
        args: json!({}),
        idempotency_key: Some("close-1".into()),
        not_before: None,
    }
}

#[tokio::test]
async fn a_job_event_the_engine_writes_validates_against_the_contract() {
    let engine = engine_after_create(Enqueuer {
        jobs: vec![full_job(), bare_job(), no_args_job()],
        notice: None,
    })
    .await;
    let events = outbox(&engine).await;
    let jobs = of_topic(&events, "sys_job_enqueued");
    assert_eq!(jobs.len(), 3);
    for event in jobs {
        let data = data_of(event);
        assert_eq!(
            system::validate("sys_job_enqueued", &data),
            Ok(()),
            "{data}"
        );
    }
}

#[tokio::test]
async fn an_absent_value_is_left_out_not_written_as_null() {
    let engine = engine_after_create(Enqueuer {
        jobs: vec![bare_job()],
        notice: None,
    })
    .await;
    let events = outbox(&engine).await;
    let data = data_of(of_topic(&events, "sys_job_enqueued")[0]);
    assert_eq!(data, json!({"job": "erp_selling:rebuild_totals"}));
}

#[tokio::test]
async fn the_intake_type_decodes_the_record_as_stored() {
    let engine = engine_after_create(Enqueuer {
        jobs: vec![full_job(), bare_job()],
        notice: None,
    })
    .await;
    let events = outbox(&engine).await;
    let jobs = of_topic(&events, "sys_job_enqueued");
    let full: JobEnqueued = serde_json::from_value(data_of(jobs[0])).unwrap();
    assert_eq!(full.site, None, "no producer writes a site");
    assert_eq!(full.job, "erp_selling:send_overdue_reminders");
    assert_eq!(full.args["limit"], json!(20));
    assert_eq!(full.idempotency_key.as_deref(), Some("overdue-2026-11-02"));
    assert_eq!(full.not_before.as_deref(), Some("2026-11-03T06:00:00Z"));
    assert_eq!((full.queue, full.context), (None, None));
    let bare: JobEnqueued = serde_json::from_value(data_of(jobs[1])).unwrap();
    assert_eq!(bare.job, "erp_selling:rebuild_totals");
    assert_eq!(bare.args, Json::Null);
    assert_eq!(bare.idempotency_key, None);
    assert_eq!(bare.not_before, None);
}

#[tokio::test]
async fn the_real_intake_turns_the_engines_job_events_into_jobs() {
    let engine = engine_after_create(Enqueuer {
        jobs: vec![full_job(), bare_job()],
        notice: Some((RecordId::new_v7(), json!({"hello": true}))),
    })
    .await;
    let store = InMemoryJobStore::new();
    let site = SiteId::new(SITE).unwrap();
    let report = process_batch(&store, engine.backend(), &site, 100, now())
        .await
        .unwrap();
    assert!(report.rejected.is_empty(), "{:?}", report.rejected);
    assert_eq!(
        report.inserted, 2,
        "the lifecycle event, the state image and the notice are not jobs"
    );
    let mut jobs = store.list(JobFilter::default()).await.unwrap();
    jobs.sort_by(|a, b| a.name.cmp(&b.name));
    let names: Vec<&str> = jobs.iter().map(|j| j.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "erp_selling:rebuild_totals",
            "erp_selling:send_overdue_reminders"
        ]
    );
    let bare = &jobs[0];
    assert_eq!(bare.args, Json::Null);
    assert_eq!(bare.idempotency_key, None);
    assert_eq!(bare.not_before, now(), "no not_before means now");
    assert_eq!(bare.queue, "default");
    assert_eq!(bare.site, site);
    let full = &jobs[1];
    assert_eq!(
        full.args,
        json!({"customer": "0192f1c0-0000-7000-8000-000000000043", "limit": 20})
    );
    assert_eq!(full.idempotency_key.as_deref(), Some("overdue-2026-11-02"));
    assert_eq!(full.not_before, not_before());
    assert_eq!(full.queue, "default");
    assert_eq!(full.site, site);
}

#[tokio::test]
async fn a_user_notice_the_engine_writes_validates_against_the_contract() {
    let user = RecordId::new_v7();
    let engine = engine_after_create(Enqueuer {
        jobs: Vec::new(),
        notice: Some((
            user,
            json!({"kind": "ping", "count": 3, "tags": ["a", "b"]}),
        )),
    })
    .await;
    let events = outbox(&engine).await;
    let notices = of_topic(&events, "sys_user_notice");
    assert_eq!(notices.len(), 1);
    let data = data_of(notices[0]);
    assert_eq!(data["user"], json!(user.to_string()));
    assert_eq!(system::validate("sys_user_notice", &data), Ok(()), "{data}");
}

#[tokio::test]
async fn the_secret_audit_records_the_engine_writes_validate_against_the_contract() {
    use support::{PASSWORD, RewrapBatch, TOKEN, rig_fake, s};
    let r = rig_fake().await.unwrap();
    let access = r.access();
    let (id, v1) = r.account("audit").await;
    r.engine
        .set_secret(&access, "account", id, "token", s(TOKEN), v1)
        .await
        .unwrap();
    let mut created = spot_app_api::Document::new(n("account"), RecordId::new_v7());
    created.set(n("label"), "created");
    r.engine
        .create_with_secrets(
            &access,
            created,
            vec![
                ("password".into(), s(PASSWORD)),
                ("api_key".into(), s("k-1")),
            ],
        )
        .await
        .unwrap();
    r.engine
        .open_secret(&access, "account", id, "token")
        .await
        .unwrap();
    r.engine
        .find_by_secret(&access, "account", "token", TOKEN.as_bytes())
        .await
        .unwrap();
    // Refused operations are recorded too, with their error codes.
    let _ = r
        .engine
        .find_by_secret(&access, "account", "password", b"x")
        .await;
    let _ = r
        .engine
        .set_secret(
            &access,
            "account",
            RecordId::new_v7(),
            "password",
            s("x"),
            support::Version::FIRST,
        )
        .await;
    r.engine
        .rewrap_secrets(&access, "account", RewrapBatch::first(10))
        .await
        .unwrap();

    let events = r
        .engine
        .backend()
        .read_outbox(FeedPosition::START, 100_000)
        .await
        .unwrap();
    let audits: Vec<&OutboxEvent> = events
        .iter()
        .map(|rec| &rec.event)
        .filter(|e| e.topic.as_str() == "sys_secret_audit")
        .collect();
    assert!(audits.len() >= 6, "the operations were recorded");
    for event in audits {
        let data = data_of(event);
        assert_eq!(
            system::validate("sys_secret_audit", &data),
            Ok(()),
            "{data}"
        );
    }
}
