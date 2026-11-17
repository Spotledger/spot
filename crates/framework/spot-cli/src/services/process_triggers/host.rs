//! The trigger intake itself: the durable marks, the document-event, schedule and webhook paths,
//! and the starter that turns a start key into one Case (see the parent module for the rules).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Bytes;
use deadpool_postgres::Pool;
use serde_json::json;
use sha2::{Digest, Sha256};
use spot_bpm_definitions::{DefinitionStore, MAX_PAGE, StoredVersion, VersionState};
use spot_bpm_defs_pg::PgDefinitionStore;
use spot_bpm_engine::{CASE_JOB_NAME, CaseId, CaseJob, CaseRecord, CaseStore};
use spot_bpm_model::{VersionStatus, from_text};
use spot_bpm_triggers::webhook::{IncomingWebhook, accept};
use spot_bpm_triggers::{
    CaseStarter, PermissionGate, Source, SourceEvent, StartGate, StartInput, StartKey,
    StartOutcome, TriggerIndex, TriggerIntake, TriggerRegistry,
};
use spot_core::{Clock, SiteId};
use spot_dag::Published;
use spot_dag_exec::start as dag_start;
use spot_document::engine::PrincipalSource;
use spot_events_contract::{Actor, PayloadDocument, Topic};
use spot_http::api::ports::webhook::{WebhookPort, WebhookRejected};
use spot_http::api::service::{BoxFuture, RequestCtx};
use spot_permissions::Principal;
use spot_scheduler::SlotRef;
use spot_scheduler::policy::plan;
use spot_storage_contract::{FeedPosition, OutboxRecord};
use spot_webhooks::EventType;
use time::OffsetDateTime;

use super::seams::{Audit, AuditEntry, Meta, Preloaded, SecretSource, doc_facts, doc_image, lock};
use super::{DECOY, READ_ATTEMPTS, Settings, delivery_id_ok, trigger_name};
use crate::services::ports::{JobSpec, JobSubmit, OutboxLease, SystemReader};
use crate::services::process::ProcessHost;
use crate::{Error, Result};

/// Everything the module is built from.
pub struct Parts {
    /// The site.
    pub site: SiteId,
    /// The site's clock.
    pub clock: Arc<dyn Clock>,
    /// The settings.
    pub settings: Settings,
    /// The engine host.
    pub process: Arc<ProcessHost>,
    /// The durable definition store the published versions are read from.
    pub definitions: Arc<PgDefinitionStore>,
    /// Case-step job submission.
    pub submit: Arc<dyn JobSubmit>,
    /// The platform's document reader.
    pub reader: Arc<dyn SystemReader>,
    /// User id to principal.
    pub principals: Arc<dyn PrincipalSource>,
    /// Live metadata.
    pub meta: Arc<dyn Meta>,
    /// Webhook secrets.
    pub secrets: Arc<dyn SecretSource>,
    /// The audit sink.
    pub audit: Arc<dyn Audit>,
    /// The site database pool (the position and the schedule marks).
    pub pool: Pool,
}

/// The durable position and schedule marks of one site.
struct Marks {
    pool: Pool,
    tenant: String,
}

impl Marks {
    async fn prepare(&self) -> Result<()> {
        let client = self.client().await?;
        client
            .batch_execute(
                "CREATE TABLE IF NOT EXISTS spot_bpm_trigger_mark (\
                 tenant text NOT NULL, name text NOT NULL, a bigint NOT NULL, b bigint NOT NULL, \
                 PRIMARY KEY (tenant, name))",
            )
            .await
            .map_err(|e| Error(format!("the trigger mark table could not be prepared: {e}")))
    }

    async fn client(&self) -> Result<deadpool_postgres::Client> {
        self.pool.get().await.map_err(|e| {
            Error(format!(
                "the trigger marks could not reach the database: {e}"
            ))
        })
    }

    async fn get(&self, name: &str) -> Result<Option<(i64, i64)>> {
        let client = self.client().await?;
        let row = client
            .query_opt(
                "SELECT a, b FROM spot_bpm_trigger_mark WHERE tenant = $1 AND name = $2",
                &[&self.tenant, &name],
            )
            .await
            .map_err(|e| Error(format!("reading a trigger mark failed: {e}")))?;
        Ok(row.map(|r| (r.get::<_, i64>(0), r.get::<_, i64>(1))))
    }

    async fn set(&self, name: &str, a: i64, b: i64) -> Result<()> {
        let client = self.client().await?;
        client
            .execute(
                "INSERT INTO spot_bpm_trigger_mark (tenant, name, a, b) VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (tenant, name) DO UPDATE SET a = EXCLUDED.a, b = EXCLUDED.b",
                &[&self.tenant, &name, &a, &b],
            )
            .await
            .map(|_| ())
            .map_err(|e| Error(format!("saving a trigger mark failed: {e}")))
    }

    async fn position(&self) -> Result<FeedPosition> {
        match self.get("outbox").await? {
            Some((g, s)) => Ok(FeedPosition::new(
                u64::try_from(g).unwrap_or(0),
                u64::try_from(s).unwrap_or(0),
            )),
            None => Ok(FeedPosition::START),
        }
    }

    async fn set_position(&self, at: FeedPosition) -> Result<()> {
        let g = i64::try_from(at.generation()).unwrap_or(i64::MAX);
        let s = i64::try_from(at.seq()).unwrap_or(i64::MAX);
        self.set("outbox", g, s).await
    }
}

/// Counters, label-free.
#[derive(Debug, Default)]
struct Stats {
    started: AtomicU64,
    existing: AtomicU64,
    refused: AtomicU64,
}

/// A snapshot of the counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StatsSnapshot {
    /// New Cases started.
    pub started: u64,
    /// Starts that found their Case already there.
    pub existing: u64,
    /// Refusals (recursion, rate limit, denied facts).
    pub refused: u64,
}

#[derive(Default)]
struct State {
    index: Arc<TriggerIndex>,
    fingerprint: Option<String>,
    broken: usize,
    windows: HashMap<String, (i64, u32)>,
    rate_hit_at: Option<i64>,
    read_failures: HashMap<String, u32>,
    last_error: Option<String>,
}

/// What one batch did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Progress {
    /// The position of the last event taken in, when any.
    pub next: Option<FeedPosition>,
    /// Events taken in (whether or not a trigger watched them).
    pub seen: u32,
    /// Whether the batch stopped at an event to retry.
    pub held: bool,
}

enum Step {
    Done,
    Hold,
}

/// The trigger intake of one site.
pub struct TriggerHost {
    p: Parts,
    marks: Marks,
    stats: Stats,
    state: Mutex<State>,
}

fn to_jiff(at: OffsetDateTime) -> Result<jiff::Timestamp> {
    jiff::Timestamp::new(
        at.unix_timestamp(),
        i32::try_from(at.nanosecond()).unwrap_or(0),
    )
    .map_err(|e| {
        Error(format!(
            "the instant does not convert for the scheduler: {e}"
        ))
    })
}

/// The Case id a start key derives to: stable on every node, `[a-z][a-z0-9_]*`.
pub fn case_id(key: &StartKey) -> std::result::Result<CaseId, spot_bpm_triggers::Error> {
    let mut digest = Sha256::new();
    for part in [
        key.process.as_str(),
        key.node.as_str(),
        key.source_event.as_str(),
    ] {
        digest.update(part.as_bytes());
        digest.update([0u8]);
    }
    let mut text = String::from("t");
    for byte in digest.finalize().iter().take(16) {
        text.push_str(&format!("{byte:02x}"));
    }
    CaseId::new(&text).map_err(|_| start_failed(key, "the case id is invalid"))
}

fn start_failed(key: &StartKey, reason: &str) -> spot_bpm_triggers::Error {
    spot_bpm_triggers::Error::StartFailed {
        process: key.process.clone(),
        node: key.node.to_string(),
        reason: reason.to_owned(),
    }
}

/// Starts Cases for one source event, enforcing the recursion rule and the rate limit.
struct EventStarter {
    host: Arc<TriggerHost>,
    origin: Option<(String, u32)>,
    source: &'static str,
    who: &'static str,
}

#[async_trait]
impl CaseStarter for EventStarter {
    async fn start(
        &self,
        key: StartKey,
        input: StartInput,
    ) -> std::result::Result<StartOutcome, spot_bpm_triggers::Error> {
        let host = &self.host;
        let id = case_id(&key)?;
        let case = id.to_string();
        let entry = |outcome: &'static str, rule: &str| AuditEntry {
            site: host.p.site.as_str().to_owned(),
            source: self.source,
            outcome,
            rule: rule.to_owned(),
            process: key.process.clone(),
            node: key.node.to_string(),
            who: self.who,
        };
        let engine = host
            .p
            .process
            .engine()
            .map_err(|_| start_failed(&key, "the process engine is not ready"))?;
        let found = engine
            .store()
            .load(&id)
            .await
            .map_err(|_| start_failed(&key, "the case store failed"))?;
        if found.is_some() {
            host.stats.existing.fetch_add(1, Ordering::Relaxed);
            host.p.audit.record(entry("existing", "ok"));
            return Ok(StartOutcome::Existing { case });
        }
        if self
            .origin
            .as_ref()
            .is_some_and(|(p, v)| *p == key.process && *v == input.version)
        {
            host.refuse(entry("refused", "recursion"));
            return Ok(StartOutcome::Existing { case });
        }
        let now = host.p.clock.now_utc();
        if !host.admit(&key.process, now.unix_timestamp()) {
            host.refuse(entry("refused", "rate_limited"));
            return Ok(StartOutcome::Existing { case });
        }
        let definition = engine
            .processes()
            .get(&key.process, input.version)
            .ok_or_else(|| start_failed(&key, "the process version is not loaded"))?;
        let pin = Published {
            version: definition.compiled.dag.version(),
            hash: definition.compiled.dag.hash().clone(),
        };
        let planned = dag_start(
            &definition.compiled.dag,
            &pin,
            id.clone(),
            case.clone(),
            now,
        )
        .map_err(|_| start_failed(&key, "the case could not be planned"))?;
        let mut record = CaseRecord::new(planned.run, input.initiator.clone());
        record.variables = input.variables.clone();
        if engine.store().create(record).await.is_err() {
            // Another node may have won the race: its Case is this key's Case.
            let won = engine
                .store()
                .load(&id)
                .await
                .map_err(|_| start_failed(&key, "the case store failed"))?;
            if won.is_none() {
                return Err(start_failed(&key, "the case could not be stored"));
            }
            host.stats.existing.fetch_add(1, Ordering::Relaxed);
            host.p.audit.record(entry("existing", "ok"));
            return Ok(StartOutcome::Existing { case });
        }
        host.stats.started.fetch_add(1, Ordering::Relaxed);
        host.p.audit.record(entry("started", "ok"));
        let job = CaseJob::new(id, host.p.site.clone(), now);
        let spec = JobSpec {
            name: CASE_JOB_NAME.to_owned(),
            queue: "default".to_owned(),
            args: json!({ "case": case }),
            idempotency_key: job.idempotency_key(),
            not_before: Some(now),
        };
        // Best effort: the engine's resume scan finds the Case either way.
        let _ = host.p.submit.submit(spec).await;
        Ok(StartOutcome::Started { case })
    }
}

impl TriggerHost {
    /// Builds the intake. `prepare` must run before the first pass.
    #[must_use]
    pub fn new(parts: Parts) -> Arc<Self> {
        let marks = Marks {
            pool: parts.pool.clone(),
            tenant: parts.site.as_str().to_owned(),
        };
        Arc::new(Self {
            p: parts,
            marks,
            stats: Stats::default(),
            state: Mutex::new(State::default()),
        })
    }

    /// The site this intake serves.
    #[must_use]
    pub fn site(&self) -> &SiteId {
        &self.p.site
    }

    /// Creates the mark table (safe to repeat).
    ///
    /// # Errors
    /// The database refuses the table.
    pub async fn prepare(&self) -> Result<()> {
        self.marks.prepare().await
    }

    /// A snapshot of the counters.
    #[must_use]
    pub fn stats(&self) -> StatsSnapshot {
        StatsSnapshot {
            started: self.stats.started.load(Ordering::Relaxed),
            existing: self.stats.existing.load(Ordering::Relaxed),
            refused: self.stats.refused.load(Ordering::Relaxed),
        }
    }

    fn refuse(&self, entry: AuditEntry) {
        self.stats.refused.fetch_add(1, Ordering::Relaxed);
        if entry.rule == "rate_limited" {
            lock(&self.state).rate_hit_at = Some(self.p.clock.now_utc().unix_timestamp());
        }
        self.p.audit.record(entry);
    }

    /// Takes one new Case of `process` from this minute's budget.
    fn admit(&self, process: &str, now: i64) -> bool {
        let minute = now.div_euclid(60);
        let mut state = lock(&self.state);
        let slot = state
            .windows
            .entry(process.to_owned())
            .or_insert((minute, 0));
        if slot.0 != minute {
            *slot = (minute, 0);
        }
        if slot.1 >= self.p.settings.start_rate_per_minute {
            return false;
        }
        slot.1 += 1;
        true
    }

    fn index(&self) -> Arc<TriggerIndex> {
        Arc::clone(&lock(&self.state).index)
    }

    /// What an operator should know: disabled webhooks, the rate limit, a failing pass.
    #[must_use]
    pub fn warnings(&self) -> Vec<String> {
        let state = lock(&self.state);
        let now = self.p.clock.now_utc().unix_timestamp();
        let mut out = Vec::new();
        if state.broken > 0 {
            out.push(format!(
                "{} published version(s) have a trigger that does not load",
                state.broken
            ));
        }
        for entry in state.index.entries() {
            if let Some(hook) = entry.spec.as_webhook() {
                let configured = self.p.settings.webhooks.get(&hook.path).and_then(|w| {
                    self.p
                        .settings
                        .secrets
                        .contains_key(&w.secret)
                        .then_some(())
                });
                if configured.is_none() || !trigger_name(&hook.path) {
                    out.push(format!(
                        "the webhook trigger of process `{}` is disabled: no usable secret is configured",
                        entry.process
                    ));
                }
            }
        }
        if state.rate_hit_at.is_some_and(|at| now - at < 300) {
            out.push("a process hit its start rate limit in the last five minutes".to_owned());
        }
        if let Some(error) = &state.last_error {
            out.push(format!("the last intake pass failed: {error}"));
        }
        out
    }

    /// Rebuilds the trigger index from the latest published version of every process when the
    /// published set changed (a publish, a retire or a reload needs no restart).
    ///
    /// # Errors
    /// The definition store's failure; the previous index stays in force.
    pub async fn reload(&self) -> Result<usize> {
        let defs = &self.p.definitions;
        let store_err = |e: spot_bpm_definitions::DefinitionError| {
            Error(format!(
                "reading the process definitions failed: {}",
                e.code()
            ))
        };
        let mut latest: BTreeMap<String, StoredVersion> = BTreeMap::new();
        for name in defs.names().await.map_err(store_err)? {
            let mut after = None;
            loop {
                let page = defs.list(&name, after, MAX_PAGE).await.map_err(store_err)?;
                let Some(last) = page.last().map(|v| v.version) else {
                    break;
                };
                for stored in page {
                    if stored.state == VersionState::Published {
                        latest.insert(name.clone(), stored);
                    }
                }
                after = Some(last);
            }
        }
        let fingerprint = latest
            .values()
            .map(|v| format!("{}:{}:{}", v.name, v.version, v.content_hash))
            .collect::<Vec<_>>()
            .join(",");
        if lock(&self.state).fingerprint.as_deref() == Some(fingerprint.as_str()) {
            return Ok(lock(&self.state).index.entries().len());
        }
        let mut index = TriggerIndex::new();
        let mut broken = 0;
        for stored in latest.values() {
            let version = from_text(&stored.text).ok().map(|mut v| {
                v.version = stored.version;
                v.status = VersionStatus::Published;
                v
            });
            match version.and_then(|v| TriggerIndex::from_versions(&[v]).ok()) {
                Some(one) => {
                    for e in one.entries() {
                        index.add_spec(
                            &e.process,
                            &e.app,
                            e.version,
                            e.node.clone(),
                            e.spec.clone(),
                        );
                    }
                }
                None => broken += 1,
            }
        }
        let count = index.entries().len();
        let mut state = lock(&self.state);
        state.index = Arc::new(index);
        state.fingerprint = Some(fingerprint);
        state.broken = broken;
        Ok(count)
    }

    fn intake(
        &self,
        starter: EventStarter,
        stateful: BTreeSet<String>,
        loader: Option<Arc<Preloaded>>,
    ) -> TriggerIntake {
        let gate: Arc<dyn StartGate> = match self.p.meta.engine() {
            Some(engine) => Arc::new(PermissionGate::with_read_engine(engine)),
            None => Arc::new(PermissionGate::roles_only()),
        };
        let mut intake = TriggerIntake::new(
            self.index(),
            Arc::new(TriggerRegistry::with_builtin_kinds()),
            Arc::new(starter),
            gate,
        )
        .with_stateful_doctypes(stateful);
        if let Some(loader) = loader {
            intake = intake.with_document_loader(loader);
        }
        intake
    }

    fn starter(self: &Arc<Self>, source: &'static str, who: &'static str) -> EventStarter {
        EventStarter {
            host: Arc::clone(self),
            origin: None,
            source,
            who,
        }
    }

    fn watches(&self, doctype: &str, kind: EventType) -> bool {
        self.index().entries().iter().any(|e| {
            e.spec
                .as_document_event()
                .is_some_and(|d| d.doctype == doctype && d.event == kind)
        })
    }

    /// The `(process, version)` of the Case whose Document node wrote the event, from the
    /// command id the engine's idempotency key renders (`<case>:<node>:<attempt>`).
    async fn origin_of(&self, command: Option<&str>) -> Result<Option<(String, u32)>> {
        let Some(case) = command
            .and_then(|c| c.split(':').next())
            .and_then(|c| CaseId::new(c).ok())
        else {
            return Ok(None);
        };
        let engine = self.p.process.engine()?;
        let found = engine
            .store()
            .load(&case)
            .await
            .map_err(|e| Error(format!("reading the origin case failed: {}", e.code())))?;
        Ok(found.map(|r| (r.run.dag_key, r.run.dag_version)))
    }

    /// Takes a batch of outbox records in. Stops (`held`) at an event whose document cannot be
    /// read yet; the position must not pass it.
    ///
    /// # Errors
    /// An infrastructure failure (the case store, the engine); the batch is retried whole and the
    /// start keys make that safe.
    pub async fn ingest(self: &Arc<Self>, records: &[OutboxRecord]) -> Result<Progress> {
        let mut progress = Progress::default();
        for record in records {
            if matches!(self.ingest_one(record).await?, Step::Hold) {
                progress.held = true;
                return Ok(progress);
            }
            progress.next = Some(record.position);
            progress.seen += 1;
        }
        Ok(progress)
    }

    fn deny(&self, rule: &str, doctype: &str) {
        self.refuse(AuditEntry {
            site: self.p.site.as_str().to_owned(),
            source: "document",
            outcome: "refused",
            rule: rule.to_owned(),
            process: String::new(),
            node: doctype.to_owned(),
            who: "platform",
        });
    }

    async fn ingest_one(self: &Arc<Self>, record: &OutboxRecord) -> Result<Step> {
        let event = &record.event;
        let Some(kind) = EventType::from_topic(event.topic.as_str()) else {
            return Ok(Step::Done);
        };
        let doctype = event.subject.entity.as_str().to_owned();
        if !self.watches(&doctype, kind) {
            return Ok(Step::Done);
        }
        let Ok(payload) = serde_json::from_str::<PayloadDocument>(&event.payload) else {
            self.deny("payload_invalid", &doctype);
            return Ok(Step::Done);
        };
        let Ok(topic) = Topic::any(event.topic.as_str()) else {
            self.deny("topic_invalid", &doctype);
            return Ok(Step::Done);
        };
        // Unreadable metadata denies every document start (fail closed): never an empty set.
        let Some(stateful) = self.p.meta.stateful() else {
            self.deny("metadata_unreadable", &doctype);
            return Ok(Step::Done);
        };
        let event_id = event.id.to_string();
        let name = event.subject.id.to_string();
        let found = match self.p.reader.read(&doctype, &name).await {
            Ok(found) => {
                lock(&self.state).read_failures.remove(&event_id);
                found
            }
            Err(_) => {
                let mut state = lock(&self.state);
                let tries = state.read_failures.entry(event_id.clone()).or_insert(0);
                *tries += 1;
                if *tries < READ_ATTEMPTS {
                    return Ok(Step::Hold);
                }
                state.read_failures.remove(&event_id);
                None
            }
        };
        // A document the platform cannot show has no values to judge: deny, whoever the actor is.
        let Some(found) = found else {
            self.deny("document_unreadable", &doctype);
            return Ok(Step::Done);
        };
        let loader = Arc::new(Preloaded(Mutex::new(HashMap::new())));
        lock(&loader.0).insert(event_id.clone(), doc_facts(&found));
        let (principal, who) = match &payload.meta.actor {
            Actor::User { id, .. } => (self.p.principals.resolve(id).await, "user"),
            _ => (Principal::system(), "platform"),
        };
        let command = payload
            .meta
            .command_id
            .as_ref()
            .map(|c| c.as_str().to_owned());
        let origin = match self.origin_of(command.as_deref()).await {
            Ok(origin) => origin,
            Err(_) => return Ok(Step::Hold),
        };
        let committed = spot_webhooks::CommittedEvent {
            event_id,
            topic,
            doctype: doctype.clone(),
            document_id: name,
            document_version: event.subject_version.map_or(0, |v| v.0),
            event_type: kind,
            occurred_at: payload.meta.occurred_at,
            after_image: doc_image(&found),
            before_image: None,
        };
        let mut starter = self.starter("document", who);
        starter.origin = origin;
        let intake = self.intake(starter, stateful, Some(loader));
        let report = intake
            .process(&SourceEvent::document(committed, principal))
            .await
            .map_err(|e| {
                Error(format!(
                    "starting from a document event failed: {}",
                    e.code().as_str()
                ))
            })?;
        for rejected in report.rejected {
            let rule: String = rejected
                .reason
                .split([':', ' '])
                .next()
                .unwrap_or("rejected")
                .chars()
                .take(64)
                .collect();
            self.refuse(AuditEntry {
                site: self.p.site.as_str().to_owned(),
                source: "document",
                outcome: "refused",
                rule,
                process: rejected.process,
                node: rejected.node,
                who,
            });
        }
        Ok(Step::Done)
    }

    /// Reads the outbox from the durable position until it is drained, taking each batch in and
    /// saving the position after it.
    ///
    /// # Errors
    /// The outbox, the marks or an infrastructure failure of the intake.
    pub async fn pump(self: &Arc<Self>, outbox: &dyn OutboxLease) -> Result<u32> {
        let mut total = 0;
        let page_size = usize::try_from(self.p.settings.batch).unwrap_or(100);
        loop {
            let after = self.marks.position().await?;
            let page = outbox
                .read(after, self.p.settings.batch)
                .await
                .map_err(|e| Error(format!("reading the outbox failed: {e}")))?;
            if page.is_empty() {
                break;
            }
            let progress = self.ingest(&page).await?;
            if let Some(next) = progress.next {
                self.marks.set_position(next).await?;
                total += progress.seen;
            }
            if progress.held || page.len() < page_size {
                break;
            }
        }
        Ok(total)
    }

    /// Plans and starts the schedule slots due at `now`. A schedule seen for the first time is
    /// marked and fires from its next slot.
    ///
    /// # Errors
    /// The marks or an infrastructure failure of the intake; the mark stays, so the slots are
    /// planned again under the same keys.
    pub async fn tick_schedules(self: &Arc<Self>, now: OffsetDateTime) -> Result<u32> {
        let ts_now = to_jiff(now)?;
        let mut fired = 0;
        for entry in self.index().entries() {
            let Some(spec) = entry.spec.as_schedule() else {
                continue;
            };
            let mark = format!("schedule:{}:{}", entry.process, entry.node);
            let Some((last, _)) = self.marks.get(&mark).await? else {
                self.marks.set(&mark, now.unix_timestamp(), 0).await?;
                continue;
            };
            let from = jiff::Timestamp::from_second(last)
                .map_err(|e| Error(format!("a schedule mark is out of range: {e}")))?;
            let slots = plan(
                &spec.schedule,
                &spec.app,
                Some(from),
                ts_now,
                spec.policy,
                SlotRef::from_planned(&spec.app, &spec.schedule),
            );
            for slot in slots {
                let intake =
                    self.intake(self.starter("schedule", "platform"), BTreeSet::new(), None);
                intake
                    .process(&SourceEvent::system(Source::Schedule(slot)))
                    .await
                    .map_err(|e| {
                        Error(format!(
                            "starting a schedule slot failed: {}",
                            e.code().as_str()
                        ))
                    })?;
                fired += 1;
            }
            self.marks.set(&mark, now.unix_timestamp(), 0).await?;
        }
        Ok(fired)
    }

    /// One intake pass: reload, outbox, schedules. A failure is remembered for health.
    ///
    /// # Errors
    /// The first failing step.
    pub async fn cycle(self: &Arc<Self>, outbox: &dyn OutboxLease) -> Result<()> {
        let result: Result<()> = async {
            self.reload().await?;
            self.pump(outbox).await?;
            self.tick_schedules(self.p.clock.now_utc()).await?;
            Ok(())
        }
        .await;
        lock(&self.state).last_error = result.as_ref().err().map(|e| e.to_string());
        result
    }

    fn hook_entry(&self, trigger: &str) -> Option<String> {
        let configured = self.p.settings.webhooks.get(trigger)?;
        let known = self
            .index()
            .entries()
            .iter()
            .any(|e| e.spec.as_webhook().is_some_and(|h| h.path == trigger));
        known.then(|| configured.secret.clone())
    }

    /// Receives one webhook delivery. Every refusal is the same `WebhookRejected` after the same
    /// work: the secret is looked up and the MAC computed whatever the cause.
    ///
    /// # Errors
    /// [`WebhookRejected`], for every cause.
    pub async fn receive_hook(
        self: &Arc<Self>,
        site: &str,
        trigger: &str,
        delivery_id: Option<&str>,
        signature: &str,
        body: &[u8],
    ) -> std::result::Result<(), WebhookRejected> {
        let refused = |rule: &str, process: &str| {
            self.refuse(AuditEntry {
                site: self.p.site.as_str().to_owned(),
                source: "webhook",
                outcome: "refused",
                rule: rule.to_owned(),
                process: process.to_owned(),
                node: String::new(),
                who: "caller",
            });
            WebhookRejected
        };
        let reference = if site == self.p.site.as_str() && trigger_name(trigger) {
            self.hook_entry(trigger)
        } else {
            None
        };
        let secret = match &reference {
            Some(reference) => self.p.secrets.secret(reference).await,
            None => None,
        };
        let usable = secret.is_some();
        let key = secret.unwrap_or_else(|| DECOY.to_vec());
        let id_ok = delivery_id.is_none_or(delivery_id_ok);
        let call = IncomingWebhook {
            path: trigger,
            signature: (!signature.is_empty()).then_some(signature),
            idempotency_key: delivery_id.filter(|_| id_ok),
            body,
        };
        let verified = accept(
            &key,
            &call,
            self.p.clock.now_utc(),
            Some(self.p.settings.webhook_tolerance_secs),
        );
        let Ok(event) = verified else {
            return Err(refused("verification_failed", ""));
        };
        if reference.is_none() || !usable || !id_ok {
            return Err(refused("trigger_unavailable", ""));
        }
        let intake = self.intake(self.starter("webhook", "caller"), BTreeSet::new(), None);
        match intake
            .process(&SourceEvent::system(Source::Webhook(event)))
            .await
        {
            Ok(_) => Ok(()),
            Err(_) => Err(refused("start_failed", "")),
        }
    }
}

/// The public webhook receiver of one site.
pub struct HookPort {
    host: Arc<TriggerHost>,
}

impl HookPort {
    /// A receiver over `host`.
    #[must_use]
    pub fn new(host: Arc<TriggerHost>) -> Self {
        Self { host }
    }
}

impl WebhookPort for HookPort {
    fn receive<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        trigger: &'a str,
        delivery_id: Option<&'a str>,
        signature: &'a str,
        body: Bytes,
    ) -> BoxFuture<'a, std::result::Result<(), WebhookRejected>> {
        Box::pin(async move {
            self.host
                .receive_hook(ctx.site.as_str(), trigger, delivery_id, signature, &body)
                .await
        })
    }
}
