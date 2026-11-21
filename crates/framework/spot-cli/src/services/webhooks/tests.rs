//! Unit tests of the webhooks hosting module (WP-INT-01.T2): the pure helpers, the pump over fake
//! ports and the job store that only submits. The end-to-end proof over PostgreSQL is
//! `tests/webhooks.rs`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use spot_core::SystemClock;
use spot_storage_contract::{NodeRef, OutboxEvent};
use spot_webhooks::InMemoryWebhookStore;

fn n(name: &str) -> Name {
    Name::new(name).unwrap()
}

fn webhook(doctype: &str, events: Vec<EventType>, condition: Option<&str>) -> Webhook {
    Webhook {
        id: WebhookId(Uuid::from_u128(rand_seed())),
        name: "hook".to_owned(),
        enabled: true,
        state: WebhookState::Active,
        url: "https://receiver.example.com/hook".to_owned(),
        doctype: doctype.to_owned(),
        events,
        condition: condition.map(str::to_owned),
        payload_mode: PayloadMode::Full,
        field_allowlist: Vec::new(),
        secret: SealedSecret {
            sealed: format!("{SIGNING_MARK}hook"),
            previous: None,
        },
        headers: Vec::new(),
        timeout_secs: 10,
        max_attempts: 3,
        created_at: OffsetDateTime::UNIX_EPOCH,
        updated_at: OffsetDateTime::UNIX_EPOCH,
        last_delivery: None,
        state_reason: None,
    }
}

/// A distinct non-zero id per call.
fn rand_seed() -> u128 {
    Uuid::now_v7().as_u128()
}

fn record(topic: &str, entity: &str) -> OutboxRecord {
    OutboxRecord {
        position: FeedPosition::new(1, 1),
        transaction: RecordId::new_v7(),
        ordinal: 0,
        event: OutboxEvent {
            id: RecordId::new_v7(),
            topic: n(topic),
            subject: NodeRef {
                entity: n(entity),
                id: RecordId::new_v7(),
            },
            subject_version: None,
            schema_version: 1,
            payload: "{}".to_owned(),
        },
    }
}

/// Records the jobs it was asked for; answers "not new" for a key it already holds.
#[derive(Default)]
struct FakeSubmit {
    jobs: Mutex<Vec<JobSpec>>,
    fail: bool,
}

impl FakeSubmit {
    fn specs(&self) -> Vec<JobSpec> {
        self.jobs.lock().unwrap().clone()
    }
}

impl JobSubmit for FakeSubmit {
    fn submit<'a>(&'a self, job: JobSpec) -> crate::services::ports::PortFuture<'a, bool> {
        Box::pin(async move {
            if self.fail {
                return Err(PortError::Unavailable("store"));
            }
            let mut jobs = self.jobs.lock().unwrap();
            if jobs
                .iter()
                .any(|j| j.idempotency_key == job.idempotency_key)
            {
                return Ok(false);
            }
            jobs.push(job);
            Ok(true)
        })
    }
}

struct FakeImages(std::result::Result<Json, &'static str>);

#[async_trait]
impl Images for FakeImages {
    async fn image(&self, _: &str, _: &str) -> std::result::Result<Image, &'static str> {
        self.0.clone().map(Image::Found)
    }
}

async fn pump(
    hooks: Vec<Webhook>,
    image: std::result::Result<Json, &'static str>,
    submit: Arc<FakeSubmit>,
    max_args: usize,
) -> Pump {
    let store = Arc::new(InMemoryWebhookStore::new());
    for hook in &hooks {
        store.put(hook).await.unwrap();
    }
    Pump {
        store,
        images: Arc::new(FakeImages(image)),
        submit,
        clock: Arc::new(SystemClock::new()),
        max_args,
    }
}

#[test]
fn the_delivery_id_is_stable_and_distinct() {
    let a = WebhookId(Uuid::from_u128(1));
    let b = WebhookId(Uuid::from_u128(2));
    assert_eq!(delivery_id(a, "evt-1"), delivery_id(a, "evt-1"));
    assert_ne!(delivery_id(a, "evt-1"), delivery_id(a, "evt-2"));
    assert_ne!(delivery_id(a, "evt-1"), delivery_id(b, "evt-1"));
}

#[test]
fn the_default_settings_are_valid_and_conservative() {
    let settings = Settings::default();
    assert_eq!(settings.validate(), Ok(()));
    assert!(settings.max_attempts <= 5);
    assert!(settings.max_payload_bytes <= 256 * 1024);
    assert!(!settings.replay_history);
    assert!(settings.allowed_hosts.is_empty());
}

#[test]
fn a_setting_out_of_range_is_named() {
    let bad = |edit: fn(&mut Settings)| {
        let mut settings = Settings::default();
        edit(&mut settings);
        settings.validate()
    };
    assert_eq!(bad(|s| s.max_attempts = 0), Err("max_attempts"));
    assert_eq!(bad(|s| s.max_attempts = 21), Err("max_attempts"));
    assert_eq!(bad(|s| s.max_payload_bytes = 0), Err("max_payload_bytes"));
    assert_eq!(
        bad(|s| s.per_endpoint_concurrency = 0),
        Err("per_endpoint_concurrency")
    );
    assert_eq!(bad(|s| s.timeout_secs = 121), Err("timeout_secs"));
    assert_eq!(bad(|s| s.poll_ms = 10), Err("poll_ms"));
    assert_eq!(bad(|s| s.batch_size = 501), Err("batch_size"));
}

#[test]
fn an_unknown_setting_is_refused() {
    let parsed: std::result::Result<Settings, _> = toml::from_str("max_attemps = 3");
    assert!(parsed.is_err());
    let parsed: Settings =
        toml::from_str("max_attempts = 3\nallowed_hosts = [\"a.example\"]").unwrap();
    assert_eq!(parsed.max_attempts, 3);
    assert_eq!(parsed.allowed_hosts, vec!["a.example".to_owned()]);
}

#[test]
fn a_class_is_cut_to_safe_characters() {
    assert_eq!(clean_class("net.timeout"), "net.timeout");
    assert_eq!(clean_class("bad class\n<x>"), "badclassx");
    assert_eq!(clean_class(&"a".repeat(200)).len(), 64);
}

#[test]
fn a_webhook_document_maps_and_is_capped_by_the_operator() {
    let mut doc = Document::new(n(WEBHOOK), RecordId::new_v7());
    let mut set = |field: &str, value: Value| doc.set(n(field), value);
    set("name", Value::Text("orders".to_owned()));
    set("state", Value::Text("active".to_owned()));
    set("payload_mode", Value::Text("ids".to_owned()));
    set(
        "url",
        Value::Text("https://receiver.example.com/h".to_owned()),
    );
    set("doctype", Value::Text("sales_order".to_owned()));
    set("enabled", Value::Bool(true));
    set(
        "events",
        Value::Json("[\"create\",\"post\",\"nonsense\"]".to_owned()),
    );
    set("timeout_secs", Value::Int(600));
    set("max_attempts", Value::Int(99));
    set("condition", Value::Text("  ".to_owned()));
    let settings = Settings::default();
    let hook = webhook_from_document(&doc, &settings).unwrap();
    assert_eq!(hook.name, "orders");
    assert_eq!(hook.state, WebhookState::Active);
    assert_eq!(hook.payload_mode, PayloadMode::Ids);
    assert_eq!(hook.events, vec![EventType::Create, EventType::Post]);
    assert_eq!(hook.condition, None);
    assert_eq!(hook.timeout_secs, settings.timeout_secs);
    assert_eq!(hook.max_attempts, settings.max_attempts);
    assert_eq!(hook.secret.sealed, format!("{SIGNING_MARK}orders"));
    assert_eq!(webhook_id_of(doc.id()), hook.id);
}

#[test]
fn a_webhook_document_with_a_bad_state_is_unmappable() {
    let mut doc = Document::new(n(WEBHOOK), RecordId::new_v7());
    doc.set(n("name"), Value::Text("x".to_owned()));
    doc.set(n("state"), Value::Text("weird".to_owned()));
    let err = webhook_from_document(&doc, &Settings::default()).unwrap_err();
    assert_eq!(err, Unmappable("state"));
}

#[tokio::test]
async fn the_memory_cursor_keeps_the_last_position() {
    let cursor = MemoryCursor::default();
    assert_eq!(cursor.load().await.unwrap(), None);
    let at = FeedPosition::new(2, 41);
    cursor.save(at).await.unwrap();
    assert_eq!(cursor.load().await.unwrap(), Some(at));
}

#[tokio::test]
async fn one_matching_event_makes_one_job_per_webhook_with_the_derived_key() {
    let submit = Arc::new(FakeSubmit::default());
    let a = webhook("task", vec![EventType::Update], None);
    let b = webhook("task", vec![EventType::Update], None);
    let pump = pump(
        vec![a.clone(), b.clone()],
        Ok(json!({"id": "x", "status": "Done"})),
        Arc::clone(&submit),
        1 << 20,
    )
    .await;
    let rec = record("doc_updated", "task");
    assert_eq!(pump.process(&rec).await, Ok(2));
    let specs = submit.specs();
    assert_eq!(specs.len(), 2);
    let event_id = rec.event.id.to_string();
    for hook in [&a, &b] {
        let key = idempotency_key(hook.id, &event_id);
        let spec = specs.iter().find(|s| s.idempotency_key == key).unwrap();
        assert_eq!(spec.name, spot_webhooks::WEBHOOK_JOB_NAME);
        assert_eq!(spec.queue, spot_webhooks::WEBHOOK_QUEUE);
        let job: DeliveryJob = serde_json::from_value(spec.args.clone()).unwrap();
        assert_eq!(job.delivery_id, delivery_id(hook.id, &event_id));
        assert_eq!(job.attempt, 1);
    }
}

#[tokio::test]
async fn a_replayed_event_creates_no_second_job() {
    let submit = Arc::new(FakeSubmit::default());
    let hook = webhook("task", vec![EventType::Update], None);
    let pump = pump(
        vec![hook],
        Ok(json!({"id": "x"})),
        Arc::clone(&submit),
        1 << 20,
    )
    .await;
    let rec = record("doc_updated", "task");
    assert_eq!(pump.process(&rec).await, Ok(1));
    assert_eq!(pump.process(&rec).await, Ok(0));
    assert_eq!(submit.specs().len(), 1);
}

#[tokio::test]
async fn a_false_condition_sends_nothing_and_a_true_one_does() {
    let submit = Arc::new(FakeSubmit::default());
    let open = webhook(
        "task",
        vec![EventType::Update],
        Some("doc.status == 'Open'"),
    );
    let done = webhook(
        "task",
        vec![EventType::Update],
        Some("doc.status == 'Done'"),
    );
    let pump = pump(
        vec![open, done.clone()],
        Ok(json!({"id": "x", "status": "Done"})),
        Arc::clone(&submit),
        1 << 20,
    )
    .await;
    let rec = record("doc_updated", "task");
    assert_eq!(pump.process(&rec).await, Ok(1));
    let key = idempotency_key(done.id, &rec.event.id.to_string());
    assert_eq!(submit.specs()[0].idempotency_key, key);
}

#[tokio::test]
async fn an_event_no_webhook_listens_to_makes_no_job() {
    let submit = Arc::new(FakeSubmit::default());
    let hook = webhook("task", vec![EventType::Create], None);
    let pump = pump(
        vec![hook],
        Ok(json!({"id": "x"})),
        Arc::clone(&submit),
        1 << 20,
    )
    .await;
    // Another event name, another DocType, a topic that is not a lifecycle event.
    assert_eq!(pump.process(&record("doc_updated", "task")).await, Ok(0));
    assert_eq!(pump.process(&record("doc_created", "invoice")).await, Ok(0));
    assert_eq!(
        pump.process(&record("sys_job_enqueued", "task")).await,
        Ok(0)
    );
    assert!(submit.specs().is_empty());
}

#[tokio::test]
async fn the_delivery_log_and_system_types_never_feed_back() {
    let submit = Arc::new(FakeSubmit::default());
    let on_log = webhook(DELIVERY, vec![EventType::Create], None);
    let on_sys = webhook("sys_secret_audit", vec![EventType::Create], None);
    let pump = pump(
        vec![on_log, on_sys],
        Ok(json!({"id": "x"})),
        Arc::clone(&submit),
        1 << 20,
    )
    .await;
    assert_eq!(pump.process(&record("doc_created", DELIVERY)).await, Ok(0));
    assert_eq!(
        pump.process(&record("doc_created", "sys_secret_audit"))
            .await,
        Ok(0)
    );
    assert!(submit.specs().is_empty());
}

#[tokio::test]
async fn a_transient_fault_stops_the_cursor() {
    let hook = webhook("task", vec![EventType::Update], None);
    let broken = Arc::new(FakeSubmit {
        fail: true,
        ..FakeSubmit::default()
    });
    let rec = record("doc_updated", "task");
    let reader_down = pump(
        vec![hook.clone()],
        Err("reader unavailable"),
        Arc::new(FakeSubmit::default()),
        1 << 20,
    )
    .await;
    assert!(reader_down.process(&rec).await.is_err());
    let store_down = pump(vec![hook], Ok(json!({"id": "x"})), broken, 1 << 20).await;
    assert!(store_down.process(&rec).await.is_err());
}

#[tokio::test]
async fn an_oversize_image_is_cut_to_the_id() {
    let submit = Arc::new(FakeSubmit::default());
    let hook = webhook("task", vec![EventType::Update], None);
    let big = json!({"id": "x", "note": "y".repeat(4000)});
    let pump = pump(vec![hook], Ok(big), Arc::clone(&submit), 2048).await;
    let rec = record("doc_updated", "task");
    assert_eq!(pump.process(&rec).await, Ok(1));
    let job: DeliveryJob = serde_json::from_value(submit.specs()[0].args.clone()).unwrap();
    assert_eq!(
        job.after_image,
        json!({ "id": rec.event.subject.id.to_string() })
    );
}

#[tokio::test]
async fn a_delete_event_carries_only_the_id() {
    let submit = Arc::new(FakeSubmit::default());
    let hook = webhook("task", vec![EventType::Delete], None);
    // The image reader must not even be consulted for a deleted document.
    let pump = pump(
        vec![hook],
        Err("must not be read"),
        Arc::clone(&submit),
        1 << 20,
    )
    .await;
    let rec = record("doc_deleted", "task");
    assert_eq!(pump.process(&rec).await, Ok(1));
    let job: DeliveryJob = serde_json::from_value(submit.specs()[0].args.clone()).unwrap();
    assert_eq!(
        job.after_image,
        json!({ "id": rec.event.subject.id.to_string() })
    );
}

#[tokio::test]
async fn the_submit_only_store_creates_jobs_and_refuses_everything_else() {
    let submit = Arc::new(FakeSubmit::default());
    let store = SubmitOnlyJobs(Arc::clone(&submit) as Arc<dyn JobSubmit>);
    let new_job = || {
        NewJob::new(
            spot_webhooks::WEBHOOK_JOB_NAME.to_owned(),
            json!({}),
            spot_webhooks::WEBHOOK_QUEUE.to_owned(),
            SiteId::new("a").unwrap(),
            OffsetDateTime::UNIX_EPOCH,
        )
        .with_idempotency_key("k".to_owned())
    };
    assert!(matches!(
        store.insert_if_new(new_job()).await,
        Ok(InsertResult::Inserted(_))
    ));
    assert!(matches!(
        store.insert_if_new(new_job()).await,
        Ok(InsertResult::Duplicate(_))
    ));
    assert!(store.get(JobId(1)).await.is_err());
    assert!(store.list(JobFilter::default()).await.is_err());
    assert!(store.cancel(JobId(1)).await.is_err());
    assert!(store.intake_position().await.is_err());
}

#[tokio::test]
async fn a_signing_marker_is_the_only_secret_the_resolver_opens() {
    struct Keys;
    impl WebhookSigningKeys for Keys {
        fn signing_key<'a>(
            &'a self,
            site: &'a str,
            webhook_name: &'a str,
        ) -> spot_http::api::service::BoxFuture<
            'a,
            std::result::Result<SecretString, SigningKeyError>,
        > {
            Box::pin(async move {
                if site == "a" && webhook_name == "hook" {
                    Ok(SecretString::new("k"))
                } else {
                    Err(SigningKeyError::WrongSite)
                }
            })
        }
    }
    let resolver = SigningResolver::new("a", Arc::new(Keys));
    assert!(resolver.resolve("webhook-signing:hook", b"").await.is_ok());
    // A secret header value, or a marker of another webhook, is not opened.
    assert!(resolver.resolve("spsec:v1:abc", b"").await.is_err());
    assert!(
        resolver
            .resolve("webhook-signing:other", b"")
            .await
            .is_err()
    );
    let other_site = SigningResolver::new("b", Arc::new(Keys));
    assert!(
        other_site
            .resolve("webhook-signing:hook", b"")
            .await
            .is_err()
    );
}
