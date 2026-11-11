//! The delivery handler reads a webhook's `url`, `headers` and `secret` as the system, not
//! through a caller's field rights (WP-INT-01.T7).
//!
//! Those fields sit in protection groups closed to every API role (`apps/platform/doctypes/
//! webhook.toml`), so the handler must take them from the webhook store and never ask the
//! field-visibility check about them. The visibility here denies every field and records every
//! question; a handler that read `url`, `headers` or `secret` the way an API principal does
//! would be refused, or would ask about the `webhook` DocType, and the tests below would fail.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use http::HeaderMap;
use spot_core::{ManualClock, SiteId};
use spot_egress::{EgressError, Response};
use spot_jobs::store::InMemoryJobStore;
use spot_secrets::SecretString;
use spot_webhooks::{
    BulkheadConfig, CircuitBreakerConfig, DeliveryHandler, DeliveryId, DeliveryJob,
    DeliveryOutcome, Egress, EventType, FieldVisibility, Header, InMemoryDeliveryLog,
    InMemoryWebhookStore, PayloadMode, RateLimitConfig, Resilience, SealedSecret, SecretResolver,
    Webhook, WebhookConfig, WebhookId, WebhookState, WebhookStore,
};
use time::OffsetDateTime;
use url::Url;

/// Denies every field and remembers each `(doctype, field)` asked about.
#[derive(Default)]
struct DenyAllRecording {
    asked: Mutex<Vec<(String, String)>>,
}

impl FieldVisibility for DenyAllRecording {
    fn may_read(&self, doctype: &str, field: &str) -> bool {
        self.asked
            .lock()
            .unwrap()
            .push((doctype.to_owned(), field.to_owned()));
        false
    }
}

/// Opens `spsec:v1:<plain>` to `<plain>`, so a test can see which sealed value was opened.
struct PrefixSecrets {
    opened: Mutex<Vec<String>>,
}

#[async_trait]
impl SecretResolver for PrefixSecrets {
    async fn resolve(&self, sealed: &str, _aad: &[u8]) -> spot_webhooks::Result<SecretString> {
        self.opened.lock().unwrap().push(sealed.to_owned());
        Ok(SecretString::new(
            sealed.trim_start_matches("spsec:v1:").to_owned(),
        ))
    }
}

/// Records what the handler sends.
#[derive(Default)]
struct Recording {
    sent: Mutex<Vec<(String, HeaderMap, Vec<u8>)>>,
}

#[async_trait]
impl Egress for Recording {
    async fn post(
        &self,
        _tenant: &str,
        url: &Url,
        headers: HeaderMap,
        body: Vec<u8>,
    ) -> Result<Response, EgressError> {
        self.sent
            .lock()
            .unwrap()
            .push((url.to_string(), headers, body));
        Ok(Response {
            status: http::StatusCode::OK,
            headers: HeaderMap::new(),
            body: Vec::new(),
        })
    }
}

fn webhook() -> Webhook {
    Webhook {
        id: WebhookId::new(),
        name: "protected".into(),
        enabled: true,
        state: WebhookState::Active,
        url: "https://example.com/hook?token=in-the-query".into(),
        doctype: "task".into(),
        events: vec![EventType::Update],
        condition: None,
        payload_mode: PayloadMode::Full,
        field_allowlist: vec![],
        secret: SealedSecret {
            sealed: "spsec:v1:signing-secret".into(),
            previous: None,
        },
        headers: vec![
            Header {
                name: "x-plain".into(),
                value: "plain-value".into(),
                is_secret: false,
            },
            Header {
                name: "authorization".into(),
                value: "spsec:v1:Bearer-token".into(),
                is_secret: true,
            },
        ],
        timeout_secs: 30,
        max_attempts: 3,
        created_at: OffsetDateTime::UNIX_EPOCH,
        updated_at: OffsetDateTime::UNIX_EPOCH,
        last_delivery: None,
        state_reason: None,
    }
}

fn job(webhook: &Webhook) -> DeliveryJob {
    DeliveryJob {
        webhook_id: webhook.id,
        delivery_id: DeliveryId::new(),
        event_id: "evt-1".into(),
        topic: spot_events_contract::Topic::any("doc_updated").unwrap(),
        doctype: webhook.doctype.clone(),
        document_id: "doc-1".into(),
        document_version: 1,
        event_type: EventType::Update,
        occurred_at: OffsetDateTime::UNIX_EPOCH,
        after_image: serde_json::json!({"status": "Done"}),
        before_image: None,
        attempt: 1,
        payload_override: None,
    }
}

struct Rig {
    visibility: Arc<DenyAllRecording>,
    secrets: Arc<PrefixSecrets>,
    egress: Arc<Recording>,
    handler: DeliveryHandler,
    store: Arc<InMemoryWebhookStore>,
}

fn rig() -> Rig {
    let clock = Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH));
    let store = Arc::new(InMemoryWebhookStore::new());
    let visibility = Arc::new(DenyAllRecording::default());
    let secrets = Arc::new(PrefixSecrets {
        opened: Mutex::new(Vec::new()),
    });
    let egress = Arc::new(Recording::default());
    let handler = DeliveryHandler::new(
        store.clone(),
        Arc::new(InMemoryJobStore::new()),
        Arc::new(InMemoryDeliveryLog::new()),
        egress.clone(),
        secrets.clone(),
        visibility.clone(),
        Resilience::new(
            clock.clone(),
            CircuitBreakerConfig::default(),
            BulkheadConfig { max_concurrent: 4 },
            RateLimitConfig::default(),
        ),
        clock,
        "site".into(),
        SiteId::new("site").unwrap(),
        WebhookConfig::default(),
    )
    .unwrap();
    Rig {
        visibility,
        secrets,
        egress,
        handler,
        store,
    }
}

#[tokio::test]
async fn the_handler_reads_url_headers_and_secret_as_the_system_not_through_field_rights() {
    let rig = rig();
    let hook = webhook();
    rig.store.put(&hook).await.unwrap();
    let outcome = rig.handler.run(&job(&hook)).await;
    assert_eq!(
        outcome,
        DeliveryOutcome::Delivered,
        "a visibility that denies every field must not stop a delivery"
    );

    let sent = rig.egress.sent.lock().unwrap();
    assert_eq!(sent.len(), 1);
    let (url, headers, _body) = &sent[0];
    assert_eq!(url, "https://example.com/hook?token=in-the-query");
    assert_eq!(headers.get("x-plain").unwrap(), "plain-value");
    assert_eq!(headers.get("authorization").unwrap(), "Bearer-token");
    assert!(headers.get("x-spot-signature").is_some(), "signed");

    // The sealed signing secret and the sealed header were opened by the handler itself.
    let opened = rig.secrets.opened.lock().unwrap();
    assert!(opened.contains(&"spsec:v1:signing-secret".to_owned()));
    assert!(opened.contains(&"spsec:v1:Bearer-token".to_owned()));

    // The visibility is the webhook owner's view of the watched DocType (`task`): it is never
    // asked about the webhook's own protected fields.
    let asked = rig.visibility.asked.lock().unwrap();
    for (doctype, field) in asked.iter() {
        assert_ne!(doctype, "webhook", "asked about webhook.{field}");
    }
}

#[tokio::test]
async fn the_visibility_still_hides_the_watched_documents_fields() {
    // The same denying visibility does apply to the payload: reading the webhook's own fields
    // as the system does not widen what the delivered document body shows.
    let rig = rig();
    let hook = webhook();
    rig.store.put(&hook).await.unwrap();
    assert_eq!(
        rig.handler.run(&job(&hook)).await,
        DeliveryOutcome::Delivered
    );
    let sent = rig.egress.sent.lock().unwrap();
    let body = String::from_utf8_lossy(&sent[0].2).into_owned();
    assert!(!body.contains("Done"), "denied field leaked: {body}");
    assert!(!body.contains("signing-secret") && !body.contains("Bearer-token"));
}
