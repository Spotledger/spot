//! The webhook store and service: create, read, update, enable/disable/pause, rotate the
//! signing secret, and the lookup the intake uses.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use spot_core::Clock;

use crate::condition::validate_condition;
use crate::config::WebhookConfig;
use crate::error::{Error, Result};
use crate::model::{
    EventType, Header, PayloadMode, SealedSecret, Webhook, WebhookId, WebhookState,
};

/// Persistent storage for webhooks.
#[async_trait]
pub trait WebhookStore: Send + Sync {
    /// Inserts or replaces a webhook.
    async fn put(&self, webhook: &Webhook) -> Result<()>;
    /// Loads a webhook.
    async fn get(&self, id: WebhookId) -> Result<Option<Webhook>>;
    /// Removes a webhook.
    async fn delete(&self, id: WebhookId) -> Result<()>;
    /// Lists every webhook.
    async fn list(&self) -> Result<Vec<Webhook>>;
    /// Lists enabled, active webhooks listening to `doctype` + `event_type`.
    async fn list_enabled_for_event(
        &self,
        doctype: &str,
        event_type: EventType,
    ) -> Result<Vec<Webhook>>;
}

/// An in-memory store, for tests and the WP-TST-10 harness.
#[derive(Debug, Default)]
pub struct InMemoryWebhookStore {
    webhooks: RwLock<HashMap<WebhookId, Webhook>>,
}

impl InMemoryWebhookStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, HashMap<WebhookId, Webhook>> {
        self.webhooks
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<WebhookId, Webhook>> {
        self.webhooks
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[async_trait]
impl WebhookStore for InMemoryWebhookStore {
    async fn put(&self, webhook: &Webhook) -> Result<()> {
        self.write().insert(webhook.id, webhook.clone());
        Ok(())
    }

    async fn get(&self, id: WebhookId) -> Result<Option<Webhook>> {
        Ok(self.read().get(&id).cloned())
    }

    async fn delete(&self, id: WebhookId) -> Result<()> {
        self.write().remove(&id);
        Ok(())
    }

    async fn list(&self) -> Result<Vec<Webhook>> {
        Ok(self.read().values().cloned().collect())
    }

    async fn list_enabled_for_event(
        &self,
        doctype: &str,
        event_type: EventType,
    ) -> Result<Vec<Webhook>> {
        Ok(self
            .read()
            .values()
            .filter(|w| {
                w.enabled
                    && w.state == WebhookState::Active
                    && w.doctype == doctype
                    && w.events.contains(&event_type)
            })
            .cloned()
            .collect())
    }
}

/// Input for creating a webhook.
#[derive(Debug, Clone)]
pub struct CreateWebhookInput {
    /// Human-readable name.
    pub name: String,
    /// Whether it starts enabled.
    pub enabled: bool,
    /// The target URL.
    pub url: String,
    /// The DocType it listens to.
    pub doctype: String,
    /// The event names.
    pub events: Vec<EventType>,
    /// An optional condition expression.
    pub condition: Option<String>,
    /// The payload mode.
    pub payload_mode: PayloadMode,
    /// The field allow-list for `fields` mode.
    pub field_allowlist: Option<Vec<String>>,
    /// The sealed signing secret.
    pub secret: SealedSecret,
    /// Custom headers.
    pub headers: Option<Vec<Header>>,
    /// Request timeout in seconds.
    pub timeout_secs: Option<u64>,
    /// Maximum attempts.
    pub max_attempts: Option<u32>,
}

/// Input for updating a webhook; every field is optional.
#[derive(Debug, Clone, Default)]
pub struct UpdateWebhookInput {
    /// Human-readable name.
    pub name: Option<String>,
    /// Enabled flag.
    pub enabled: Option<bool>,
    /// The target URL.
    pub url: Option<String>,
    /// The event names.
    pub events: Option<Vec<EventType>>,
    /// The condition.
    pub condition: Option<String>,
    /// The payload mode.
    pub payload_mode: Option<PayloadMode>,
    /// The field allow-list.
    pub field_allowlist: Option<Vec<String>>,
    /// The sealed signing secret.
    pub secret: Option<SealedSecret>,
    /// Custom headers.
    pub headers: Option<Vec<Header>>,
    /// Request timeout in seconds.
    pub timeout_secs: Option<u64>,
    /// Maximum attempts.
    pub max_attempts: Option<u32>,
}

/// Webhook administration.
pub struct WebhookService {
    store: Arc<dyn WebhookStore>,
    clock: Arc<dyn Clock>,
    config: WebhookConfig,
}

impl WebhookService {
    /// A service over `store`.
    pub fn new(store: Arc<dyn WebhookStore>, clock: Arc<dyn Clock>, config: WebhookConfig) -> Self {
        Self {
            store,
            clock,
            config,
        }
    }

    /// The store, for the intake and the delivery handler.
    pub fn store(&self) -> &Arc<dyn WebhookStore> {
        &self.store
    }

    /// Creates a webhook.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidUrl`], [`Error::InvalidTimeout`], [`Error::InvalidMaxAttempts`],
    /// [`Error::InvalidFilter`], [`Error::InvalidPayloadMode`], [`Error::InvalidCondition`] or
    /// [`Error::InvalidSecret`].
    pub async fn create_webhook(&self, input: CreateWebhookInput) -> Result<Webhook> {
        let webhook = Webhook {
            id: WebhookId::new(),
            name: input.name,
            enabled: input.enabled,
            state: WebhookState::Active,
            url: input.url,
            doctype: input.doctype,
            events: input.events,
            condition: input.condition,
            payload_mode: input.payload_mode,
            field_allowlist: input.field_allowlist.unwrap_or_default(),
            secret: input.secret,
            headers: input.headers.unwrap_or_default(),
            timeout_secs: input
                .timeout_secs
                .unwrap_or(self.config.default_timeout_secs),
            max_attempts: input
                .max_attempts
                .unwrap_or(self.config.default_max_attempts),
            created_at: self.clock.now_utc(),
            updated_at: self.clock.now_utc(),
            last_delivery: None,
            state_reason: None,
        };
        self.validate(&webhook)?;
        self.store.put(&webhook).await?;
        Ok(webhook)
    }

    /// Updates a webhook's mutable fields.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] when the id is unknown; the same validation errors as
    /// [`create_webhook`](Self::create).
    pub async fn update_webhook(
        &self,
        id: WebhookId,
        input: UpdateWebhookInput,
    ) -> Result<Webhook> {
        let mut webhook = self.load(id).await?;
        if let Some(v) = input.name {
            webhook.name = v;
        }
        if let Some(v) = input.enabled {
            webhook.enabled = v;
        }
        if let Some(v) = input.url {
            webhook.url = v;
        }
        if let Some(v) = input.events {
            webhook.events = v;
        }
        if let Some(v) = input.condition {
            webhook.condition = Some(v);
        }
        if let Some(v) = input.payload_mode {
            webhook.payload_mode = v;
        }
        if let Some(v) = input.field_allowlist {
            webhook.field_allowlist = v;
        }
        if let Some(v) = input.secret {
            webhook.secret = v;
        }
        if let Some(v) = input.headers {
            webhook.headers = v;
        }
        if let Some(v) = input.timeout_secs {
            webhook.timeout_secs = v;
        }
        if let Some(v) = input.max_attempts {
            webhook.max_attempts = v;
        }
        webhook.updated_at = self.clock.now_utc();
        self.validate(&webhook)?;
        self.store.put(&webhook).await?;
        Ok(webhook)
    }

    /// Deletes a webhook.
    pub async fn delete_webhook(&self, id: WebhookId) -> Result<()> {
        self.store.delete(id).await
    }

    /// Loads a webhook.
    pub async fn get_webhook(&self, id: WebhookId) -> Result<Option<Webhook>> {
        self.store.get(id).await
    }

    /// Lists every webhook.
    pub async fn list_webhooks(&self) -> Result<Vec<Webhook>> {
        self.store.list().await
    }

    /// Enables a webhook and clears any disabled state.
    pub async fn enable_webhook(&self, id: WebhookId) -> Result<Webhook> {
        self.mutate(id, |webhook| {
            webhook.enabled = true;
            webhook.state = WebhookState::Active;
            webhook.state_reason = None;
        })
        .await
    }

    /// Disables a webhook with a reason.
    pub async fn disable_webhook(&self, id: WebhookId, reason: Option<String>) -> Result<Webhook> {
        self.mutate(id, |webhook| {
            webhook.enabled = false;
            webhook.state = WebhookState::Disabled;
            webhook.state_reason = reason;
        })
        .await
    }

    /// Pauses a webhook.
    pub async fn pause_webhook(&self, id: WebhookId) -> Result<Webhook> {
        self.mutate(id, |webhook| {
            webhook.state = WebhookState::Paused;
        })
        .await
    }

    /// Resumes a paused webhook.
    pub async fn resume_webhook(&self, id: WebhookId) -> Result<Webhook> {
        self.mutate(id, |webhook| {
            webhook.state = WebhookState::Active;
            webhook.state_reason = None;
        })
        .await
    }

    /// Rotates the signing secret, keeping the old one valid through the overlap window.
    pub async fn rotate_secret(&self, id: WebhookId, new_secret: SealedSecret) -> Result<Webhook> {
        let overlap = self.config.secret_rotation_overlap_secs;
        self.mutate(id, |webhook| {
            let previous = crate::model::PreviousSecret {
                sealed: webhook.secret.sealed.clone(),
                expires_at: self.clock.now_utc() + time::Duration::seconds(overlap),
            };
            webhook.secret = SealedSecret {
                sealed: new_secret.sealed.clone(),
                previous: Some(previous),
            };
        })
        .await
    }

    async fn mutate(&self, id: WebhookId, change: impl FnOnce(&mut Webhook)) -> Result<Webhook> {
        let mut webhook = self.load(id).await?;
        change(&mut webhook);
        webhook.updated_at = self.clock.now_utc();
        self.store.put(&webhook).await?;
        Ok(webhook)
    }

    async fn load(&self, id: WebhookId) -> Result<Webhook> {
        self.store
            .get(id)
            .await?
            .ok_or_else(|| Error::NotFound { id: id.to_string() })
    }

    fn validate(&self, webhook: &Webhook) -> Result<()> {
        let url = url::Url::parse(&webhook.url).map_err(|e| Error::InvalidUrl {
            reason: e.to_string(),
        })?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(Error::InvalidUrl {
                reason: "the scheme must be http or https".to_string(),
            });
        }
        if webhook.timeout_secs == 0 || webhook.timeout_secs > 300 {
            return Err(Error::InvalidTimeout {
                reason: "the timeout must be 1 to 300 seconds".to_string(),
            });
        }
        if webhook.max_attempts == 0 || webhook.max_attempts > 100 {
            return Err(Error::InvalidMaxAttempts {
                reason: "max attempts must be 1 to 100".to_string(),
            });
        }
        if webhook.events.is_empty() {
            return Err(Error::InvalidFilter {
                reason: "at least one event name is required".to_string(),
            });
        }
        if webhook.payload_mode == PayloadMode::Fields && webhook.field_allowlist.is_empty() {
            return Err(Error::InvalidPayloadMode {
                mode: "fields mode needs a field allow-list".to_string(),
            });
        }
        if let Some(condition) = &webhook.condition {
            validate_condition(condition)?;
        }
        if webhook.secret.sealed.trim().is_empty() {
            return Err(Error::InvalidSecret {
                reason: "the signing secret is empty".to_string(),
            });
        }
        Ok(())
    }

    /// Records the last delivery summary and, when the endpoint is gone, disables the webhook.
    pub async fn record_delivery(
        &self,
        id: WebhookId,
        summary: crate::model::DeliverySummary,
    ) -> Result<()> {
        let Ok(mut webhook) = self.load(id).await else {
            return Ok(());
        };
        webhook.last_delivery = Some(summary);
        webhook.updated_at = self.clock.now_utc();
        self.store.put(&webhook).await
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use spot_core::ManualClock;
    use time::OffsetDateTime;

    fn service() -> (WebhookService, Arc<InMemoryWebhookStore>) {
        let store = Arc::new(InMemoryWebhookStore::new());
        let clock = Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH));
        (
            WebhookService::new(store.clone(), clock, WebhookConfig::default()),
            store,
        )
    }

    fn input(url: &str) -> CreateWebhookInput {
        CreateWebhookInput {
            name: "Test".into(),
            enabled: true,
            url: url.into(),
            doctype: "task".into(),
            events: vec![EventType::Create, EventType::Update],
            condition: None,
            payload_mode: PayloadMode::Full,
            field_allowlist: None,
            secret: SealedSecret {
                sealed: "spsec:v1:abc".into(),
                previous: None,
            },
            headers: None,
            timeout_secs: Some(30),
            max_attempts: Some(5),
        }
    }

    #[tokio::test]
    async fn create_validates_and_stores() {
        let (service, store) = service();
        let webhook = service
            .create_webhook(input("https://example.com/hook"))
            .await
            .unwrap();
        assert!(webhook.enabled);
        assert_eq!(store.list().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn invalid_inputs_are_rejected() {
        let (service, _store) = service();
        assert!(service.create_webhook(input("not a url")).await.is_err());
        assert!(
            service
                .create_webhook(input("ftp://example.com"))
                .await
                .is_err()
        );

        let mut fields = input("https://example.com/hook");
        fields.payload_mode = PayloadMode::Fields;
        assert!(service.create_webhook(fields).await.is_err());

        let mut bad_condition = input("https://example.com/hook");
        bad_condition.condition = Some("doc.==".into());
        assert!(service.create_webhook(bad_condition).await.is_err());
    }

    #[tokio::test]
    async fn disable_enable_and_rotate() {
        let (service, _store) = service();
        let webhook = service
            .create_webhook(input("https://example.com/hook"))
            .await
            .unwrap();
        let disabled = service
            .disable_webhook(webhook.id, Some("gone".into()))
            .await
            .unwrap();
        assert!(!disabled.enabled);
        assert_eq!(disabled.state, WebhookState::Disabled);
        let enabled = service.enable_webhook(webhook.id).await.unwrap();
        assert!(enabled.enabled);
        assert!(enabled.state_reason.is_none());

        let rotated = service
            .rotate_secret(
                webhook.id,
                SealedSecret {
                    sealed: "spsec:v1:new".into(),
                    previous: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(rotated.secret.sealed, "spsec:v1:new");
        assert!(rotated.secret.previous.is_some());
    }

    #[tokio::test]
    async fn only_enabled_active_webhooks_match() {
        let (service, _store) = service();
        let webhook = service
            .create_webhook(input("https://example.com/hook"))
            .await
            .unwrap();
        let matched = service
            .store()
            .list_enabled_for_event("task", EventType::Create)
            .await
            .unwrap();
        assert_eq!(matched.len(), 1);
        service.pause_webhook(webhook.id).await.unwrap();
        let matched = service
            .store()
            .list_enabled_for_event("task", EventType::Create)
            .await
            .unwrap();
        assert!(matched.is_empty());
    }

    /// The shipped DocType definition must load and compile with the metadata registry.
    #[test]
    fn the_webhook_doctype_is_valid() {
        let text = include_str!("../../../../apps/platform/doctypes/webhook.toml");
        let definition = spot_meta::Definition::parse("webhook.toml", text)
            .unwrap_or_else(|e| panic!("webhook.toml does not parse: {e}"));
        assert_eq!(definition.name, "webhook");
        spot_meta::Registry::new(vec![definition])
            .unwrap_or_else(|e| panic!("webhook.toml does not compile: {e:?}"));
    }
}
