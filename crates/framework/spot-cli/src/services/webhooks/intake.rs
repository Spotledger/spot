//! The intake: the outbox cursor, document images, the pump that turns one event into delivery
//! jobs, and the service that runs it under the outbox lease (WP-INT-01.T2).

use super::*;

// ---------------------------------------------------------------------------------------------
// Intake
// ---------------------------------------------------------------------------------------------

/// The cursor of the intake in the outbox.
#[async_trait]
pub trait CursorStore: Send + Sync {
    /// The saved position, `None` before the first save.
    async fn load(&self) -> std::result::Result<Option<FeedPosition>, &'static str>;
    /// Saves the position.
    async fn save(&self, position: FeedPosition) -> std::result::Result<(), &'static str>;
}

/// An in-memory cursor, for tests.
#[derive(Default)]
pub struct MemoryCursor(Mutex<Option<FeedPosition>>);

#[async_trait]
impl CursorStore for MemoryCursor {
    async fn load(&self) -> std::result::Result<Option<FeedPosition>, &'static str> {
        Ok(*self.0.lock().unwrap_or_else(|p| p.into_inner()))
    }
    async fn save(&self, position: FeedPosition) -> std::result::Result<(), &'static str> {
        *self.0.lock().unwrap_or_else(|p| p.into_inner()) = Some(position);
        Ok(())
    }
}

/// The cursor in the site's database (`spot_webhook_intake`, one row per site).
pub struct PgCursor {
    pool: deadpool_postgres::Pool,
    site: String,
}

impl PgCursor {
    /// A cursor for `site`; call [`prepare`](Self::prepare) once.
    pub fn new(pool: deadpool_postgres::Pool, site: impl Into<String>) -> Self {
        Self {
            pool,
            site: site.into(),
        }
    }

    /// Creates the table when absent.
    pub async fn prepare(&self) -> std::result::Result<(), &'static str> {
        let conn = self.pool.get().await.map_err(|_| "database unavailable")?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS spot_webhook_intake (site text PRIMARY KEY, \
             generation bigint NOT NULL, seq bigint NOT NULL, updated_at timestamptz NOT NULL DEFAULT now())",
            &[],
        )
        .await
        .map(|_| ())
        .map_err(|_| "cursor table not created")
    }
}

#[async_trait]
impl CursorStore for PgCursor {
    async fn load(&self) -> std::result::Result<Option<FeedPosition>, &'static str> {
        let conn = self.pool.get().await.map_err(|_| "database unavailable")?;
        let row = conn
            .query_opt(
                "SELECT generation, seq FROM spot_webhook_intake WHERE site = $1",
                &[&self.site],
            )
            .await
            .map_err(|_| "cursor unreadable")?;
        let Some(row) = row else { return Ok(None) };
        let g: i64 = row.try_get(0).map_err(|_| "cursor unreadable")?;
        let s: i64 = row.try_get(1).map_err(|_| "cursor unreadable")?;
        Ok(Some(FeedPosition::new(
            u64::try_from(g).unwrap_or(0),
            u64::try_from(s).unwrap_or(0),
        )))
    }

    async fn save(&self, position: FeedPosition) -> std::result::Result<(), &'static str> {
        let g = i64::try_from(position.generation()).map_err(|_| "cursor out of range")?;
        let s = i64::try_from(position.seq()).map_err(|_| "cursor out of range")?;
        let conn = self.pool.get().await.map_err(|_| "database unavailable")?;
        conn.execute(
            "INSERT INTO spot_webhook_intake (site, generation, seq) VALUES ($1, $2, $3) \
             ON CONFLICT (site) DO UPDATE SET generation = EXCLUDED.generation, seq = EXCLUDED.seq, updated_at = now()",
            &[&self.site, &g, &s],
        )
        .await
        .map(|_| ())
        .map_err(|_| "cursor not saved")
    }
}

/// What reading a document for an event found.
pub enum Image {
    /// The payload image.
    Found(Json),
    /// The document no longer exists.
    Gone,
}

/// Where the intake reads a document image from.
#[async_trait]
pub trait Images: Send + Sync {
    /// The image of the document, or why it could not be read (a transient fault).
    async fn image(&self, doctype: &str, id: &str) -> std::result::Result<Image, &'static str>;
}

/// [`Images`] over the T4 `SystemReader` and the engine's metadata.
pub struct EngineImages {
    pub(super) reader: Arc<dyn SystemReader>,
    pub(super) access: EngineAccess,
}

#[async_trait]
impl Images for EngineImages {
    async fn image(&self, doctype: &str, id: &str) -> std::result::Result<Image, &'static str> {
        let doc = self
            .reader
            .read(doctype, id)
            .await
            .map_err(|_| "reader unavailable")?;
        let Some(doc) = doc else {
            return Ok(Image::Gone);
        };
        let engine = self.access.engine()?;
        let def = engine
            .meta(doctype)
            .await
            .map_err(|_| "metadata unavailable")?;
        Ok(Image::Found(image_of(&def, &doc)))
    }
}

/// Turns one outbox event into delivery jobs.
pub struct Pump {
    /// Webhooks.
    pub store: Arc<dyn WebhookStore>,
    /// Document images.
    pub images: Arc<dyn Images>,
    /// Job creation.
    pub submit: Arc<dyn JobSubmit>,
    /// The clock the conditions are evaluated at.
    pub clock: Arc<dyn Clock>,
    /// The largest job argument, bytes.
    pub max_args: usize,
}

impl Pump {
    /// Creates the jobs of one event. `Err` is a transient fault: the cursor must not pass it.
    pub async fn process(&self, record: &OutboxRecord) -> std::result::Result<u32, &'static str> {
        let event = &record.event;
        let Some(event_type) = EventType::from_topic(event.topic.as_str()) else {
            return Ok(0);
        };
        let doctype = event.subject.entity.as_str();
        // The log of deliveries and system types never feed back into webhooks.
        if doctype == DELIVERY || doctype.starts_with("sys_") {
            return Ok(0);
        }
        let webhooks = self
            .store
            .list_enabled_for_event(doctype, event_type)
            .await
            .map_err(|_| "webhook store unavailable")?;
        if webhooks.is_empty() {
            return Ok(0);
        }
        let document_id = event.subject.id.to_string();
        let image = if event_type == EventType::Delete {
            json!({ "id": document_id })
        } else {
            match self.images.image(doctype, &document_id).await? {
                Image::Found(image) => image,
                Image::Gone => json!({ "id": document_id }),
            }
        };
        let event_id = event.id.to_string();
        let occurred_at = self.clock.now_utc();
        let mut created = 0;
        for webhook in webhooks {
            if let Some(condition) = &webhook.condition {
                match evaluate_condition(condition, &image, occurred_at) {
                    Ok(true) => {}
                    Ok(false) => continue,
                    Err(_) => {
                        tracing::warn!(target: "spot::audit::webhooks", webhook = %webhook.id, event = event_type.as_str(), "condition failed to evaluate: no delivery");
                        continue;
                    }
                }
            }
            let mut job = DeliveryJob {
                webhook_id: webhook.id,
                delivery_id: delivery_id(webhook.id, &event_id),
                event_id: event_id.clone(),
                topic: Topic::any(event.topic.as_str()).map_err(|_| "topic")?,
                doctype: doctype.to_owned(),
                document_id: document_id.clone(),
                document_version: event.subject_version.map_or(0, |v| v.0),
                event_type,
                occurred_at,
                after_image: image.clone(),
                before_image: None,
                attempt: 1,
                payload_override: None,
            };
            let mut args = serde_json::to_value(&job).map_err(|_| "job encoding")?;
            if args.to_string().len() > self.max_args {
                job.after_image = json!({ "id": document_id });
                args = serde_json::to_value(&job).map_err(|_| "job encoding")?;
            }
            let spec = JobSpec {
                name: spot_webhooks::WEBHOOK_JOB_NAME.to_owned(),
                queue: spot_webhooks::WEBHOOK_QUEUE.to_owned(),
                args,
                idempotency_key: idempotency_key(webhook.id, &event_id),
                not_before: None,
            };
            match self.submit.submit(spec).await {
                Ok(true) => created += 1,
                Ok(false) => {}
                Err(PortError::Invalid(_)) => {
                    tracing::warn!(target: "spot::audit::webhooks", webhook = %webhook.id, "delivery job refused");
                }
                Err(_) => return Err("job submit unavailable"),
            }
        }
        Ok(created)
    }
}

pub(super) struct WebhooksService {
    pub(super) site: String,
    pub(super) outbox: Arc<dyn OutboxLease>,
    pub(super) pump: Pump,
    pub(super) cursor: Arc<dyn CursorStore>,
    pub(super) settings: Settings,
    pub(super) problem: Option<String>,
}

impl WebhooksService {
    /// Reads and handles pages from `from` until the outbox is drained. Returns the new cursor and
    /// whether the lease was lost.
    async fn drain(
        &self,
        lease: &ConsumerLease,
        from: FeedPosition,
        deliver: bool,
    ) -> (FeedPosition, bool) {
        let mut at = from;
        loop {
            let page = match self.outbox.read(at, self.settings.batch_size).await {
                Ok(page) => page,
                Err(_) => return (at, false),
            };
            let full = page.len() >= self.settings.batch_size as usize;
            let mut last = at;
            let mut stalled = false;
            for record in &page {
                if deliver && self.pump.process(record).await.is_err() {
                    stalled = true;
                    break;
                }
                last = record.position;
            }
            if last != at {
                if !matches!(self.outbox.still_held(lease).await, Ok(true)) {
                    return (at, true);
                }
                if self.cursor.save(last).await.is_err() {
                    return (at, false);
                }
                at = last;
            }
            if stalled || !full {
                return (at, false);
            }
        }
    }

    /// Leads until stopped (`true`) or until the lease is lost (`false`).
    async fn lead(&self, stop: &mut watch::Receiver<bool>, mut lease: ConsumerLease) -> bool {
        let mut cursor = match self.cursor.load().await {
            Ok(Some(position)) => position,
            Ok(None) => {
                // First run: later events only, unless the operator asked for the history.
                let (at, lost) = self
                    .drain(&lease, FeedPosition::START, self.settings.replay_history)
                    .await;
                if lost {
                    let _ = self.outbox.release(&lease).await;
                    return false;
                }
                at
            }
            Err(_) => {
                let _ = self.outbox.release(&lease).await;
                return false;
            }
        };
        let mut ticker = tokio::time::interval(Duration::from_millis(self.settings.poll_ms));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let stopped = loop {
            tokio::select! {
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() { break true; }
                }
                _ = ticker.tick() => {
                    match self.outbox.renew(&lease).await {
                        Ok(renewed) => lease = renewed,
                        Err(_) => break false,
                    }
                    let (at, lost) = self.drain(&lease, cursor, true).await;
                    cursor = at;
                    if lost { break false; }
                }
            }
        };
        let _ = self.outbox.release(&lease).await;
        stopped
    }
}

impl Service for WebhooksService {
    fn name(&self) -> &'static str {
        "webhooks"
    }

    fn level(&self) -> Level {
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            if let Some(problem) = &self.problem {
                return Err(problem.clone());
            }
            self.cursor
                .load()
                .await
                .map(|_| ())
                .map_err(|e| format!("the webhook intake cursor is unusable: {e}"))
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            let standby = Duration::from_millis(self.settings.poll_ms.max(500));
            loop {
                if *stop.borrow() {
                    return Ok(());
                }
                match self.outbox.acquire(CONSUMER).await {
                    Ok(lease) => {
                        if self.lead(&mut stop, lease).await {
                            return Ok(());
                        }
                    }
                    Err(PortError::Stopped) => return Ok(()),
                    Err(_) => {
                        tracing::debug!(site = %self.site, "the webhook intake lease is held elsewhere")
                    }
                }
                tokio::select! {
                    changed = stop.changed() => {
                        if changed.is_err() || *stop.borrow() { return Ok(()); }
                    }
                    () = tokio::time::sleep(standby) => {}
                }
            }
        })
    }
}
