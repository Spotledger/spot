//! The job intake reads what the document engine writes (WP-CON-04.T10).
//!
//! The engine stores a `sys_job_enqueued` event as a payload document (`{"meta", "data"}`) whose
//! `data` is the shape of `contracts/events` (0.7.7): `job`, and optionally `args`,
//! `idempotency_key`, `not_before`, `queue` and `context`, `null` for absent, and no `site`
//! (the site is the intake caller's context). Every example the contract ships for the topic is
//! wrapped here the way the engine wraps it and run through the real intake.
//!
//! A floating-point number in `args` is rejected with the existing reason
//! `undecodable_payload` (no new reason name is introduced).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use serde_json::{json, Value as Json};
use spot_core::SiteId;
use spot_events_contract::{Actor, EventMeta, PayloadDocument};
use spot_jobs::model::Job;
use spot_jobs::store::{JobFilter, JobStore};
use spot_jobs::{process_batch, InMemoryJobStore, IntakeReport, SYS_JOB_ENQUEUED_TOPIC};
use spot_storage_contract::contract_tests;
use spot_storage_contract::*;
use spot_storage_fake::FakeBackend;
use std::sync::Arc;
use time::OffsetDateTime;

/// Every example of the topic, by file name. A new `sys_job_enqueued*.json` in the contract
/// must be added here (the last test fails until it is).
const EXAMPLES: [(&str, &str); 2] = [
    (
        "sys_job_enqueued.json",
        include_str!("../../../../contracts/events/examples/sys/sys_job_enqueued.json"),
    ),
    (
        "sys_job_enqueued.minimal.json",
        include_str!("../../../../contracts/events/examples/sys/sys_job_enqueued.minimal.json"),
    ),
];

fn name(s: &str) -> Name {
    Name::new(s).unwrap()
}

fn site() -> SiteId {
    SiteId::new("acme.test").unwrap()
}

fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_790_000_000).unwrap()
}

/// The outbox event as the engine writes it: the payload document, `data` nested.
fn wrapped(data: Json) -> OutboxEvent {
    let id = RecordId::new_v7();
    let doc = PayloadDocument {
        meta: EventMeta {
            occurred_at: now(),
            actor: Actor::System {
                component: name("engine_tests"),
            },
            command_id: None,
            causation_id: None,
            correlation_id: id,
            trace_id: None,
        },
        data,
    };
    OutboxEvent {
        id,
        topic: name(SYS_JOB_ENQUEUED_TOPIC),
        subject: NodeRef {
            entity: name("party"),
            id: RecordId::new_v7(),
        },
        subject_version: None,
        schema_version: 1,
        payload: serde_json::to_string(&doc).unwrap(),
    }
}

fn example(file: &str) -> Json {
    let (_, text) = EXAMPLES.iter().find(|(f, _)| *f == file).unwrap();
    serde_json::from_str(text).unwrap()
}

struct Run {
    report: IntakeReport,
    jobs: Vec<Job>,
}

/// Commits `events` in one transaction and runs one intake batch for `site()`.
async fn run(events: Vec<OutboxEvent>) -> Run {
    let backend = FakeBackend::new();
    let catalog: Arc<Catalog> = Arc::new(contract_tests::catalog().unwrap());
    backend.prepare(&catalog).await.unwrap();
    let mut tx = backend
        .begin(
            catalog.clone(),
            TxOptions::new(name("intake_contract"), Isolation::ReadCommitted),
        )
        .await
        .unwrap();
    for event in &events {
        tx.enqueue(event).await.unwrap();
    }
    tx.commit().await.unwrap();
    let store = InMemoryJobStore::new();
    let report = process_batch(&store, &backend, &site(), 100, now())
        .await
        .unwrap();
    let jobs = store.list(JobFilter::default()).await.unwrap();
    Run { report, jobs }
}

fn reasons(run: &Run) -> Vec<&'static str> {
    run.report.rejected.iter().map(|r| r.reason).collect()
}

#[tokio::test]
async fn the_full_example_becomes_a_job_with_every_field() {
    let r = run(vec![wrapped(example("sys_job_enqueued.json"))]).await;
    assert!(r.report.rejected.is_empty(), "{:?}", r.report.rejected);
    assert_eq!(r.report.inserted, 1);
    let [job] = r.jobs.as_slice() else {
        panic!("one job expected, got {}", r.jobs.len());
    };
    assert_eq!(job.name, "erp_selling:send_overdue_reminders");
    assert_eq!(
        job.args,
        json!({"customer": "0192f1c0-0000-7000-8000-000000000043", "limit": 20, "dry_run": false})
    );
    assert_eq!(job.idempotency_key.as_deref(), Some("overdue-2026-11-02"));
    // 2026-11-03T06:00:00Z
    assert_eq!(
        job.not_before,
        OffsetDateTime::from_unix_timestamp(1_793_685_600).unwrap()
    );
    assert_eq!(job.queue, "reminders");
    assert_eq!(job.context, json!({"company": "main", "locale": "en"}));
    assert_eq!(job.site, site(), "the site is the caller's");
}

#[tokio::test]
async fn the_minimal_example_becomes_a_job_on_the_default_queue_starting_now() {
    let r = run(vec![wrapped(example("sys_job_enqueued.minimal.json"))]).await;
    assert!(r.report.rejected.is_empty(), "{:?}", r.report.rejected);
    let [job] = r.jobs.as_slice() else {
        panic!("one job expected, got {}", r.jobs.len());
    };
    assert_eq!(job.name, "erp_selling:send_overdue_reminders");
    assert_eq!(job.args, Json::Null);
    assert_eq!(job.idempotency_key, None);
    assert_eq!(job.not_before, now());
    assert_eq!(job.queue, "default");
    assert_eq!(job.context, Json::Null);
    assert_eq!(job.site, site());
}

#[tokio::test]
async fn an_explicit_null_means_absent_for_every_optional_field() {
    // What the engine writes for a job with nothing optional, before it drops the nulls.
    let r = run(vec![wrapped(json!({
        "job": "app:nulls",
        "args": null,
        "idempotency_key": null,
        "not_before": null,
        "queue": null,
        "context": null,
        "site": null,
    }))])
    .await;
    assert!(r.report.rejected.is_empty(), "{:?}", r.report.rejected);
    let [job] = r.jobs.as_slice() else {
        panic!("one job expected");
    };
    assert_eq!(job.args, Json::Null);
    assert_eq!(job.idempotency_key, None);
    assert_eq!(job.not_before, now());
    assert_eq!(job.queue, "default");
    assert_eq!(job.context, Json::Null);
}

#[tokio::test]
async fn a_legacy_payload_naming_the_callers_site_still_works() {
    let mut data = example("sys_job_enqueued.json");
    data["site"] = json!("acme.test");
    let r = run(vec![wrapped(data)]).await;
    assert!(r.report.rejected.is_empty(), "{:?}", r.report.rejected);
    assert_eq!(r.report.inserted, 1);
    assert_eq!(r.jobs[0].site, site());
}

#[tokio::test]
async fn a_payload_naming_another_site_is_rejected_and_creates_no_job() {
    let mut other = example("sys_job_enqueued.json");
    other["site"] = json!("other.test");
    let mut malformed = example("sys_job_enqueued.json");
    malformed["site"] = json!("Not A Site!");
    let r = run(vec![
        wrapped(other),
        wrapped(malformed),
        wrapped(example("sys_job_enqueued.minimal.json")),
    ])
    .await;
    assert_eq!(reasons(&r), ["invalid_site", "invalid_site"]);
    assert_eq!(r.report.inserted, 1, "the later good event is not blocked");
    assert_eq!(r.jobs.len(), 1);
    assert_eq!(r.jobs[0].site, site());
}

#[tokio::test]
async fn a_float_in_args_or_context_is_rejected_undecodable() {
    let mut in_args = example("sys_job_enqueued.json");
    in_args["args"] = json!({"nested": [{"rate": 1.5}]});
    let mut in_context = example("sys_job_enqueued.json");
    in_context["context"] = json!({"weight": 0.25});
    let r = run(vec![wrapped(in_args), wrapped(in_context)]).await;
    assert_eq!(reasons(&r), ["undecodable_payload", "undecodable_payload"]);
    assert!(r.jobs.is_empty());
}

#[tokio::test]
async fn an_integer_that_looks_like_a_float_is_a_float_only_if_written_as_one() {
    // 20 is an integer; 20.0 is a float and is refused (a decimal is a string, ADR-055).
    let mut ok = example("sys_job_enqueued.json");
    ok["args"] = json!({"limit": 20});
    let bad = wrapped_text(r#"{"job": "app:float", "args": {"limit": 20.0}}"#);
    let r = run(vec![wrapped(ok), bad]).await;
    assert_eq!(reasons(&r), ["undecodable_payload"]);
    assert_eq!(r.jobs.len(), 1);
}

/// Like [`wrapped`], for `data` that must keep its exact text (`20.0` stays a float).
fn wrapped_text(data_text: &str) -> OutboxEvent {
    let mut event = wrapped(json!({"job": "app:placeholder"}));
    let doc: Json = serde_json::from_str(&event.payload).unwrap();
    let meta = serde_json::to_string(&doc["meta"]).unwrap();
    event.payload = format!(r#"{{"meta":{meta},"data":{data_text}}}"#);
    event
}

#[tokio::test]
async fn the_other_contract_rules_are_enforced() {
    let cases: [(&str, Json, &str); 7] = [
        (
            "a job name without an app",
            json!({"job": "nocolon"}),
            "job.invalid_name",
        ),
        (
            "an empty idempotency key",
            json!({"job": "app:j", "idempotency_key": ""}),
            "undecodable_payload",
        ),
        (
            "a not_before that is not RFC 3339",
            json!({"job": "app:j", "not_before": "tomorrow"}),
            "undecodable_payload",
        ),
        (
            "a queue name with capitals",
            json!({"job": "app:j", "queue": "Reminders"}),
            "undecodable_payload",
        ),
        (
            "a context that is not an object",
            json!({"job": "app:j", "context": [1, 2]}),
            "undecodable_payload",
        ),
        ("no job at all", json!({"args": {}}), "undecodable_payload"),
        (
            "a job that is not text",
            json!({"job": 7}),
            "undecodable_payload",
        ),
    ];
    for (what, data, reason) in cases {
        let r = run(vec![wrapped(data)]).await;
        assert_eq!(reasons(&r), [reason], "{what}");
        assert!(r.jobs.is_empty(), "{what}");
    }
}

#[tokio::test]
async fn a_flat_payload_without_the_document_envelope_is_refused() {
    // The old hand-written shape no producer writes: data at the top level, no `meta`.
    let mut event = wrapped(json!({"job": "app:j"}));
    event.payload = json!({"site": "acme.test", "job": "app:j", "args": {}}).to_string();
    let r = run(vec![event]).await;
    assert_eq!(reasons(&r), ["undecodable_payload"]);
    assert!(r.jobs.is_empty());
}

#[tokio::test]
async fn a_replayed_batch_creates_no_second_job() {
    let backend = FakeBackend::new();
    let catalog: Arc<Catalog> = Arc::new(contract_tests::catalog().unwrap());
    backend.prepare(&catalog).await.unwrap();
    let mut tx = backend
        .begin(
            catalog.clone(),
            TxOptions::new(name("intake_contract"), Isolation::ReadCommitted),
        )
        .await
        .unwrap();
    // No idempotency key: only the outbox event id protects the replay.
    tx.enqueue(&wrapped(example("sys_job_enqueued.minimal.json")))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let store = InMemoryJobStore::new();
    process_batch(&store, &backend, &site(), 100, now())
        .await
        .unwrap();
    store
        .set_intake_position(FeedPosition::START)
        .await
        .unwrap();
    let again = process_batch(&store, &backend, &site(), 100, now())
        .await
        .unwrap();
    assert_eq!(again.inserted, 0);
    assert_eq!(store.list(JobFilter::default()).await.unwrap().len(), 1);
}

#[test]
fn every_example_of_the_topic_is_exercised_here() {
    let dir = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../contracts/events/examples/sys"
    );
    let mut on_disk: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|f| f.starts_with("sys_job_enqueued") && f.ends_with(".json"))
        .collect();
    on_disk.sort();
    let mut known: Vec<String> = EXAMPLES.iter().map(|(f, _)| (*f).to_owned()).collect();
    known.sort();
    assert_eq!(
        on_disk, known,
        "add the new example to EXAMPLES and give it a test"
    );
}
