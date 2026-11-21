//! The engine, as the platform itself: `webhook` documents, the `webhook_delivery` log and the
//! signing-key resolver (WP-INT-01.T2).

use super::*;

// ---------------------------------------------------------------------------------------------
// The engine, as the platform itself
// ---------------------------------------------------------------------------------------------

/// The one filter shape the lookups need.
enum Filter<'a> {
    BoolTrue(&'a str),
    TextEquals(&'a str, String),
}

/// What the engine-backed adapters share.
#[derive(Clone)]
pub struct EngineAccess {
    pub(super) site: SiteId,
    pub(super) live: Arc<SiteLive>,
    pub(super) clock: Arc<dyn Clock>,
    pub(super) catalog: Arc<Catalog>,
}

impl EngineAccess {
    pub(super) fn engine(&self) -> std::result::Result<Arc<Engine<PgBackend>>, &'static str> {
        self.live.engine().ok_or("engine unavailable")
    }

    pub(super) fn inv(&self) -> std::result::Result<Invocation, &'static str> {
        Ok(Invocation {
            site: self.site.clone(),
            actor: Actor::System {
                component: Name::new("webhooks").map_err(|_| "name")?,
            },
            command_id: None,
            language: None,
            now: self.clock.now_utc(),
        })
    }

    /// `select` columns of `entity` where `filter` holds (at most [`MAX_ROWS`] rows).
    async fn rows(
        &self,
        entity: &str,
        select: &[&str],
        filter: Filter<'_>,
    ) -> std::result::Result<Vec<Vec<Value>>, &'static str> {
        let bad = |_| "name";
        let entity = Name::new(entity).map_err(bad)?;
        let alias = Name::new("w").map_err(bad)?;
        let mut query = Query::from(&entity, &alias, QueryPath::Report);
        for column in select {
            query.select.push(Selection {
                expr: Expr::field(&alias, &Name::new(column).map_err(bad)?),
                alias: Name::new(column).map_err(bad)?,
            });
        }
        query.filter = Some(match filter {
            Filter::BoolTrue(field) => {
                Expr::field(&alias, &Name::new(field).map_err(bad)?).eq(Expr::value(true))
            }
            Filter::TextEquals(field, pattern) => Expr::Text(
                Box::new(Expr::field(&alias, &Name::new(field).map_err(bad)?)),
                TextMatch {
                    mode: TextMode::Equals,
                    pattern,
                    case_insensitive: false,
                },
            ),
        });
        query.limit = Some(MAX_ROWS);
        let options = TxOptions::new(
            Name::new("webhooks_read").map_err(bad)?,
            Isolation::ReadCommitted,
        )
        .read_only();
        let engine = self.engine()?;
        let mut tx = engine
            .backend()
            .begin(Arc::clone(&self.catalog), options)
            .await
            .map_err(|_| "storage unavailable")?;
        let rows = tx.query(&query).await;
        let _ = tx.rollback().await;
        rows.map(|r| r.rows).map_err(|_| "storage unavailable")
    }

    pub(super) async fn load(
        &self,
        doctype: &str,
        id: RecordId,
    ) -> std::result::Result<Option<Document>, &'static str> {
        let engine = self.engine()?;
        let inv = self.inv()?;
        match engine.load(&inv, doctype, id).await {
            Ok(loaded) => Ok(Some(loaded.value)),
            Err(e) if e.class == ErrorClass::NotFound => Ok(None),
            Err(_) => Err("engine unavailable"),
        }
    }
}

fn store_error(reason: &'static str) -> WhError {
    WhError::IntakeFailed {
        reason: reason.to_owned(),
    }
}

/// `webhook` documents as the delivery crate's [`WebhookStore`]. It reads documents and writes
/// back only the run-time fields (`last_delivery`, `state`, `enabled`, `state_reason`).
pub struct EngineWebhooks {
    pub(super) access: EngineAccess,
    pub(super) settings: Settings,
}

#[async_trait]
impl WebhookStore for EngineWebhooks {
    async fn put(&self, webhook: &Webhook) -> spot_webhooks::Result<()> {
        let id = record_id_of(webhook.id).ok_or_else(|| store_error("bad id"))?;
        let Some(doc) = self.access.load(WEBHOOK, id).await.map_err(store_error)? else {
            return Err(WhError::NotFound {
                id: webhook.id.to_string(),
            });
        };
        let mut patch = DocumentPatch::default();
        let mut set = |field: &str, value: Value| {
            if let Ok(name) = Name::new(field) {
                patch.fields.insert(name, value);
            }
        };
        let state = serde_json::to_value(webhook.state).ok();
        if let Some(state) = state.as_ref().and_then(Json::as_str) {
            set("state", Value::Text(state.to_owned()));
        }
        set("enabled", Value::Bool(webhook.enabled));
        set(
            "state_reason",
            webhook
                .state_reason
                .clone()
                .map_or(Value::Null, Value::Text),
        );
        if let Some(last) = &webhook.last_delivery {
            // The summary holds a status, an error class and a duration, never a body.
            if let Ok(text) = serde_json::to_string(last) {
                set("last_delivery", Value::Json(text));
            }
        }
        let version = doc.version().unwrap_or(Version::FIRST);
        let engine = self.access.engine().map_err(store_error)?;
        let inv = self.access.inv().map_err(store_error)?;
        // A lost version race only loses one run-time summary.
        engine
            .update(&inv, WEBHOOK, id, patch, version)
            .await
            .map(|_| ())
            .map_err(|_| store_error("update failed"))
    }

    async fn get(&self, id: WebhookId) -> spot_webhooks::Result<Option<Webhook>> {
        let Some(record) = record_id_of(id) else {
            return Ok(None);
        };
        let doc = self
            .access
            .load(WEBHOOK, record)
            .await
            .map_err(store_error)?;
        doc.map(|d| webhook_from_document(&d, &self.settings))
            .transpose()
            .map_err(|e| store_error(e.0))
    }

    async fn delete(&self, _id: WebhookId) -> spot_webhooks::Result<()> {
        Err(store_error("read only"))
    }

    async fn list(&self) -> spot_webhooks::Result<Vec<Webhook>> {
        let rows = self
            .access
            .rows(WEBHOOK, &["id"], Filter::BoolTrue("enabled"))
            .await
            .map_err(store_error)?;
        let mut out = Vec::new();
        for row in rows {
            if let Some(Value::Id(id)) = row.first()
                && let Some(doc) = self.access.load(WEBHOOK, *id).await.map_err(store_error)?
                && let Ok(webhook) = webhook_from_document(&doc, &self.settings)
            {
                out.push(webhook);
            }
        }
        Ok(out)
    }

    async fn list_enabled_for_event(
        &self,
        doctype: &str,
        event_type: EventType,
    ) -> spot_webhooks::Result<Vec<Webhook>> {
        Ok(self
            .list()
            .await?
            .into_iter()
            .filter(|w| {
                w.enabled
                    && w.state == WebhookState::Active
                    && w.doctype == doctype
                    && w.events.contains(&event_type)
            })
            .collect())
    }
}

/// The delivery log as `webhook_delivery` documents: one per attempt.
pub struct EngineDeliveryLog {
    pub(super) access: EngineAccess,
}

struct AttemptRow {
    id: RecordId,
    attempt: u32,
    duration_ms: u64,
    state: String,
}

impl EngineDeliveryLog {
    async fn attempts(
        &self,
        delivery: DeliveryId,
    ) -> std::result::Result<Vec<AttemptRow>, &'static str> {
        let wanted = delivery.to_string();
        let rows = self
            .access
            .rows(
                DELIVERY,
                &["id", "attempt", "duration_ms", "state"],
                Filter::TextEquals("delivery_id", wanted),
            )
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|row| match row.as_slice() {
                [
                    Value::Id(id),
                    Value::Int(attempt),
                    duration,
                    Value::Text(state),
                ] => Some(AttemptRow {
                    id: *id,
                    attempt: u32::try_from(*attempt).unwrap_or(0),
                    duration_ms: match duration {
                        Value::Int(d) => u64::try_from(*d).unwrap_or(0),
                        _ => 0,
                    },
                    state: state.clone(),
                }),
                _ => None,
            })
            .collect())
    }
}

#[async_trait]
impl DeliveryLog for EngineDeliveryLog {
    async fn put(&self, delivery: &WebhookDelivery) -> spot_webhooks::Result<()> {
        let rows = self.attempts(delivery.id).await.map_err(store_error)?;
        let previous: u64 = rows
            .iter()
            .filter(|r| r.attempt < delivery.attempts)
            .map(|r| r.duration_ms)
            .sum();
        let duration =
            i64::try_from(delivery.total_duration_ms.saturating_sub(previous)).unwrap_or(0);
        let mut fields: BTreeMap<Name, Value> = BTreeMap::new();
        let mut set = |field: &str, value: Value| {
            if let Ok(name) = Name::new(field) {
                fields.insert(name, value);
            }
        };
        set("state", Value::Text(state_text(delivery).to_owned()));
        set(
            "status_code",
            delivery
                .last_status_code
                .map_or(Value::Null, |c| Value::Int(i64::from(c))),
        );
        set(
            "error_class",
            delivery
                .last_error_class
                .as_deref()
                .map_or(Value::Null, |c| Value::Text(clean_class(c))),
        );
        set("finished_at", Value::DateTime(delivery.updated_at));
        set("duration_ms", Value::Int(duration));
        let engine = self.access.engine().map_err(store_error)?;
        let inv = self.access.inv().map_err(store_error)?;
        if let Some(existing) = rows.iter().find(|r| r.attempt == delivery.attempts) {
            let doc = self
                .access
                .load(DELIVERY, existing.id)
                .await
                .map_err(store_error)?;
            let version = doc.and_then(|d| d.version()).unwrap_or(Version::FIRST);
            return engine
                .update(
                    &inv,
                    DELIVERY,
                    existing.id,
                    DocumentPatch {
                        fields,
                        ..DocumentPatch::default()
                    },
                    version,
                )
                .await
                .map(|_| ())
                .map_err(|_| store_error("log update failed"));
        }
        let webhook = record_id_of(delivery.webhook_id).ok_or_else(|| store_error("bad id"))?;
        let mut doc = Document::new(
            Name::new(DELIVERY).map_err(|_| store_error("name"))?,
            RecordId::new_v7(),
        );
        for (name, value) in fields {
            doc.set(name, value);
        }
        let mut put = |field: &str, value: Value| {
            if let Ok(name) = Name::new(field) {
                doc.set(name, value);
            }
        };
        put("webhook", Value::Id(webhook));
        put("event_id", Value::Text(delivery.event_id.clone()));
        put("delivery_id", Value::Text(delivery.id.to_string()));
        put("attempt", Value::Int(i64::from(delivery.attempts)));
        put("started_at", Value::DateTime(delivery.updated_at));
        engine
            .create(&inv, doc)
            .await
            .map(|_| ())
            .map_err(|_| store_error("log write failed"))
    }

    async fn get(&self, id: DeliveryId) -> spot_webhooks::Result<Option<WebhookDelivery>> {
        let rows = self.attempts(id).await.map_err(store_error)?;
        let Some(last) = rows.iter().max_by_key(|r| r.attempt) else {
            return Ok(None);
        };
        let status = match last.state.as_str() {
            "delivered" => DeliveryStatus::Delivered,
            "dead_letter" => DeliveryStatus::DeadLetter,
            _ => DeliveryStatus::Pending,
        };
        let now = self.access.clock.now_utc();
        Ok(Some(WebhookDelivery {
            id,
            webhook_id: WebhookId(Uuid::nil()),
            event_id: String::new(),
            topic: Topic::any("doc_updated").map_err(|_| store_error("topic"))?,
            document_id: String::new(),
            document_version: 0,
            status,
            attempts: last.attempt,
            max_attempts: 0,
            last_status_code: None,
            last_error_class: None,
            last_error_message: None,
            total_duration_ms: rows.iter().map(|r| r.duration_ms).sum(),
            created_at: now,
            updated_at: now,
            next_retry_at: None,
            idempotency_key: String::new(),
            payload_sent: None,
            response_body: None,
        }))
    }
}

/// The signing key through the T5 port. Only the signing marker opens; any other sealed text (a
/// secret header value) is refused, so nothing is sent with a secret this module cannot open.
pub struct SigningResolver {
    site: String,
    keys: Arc<dyn WebhookSigningKeys>,
}

impl SigningResolver {
    /// A resolver for `site` over `keys`.
    pub fn new(site: impl Into<String>, keys: Arc<dyn WebhookSigningKeys>) -> Self {
        Self {
            site: site.into(),
            keys,
        }
    }
}

#[async_trait]
impl SecretResolver for SigningResolver {
    async fn resolve(&self, sealed: &str, _aad: &[u8]) -> spot_webhooks::Result<SecretString> {
        let invalid = |reason: &str| WhError::InvalidSecret {
            reason: reason.to_owned(),
        };
        let name = sealed
            .strip_prefix(SIGNING_MARK)
            .ok_or_else(|| invalid("secret.unsupported"))?;
        self.keys.signing_key(&self.site, name).await.map_err(|e| {
            invalid(match e {
                SigningKeyError::NotConfigured => "secret.not_configured",
                SigningKeyError::NotFound => "secret.webhook_not_found",
                SigningKeyError::NotSet => "secret.not_set",
                SigningKeyError::WrongSite => "secret.wrong_site",
                SigningKeyError::Unavailable => "secret.unavailable",
                SigningKeyError::Failed => "secret.open_failed",
            })
        })
    }
}
