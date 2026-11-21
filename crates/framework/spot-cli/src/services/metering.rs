//! The metering emitter (WP-CTL-05.T6, ADR-061): the optional tenant-host service that counts
//! active users, transactions, storage and AI tokens and ships them to the control plane
//! (`POST /cp/v1/host/usage`). **Off by default** (`[services.metering] enabled = false`): a desktop
//! tenant without a control plane makes no network call and keeps working.
//!
//! # Never in the request path, never fails a write
//!
//! Nothing here is called by a request. A background task samples counters the host already keeps
//! (read through [`UsageSources`], cheap queries run by that task) and writes events to a **local
//! outbox table** in the tenant's own database; a second step ships them. A write posted while the
//! control plane is down, or while this task is failing, is unaffected: the outbox absorbs the events.
//!
//! # Idempotent ids, durable batches
//!
//! An event id is `tenant:meter:bucket` (amounts, one per bucket of `interval_seconds`) or
//! `tenant:active_users:digest:day` (one per user per UTC day), so a restart or a second sample of the
//! same bucket cannot count twice, here or at the control plane. A cursor per counter moves **in the
//! same transaction** as the event it produced, and only when the event was new: a counter delta is
//! either in an event or still pending in the cursor, never lost and never doubled. The first sample
//! of a counter only sets its baseline (history before the service was enabled is not billed).
//!
//! # Offline is fine
//!
//! A batch that gets no answer, or a 429 or 5xx, stays in the outbox and is shipped later. A refusal
//! that retrying cannot fix (400, 413, 422) drops that batch and counts it. The outbox is bounded
//! (`outbox_max`): when full, the oldest amount events of one meter are **summarised** into one event
//! (counters add, the gauge keeps its latest value), never dropped. If it still cannot shrink (only
//! user events remain) readiness turns degraded, an optional service's state; no user sees an error.
//!
//! # No personal data
//!
//! Active users are sent as salted SHA-256 digests (the per-site salt comes from the host), counts and
//! digests only: never a user name, e-mail or id. The salt and the digests are never logged. The
//! transport is mutual TLS, outbound only; the certificate files are checked for their modes
//! ([`check_credentials`]).
//!
//! Nothing here reads the wall clock (every instant is an argument), draws randomness or uses floats.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use deadpool_postgres::Pool;
use deadpool_postgres::tokio_postgres::types::ToSql;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use spot_core::Clock;
use spot_sync_http::{HttpRequest, Transport};
use tokio::sync::watch;

use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

/// Start order: late, after the services whose counters it reads.
pub const ORDER: u32 = 1700;

/// Events per batch the control plane accepts.
pub const MAX_EVENTS: usize = 1000;
/// Bytes per batch the control plane accepts.
pub const MAX_BODY: usize = 256 * 1024;
/// The shipped events kept to absorb a resample of the same bucket: two days.
const SHIPPED_KEEP_SECS: i64 = 2 * 86_400;
const DAY: i64 = 86_400;

/// `[services.metering]`. Unknown keys are refused.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Whether the emitter runs. Off by default.
    pub enabled: bool,
    /// This tenant's id, part of every event id (at most 64 characters of `A-Za-z0-9_-`).
    pub tenant: String,
    /// The control plane's host listener, `https://host:port`.
    pub control_plane: String,
    /// The CA that signed the control plane's server certificate (PEM), optional.
    pub ca_cert: String,
    /// This host's client certificate issued by the control plane's host CA (PEM).
    pub host_cert: String,
    /// The client key (PEM), mode 0600.
    pub host_key: String,
    /// The sampling bucket and the shipping period, seconds. At least 60.
    pub interval_seconds: u64,
    /// Events the local outbox holds before it summarises. At least 100.
    pub outbox_max: usize,
    /// Events per shipped batch, at most 1000. At least 1.
    pub batch: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            tenant: String::new(),
            control_plane: String::new(),
            ca_cert: String::new(),
            host_cert: String::new(),
            host_key: String::new(),
            interval_seconds: 3_600,
            outbox_max: 10_000,
            batch: 500,
        }
    }
}

impl Settings {
    /// Checks the ranges of an enabled configuration.
    ///
    /// # Errors
    /// A message naming the setting, never a value.
    pub fn validate(&self) -> Result<()> {
        let bad = |name: &str, rule: &str| Error(format!("services.metering.{name} {rule}"));
        let tenant_ok = !self.tenant.is_empty()
            && self.tenant.len() <= 64
            && self
                .tenant
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
        if !tenant_ok {
            return Err(bad("tenant", "must be 1 to 64 characters of A-Za-z0-9_-"));
        }
        if !self.control_plane.starts_with("https://") {
            return Err(bad("control_plane", "must be an https:// URL"));
        }
        if self.host_cert.is_empty() || self.host_key.is_empty() {
            return Err(bad("host_cert", "and host_key are required"));
        }
        if self.interval_seconds < 60 {
            return Err(bad("interval_seconds", "must be at least 60"));
        }
        if self.outbox_max < 100 {
            return Err(bad("outbox_max", "must be at least 100"));
        }
        if self.batch == 0 || self.batch > MAX_EVENTS {
            return Err(bad("batch", "must be 1 to 1000"));
        }
        Ok(())
    }
}

/// Checks the credential files: they exist, and the private key is not readable by group or others.
///
/// # Errors
/// A message naming the setting, never a path's content.
pub fn check_credentials(settings: &Settings) -> std::result::Result<(), String> {
    for (name, path) in [
        ("host_cert", &settings.host_cert),
        ("host_key", &settings.host_key),
    ] {
        if !Path::new(path).is_file() {
            return Err(format!(
                "services.metering.{name} does not name a readable file"
            ));
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&settings.host_key)
            .map_err(|_| "services.metering.host_key cannot be read".to_owned())?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            return Err("services.metering.host_key must be mode 0600 or stricter".to_owned());
        }
    }
    Ok(())
}

// ---- sources -------------------------------------------------------------------------------

/// The counters the host keeps.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    /// Posted transactions since the site began: monotonic, may reset to a smaller value.
    pub transactions: u64,
    /// AI tokens settled since the site began: monotonic, may reset.
    pub ai_tokens: u64,
    /// Storage in use now, bytes (a level, not a counter).
    pub storage_bytes: u64,
}

/// Where the numbers come from: cheap reads of what the host already counts, run by the emitter's
/// own task.
pub trait UsageSources: Send + Sync {
    /// The counters now.
    ///
    /// # Errors
    /// A message (no values) when the source cannot answer; the sample is skipped.
    fn counters(&self) -> std::result::Result<Counters, String>;

    /// The ids of the users active in `[from, until]` (Unix seconds). They are hashed with the
    /// site salt before they leave this module.
    ///
    /// # Errors
    /// As [`UsageSources::counters`].
    fn active_users(&self, from: i64, until: i64) -> std::result::Result<Vec<String>, String>;
}

/// The per-site salt of the active-user digests (a secret of this site).
pub trait SiteSalt: Send + Sync {
    /// The salt bytes; never empty.
    fn salt(&self) -> Vec<u8>;
}

/// The digest of one user for one site: `hex(sha256(salt || 0x00 || id))`.
#[must_use]
pub fn member_digest(salt: &[u8], user: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(salt);
    hasher.update([0u8]);
    hasher.update(user.as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

// ---- events and the plan -------------------------------------------------------------------

/// One event in the outbox, as it goes on the wire.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutboxEvent {
    /// The idempotency key.
    pub id: String,
    /// The meter: `active_users`, `transactions`, `storage_bytes` or `ai_tokens`.
    pub meter: String,
    /// The amount (0 for a member event).
    pub amount: u64,
    /// The salted digest of a user (`active_users` only).
    pub member: Option<String>,
    /// Unix seconds.
    pub at: i64,
}

/// One planned write: an event (or none) and the cursor move it carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Planned {
    /// The event, or `None` when only the cursor moves (a zero delta, the first baseline).
    pub event: Option<OutboxEvent>,
    /// The counter whose cursor moves, with its new value; `None` for gauges and members.
    pub cursor: Option<(&'static str, u64)>,
}

/// What the cursors held before this sample.
pub type Cursors = BTreeMap<String, u64>;

/// Plans one sampling pass at `now` (Unix seconds). Pure: no store, no clock.
#[must_use]
pub fn plan(
    tenant: &str,
    now: i64,
    interval: i64,
    cursors: &Cursors,
    counters: Counters,
    active: &[String],
    salt: &[u8],
) -> Vec<Planned> {
    let bucket = now.div_euclid(interval.max(1));
    let mut out = Vec::new();
    for (meter, current) in [
        ("transactions", counters.transactions),
        ("ai_tokens", counters.ai_tokens),
    ] {
        match cursors.get(meter) {
            // The first sample: the baseline only.
            None => out.push(Planned {
                event: None,
                cursor: Some((meter, current)),
            }),
            Some(last) => {
                let delta = if current >= *last {
                    current - last
                } else {
                    current
                };
                let event = (delta > 0).then(|| OutboxEvent {
                    id: format!("{tenant}:{meter}:{bucket}"),
                    meter: meter.to_owned(),
                    amount: delta,
                    member: None,
                    at: now,
                });
                out.push(Planned {
                    event,
                    cursor: Some((meter, current)),
                });
            }
        }
    }
    out.push(Planned {
        event: Some(OutboxEvent {
            id: format!("{tenant}:storage_bytes:{bucket}"),
            meter: "storage_bytes".to_owned(),
            amount: counters.storage_bytes,
            member: None,
            at: now,
        }),
        cursor: None,
    });
    let day = now.div_euclid(DAY);
    let mut seen = std::collections::BTreeSet::new();
    for user in active {
        let digest = member_digest(salt, user);
        if !seen.insert(digest.clone()) {
            continue;
        }
        out.push(Planned {
            event: Some(OutboxEvent {
                id: format!("{tenant}:active_users:{}:{day}", &digest[..32]),
                meter: "active_users".to_owned(),
                amount: 0,
                member: Some(digest),
                at: now,
            }),
            cursor: None,
        });
    }
    out
}

/// Plans the summarising of an outbox over its bound: which rows to remove and which to insert.
/// Counters add, the gauge keeps its latest value; user events are never merged or dropped.
/// Returns `None` when nothing can be summarised.
#[must_use]
pub fn plan_compaction(
    pending: &[OutboxEvent],
    excess: usize,
) -> Option<(Vec<String>, Vec<OutboxEvent>)> {
    let mut by_meter: BTreeMap<&str, Vec<&OutboxEvent>> = BTreeMap::new();
    for event in pending.iter().filter(|e| e.member.is_none()) {
        by_meter
            .entry(event.meter.as_str())
            .or_default()
            .push(event);
    }
    let mut remove = Vec::new();
    let mut insert = Vec::new();
    let mut freed = 0usize;
    for (meter, mut events) in by_meter {
        if freed >= excess {
            break;
        }
        events.sort_by(|a, b| a.at.cmp(&b.at).then(a.id.cmp(&b.id)));
        if events.len() < 2 {
            continue;
        }
        let first = events.first()?;
        let last = events.last()?;
        let amount = if meter == "storage_bytes" {
            last.amount
        } else {
            events
                .iter()
                .fold(0u64, |sum, e| sum.saturating_add(e.amount))
        };
        let tenant_prefix = first.id.split(':').next().unwrap_or("");
        let id = format!("{tenant_prefix}:{meter}:summary:{}-{}", first.at, last.at);
        freed += events.len() - 1;
        remove.extend(events.iter().map(|e| e.id.clone()));
        insert.push(OutboxEvent {
            id,
            meter: meter.to_owned(),
            amount,
            member: None,
            at: last.at,
        });
    }
    (!insert.is_empty()).then_some((remove, insert))
}

/// The JSON body of one batch.
#[must_use]
pub fn wire_body(events: &[OutboxEvent]) -> Vec<u8> {
    let list: Vec<serde_json::Value> = events
        .iter()
        .map(|e| {
            let mut object = serde_json::Map::new();
            object.insert("id".to_owned(), e.id.clone().into());
            object.insert("meter".to_owned(), e.meter.clone().into());
            match &e.member {
                Some(member) => {
                    object.insert("member".to_owned(), member.clone().into());
                }
                None => {
                    object.insert("amount".to_owned(), e.amount.into());
                }
            }
            object.insert("at".to_owned(), e.at.into());
            serde_json::Value::Object(object)
        })
        .collect();
    serde_json::to_vec(&serde_json::json!({ "events": list })).unwrap_or_default()
}

// ---- the outbox ----------------------------------------------------------------------------

/// The durable local outbox.
#[async_trait]
pub trait UsageOutbox: Send + Sync {
    /// The cursors (counter name to last value).
    ///
    /// # Errors
    /// A message when the store cannot answer.
    async fn cursors(&self) -> std::result::Result<Cursors, String>;

    /// Applies planned writes atomically: each event is inserted unless its id exists; its cursor
    /// moves only when the event was new (or there is no event). Returns the events inserted.
    ///
    /// # Errors
    /// A message when the store cannot answer.
    async fn append(&self, planned: &[Planned]) -> std::result::Result<usize, String>;

    /// The oldest unshipped events, at most `limit`.
    ///
    /// # Errors
    /// A message when the store cannot answer.
    async fn pending(&self, limit: usize) -> std::result::Result<Vec<OutboxEvent>, String>;

    /// How many events are unshipped.
    ///
    /// # Errors
    /// A message when the store cannot answer.
    async fn unshipped(&self) -> std::result::Result<usize, String>;

    /// Marks events shipped (kept to absorb a resample until purged).
    ///
    /// # Errors
    /// A message when the store cannot answer.
    async fn mark_shipped(&self, ids: &[String]) -> std::result::Result<(), String>;

    /// Removes shipped events older than `before` (Unix seconds).
    ///
    /// # Errors
    /// A message when the store cannot answer.
    async fn purge_shipped(&self, before: i64) -> std::result::Result<(), String>;

    /// Removes `remove` and inserts `insert` in one transaction (summarising).
    ///
    /// # Errors
    /// A message when the store cannot answer.
    async fn replace(
        &self,
        remove: &[String],
        insert: &[OutboxEvent],
    ) -> std::result::Result<(), String>;
}

#[derive(Default)]
struct MemoryState {
    events: Vec<(OutboxEvent, bool)>,
    cursors: Cursors,
}

/// An in-memory outbox: the reference for the contract and for tests.
#[derive(Default)]
pub struct MemoryOutbox {
    state: Mutex<MemoryState>,
}

impl MemoryOutbox {
    /// An empty outbox.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every event with its shipped flag, for tests.
    #[must_use]
    pub fn all(&self) -> Vec<(OutboxEvent, bool)> {
        self.lock().events.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, MemoryState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[async_trait]
impl UsageOutbox for MemoryOutbox {
    async fn cursors(&self) -> std::result::Result<Cursors, String> {
        Ok(self.lock().cursors.clone())
    }

    async fn append(&self, planned: &[Planned]) -> std::result::Result<usize, String> {
        let mut state = self.lock();
        let mut inserted = 0;
        for item in planned {
            let new = match &item.event {
                Some(event) if state.events.iter().any(|(e, _)| e.id == event.id) => false,
                Some(event) => {
                    state.events.push((event.clone(), false));
                    inserted += 1;
                    true
                }
                None => true,
            };
            if new && let Some((meter, value)) = item.cursor {
                state.cursors.insert(meter.to_owned(), value);
            }
        }
        Ok(inserted)
    }

    async fn pending(&self, limit: usize) -> std::result::Result<Vec<OutboxEvent>, String> {
        let state = self.lock();
        let mut open: Vec<OutboxEvent> = state
            .events
            .iter()
            .filter(|(_, shipped)| !shipped)
            .map(|(event, _)| event.clone())
            .collect();
        open.sort_by(|a, b| a.at.cmp(&b.at).then(a.id.cmp(&b.id)));
        open.truncate(limit);
        Ok(open)
    }

    async fn unshipped(&self) -> std::result::Result<usize, String> {
        Ok(self.lock().events.iter().filter(|(_, s)| !s).count())
    }

    async fn mark_shipped(&self, ids: &[String]) -> std::result::Result<(), String> {
        for (event, shipped) in &mut self.lock().events {
            if ids.contains(&event.id) {
                *shipped = true;
            }
        }
        Ok(())
    }

    async fn purge_shipped(&self, before: i64) -> std::result::Result<(), String> {
        self.lock()
            .events
            .retain(|(e, shipped)| !(*shipped && e.at < before));
        Ok(())
    }

    async fn replace(
        &self,
        remove: &[String],
        insert: &[OutboxEvent],
    ) -> std::result::Result<(), String> {
        let mut state = self.lock();
        state.events.retain(|(e, _)| !remove.contains(&e.id));
        for event in insert {
            if !state.events.iter().any(|(e, _)| e.id == event.id) {
                state.events.push((event.clone(), false));
            }
        }
        Ok(())
    }
}

/// The outbox in the tenant's own PostgreSQL database.
pub struct PgOutbox {
    pool: Pool,
    events: String,
    cursor: String,
}

impl PgOutbox {
    /// An outbox over `pool` whose tables are `<prefix>_outbox` and `<prefix>_cursor`.
    ///
    /// # Errors
    /// A message when `prefix` is not `[a-z][a-z0-9_]{0,40}`.
    pub fn new(pool: Pool, prefix: &str) -> std::result::Result<Self, String> {
        let ok = !prefix.is_empty()
            && prefix.len() <= 41
            && prefix.starts_with(|c: char| c.is_ascii_lowercase())
            && prefix
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
        if !ok {
            return Err("the table prefix is not a plain identifier".to_owned());
        }
        Ok(Self {
            pool,
            events: format!("{prefix}_outbox"),
            cursor: format!("{prefix}_cursor"),
        })
    }

    /// Creates the tables (idempotent).
    ///
    /// # Errors
    /// A message when the database cannot be reached.
    pub async fn prepare(&self) -> std::result::Result<(), String> {
        let client = self.pool.get().await.map_err(|_| unavailable())?;
        let statements = [
            format!(
                "CREATE TABLE IF NOT EXISTS {e} (event_id text PRIMARY KEY, meter text NOT NULL, \
                 amount bigint NOT NULL CHECK (amount >= 0), member text, at bigint NOT NULL, \
                 shipped boolean NOT NULL DEFAULT false)",
                e = self.events
            ),
            format!(
                "CREATE TABLE IF NOT EXISTS {c} (meter text PRIMARY KEY, last bigint NOT NULL CHECK (last >= 0))",
                c = self.cursor
            ),
            format!(
                "CREATE INDEX IF NOT EXISTS {e}_pending ON {e} (at, event_id) WHERE NOT shipped",
                e = self.events
            ),
        ];
        for statement in &statements {
            client
                .batch_execute(statement)
                .await
                .map_err(|_| unavailable())?;
        }
        Ok(())
    }
}

fn unavailable() -> String {
    "the usage outbox is unavailable".to_owned()
}

fn to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn to_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

#[async_trait]
impl UsageOutbox for PgOutbox {
    async fn cursors(&self) -> std::result::Result<Cursors, String> {
        let client = self.pool.get().await.map_err(|_| unavailable())?;
        let rows = client
            .query(&format!("SELECT meter, last FROM {}", self.cursor), &[])
            .await
            .map_err(|_| unavailable())?;
        Ok(rows
            .iter()
            .map(|row| (row.get::<_, String>(0), to_u64(row.get::<_, i64>(1))))
            .collect())
    }

    async fn append(&self, planned: &[Planned]) -> std::result::Result<usize, String> {
        let mut client = self.pool.get().await.map_err(|_| unavailable())?;
        let tx = client.transaction().await.map_err(|_| unavailable())?;
        let insert = format!(
            "INSERT INTO {} (event_id, meter, amount, member, at) VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (event_id) DO NOTHING",
            self.events
        );
        let upsert = format!(
            "INSERT INTO {c} (meter, last) VALUES ($1, $2) ON CONFLICT (meter) DO UPDATE SET last = EXCLUDED.last",
            c = self.cursor
        );
        let mut inserted = 0;
        for item in planned {
            let new = match &item.event {
                Some(event) => {
                    let amount = to_i64(event.amount);
                    let params: [&(dyn ToSql + Sync); 5] =
                        [&event.id, &event.meter, &amount, &event.member, &event.at];
                    let n = tx
                        .execute(&insert, &params)
                        .await
                        .map_err(|_| unavailable())?;
                    inserted += usize::try_from(n).unwrap_or(0);
                    n > 0
                }
                None => true,
            };
            if new && let Some((meter, value)) = item.cursor {
                let value = to_i64(value);
                tx.execute(&upsert, &[&meter, &value])
                    .await
                    .map_err(|_| unavailable())?;
            }
        }
        tx.commit().await.map_err(|_| unavailable())?;
        Ok(inserted)
    }

    async fn pending(&self, limit: usize) -> std::result::Result<Vec<OutboxEvent>, String> {
        let client = self.pool.get().await.map_err(|_| unavailable())?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = client
            .query(
                &format!(
                    "SELECT event_id, meter, amount, member, at FROM {} WHERE NOT shipped \
                     ORDER BY at, event_id LIMIT $1",
                    self.events
                ),
                &[&limit],
            )
            .await
            .map_err(|_| unavailable())?;
        Ok(rows
            .iter()
            .map(|row| OutboxEvent {
                id: row.get(0),
                meter: row.get(1),
                amount: to_u64(row.get::<_, i64>(2)),
                member: row.get(3),
                at: row.get(4),
            })
            .collect())
    }

    async fn unshipped(&self) -> std::result::Result<usize, String> {
        let client = self.pool.get().await.map_err(|_| unavailable())?;
        let row = client
            .query_one(
                &format!("SELECT count(*) FROM {} WHERE NOT shipped", self.events),
                &[],
            )
            .await
            .map_err(|_| unavailable())?;
        Ok(usize::try_from(row.get::<_, i64>(0)).unwrap_or(0))
    }

    async fn mark_shipped(&self, ids: &[String]) -> std::result::Result<(), String> {
        let client = self.pool.get().await.map_err(|_| unavailable())?;
        let ids: Vec<String> = ids.to_vec();
        client
            .execute(
                &format!(
                    "UPDATE {} SET shipped = true WHERE event_id = ANY($1)",
                    self.events
                ),
                &[&ids],
            )
            .await
            .map_err(|_| unavailable())?;
        Ok(())
    }

    async fn purge_shipped(&self, before: i64) -> std::result::Result<(), String> {
        let client = self.pool.get().await.map_err(|_| unavailable())?;
        client
            .execute(
                &format!("DELETE FROM {} WHERE shipped AND at < $1", self.events),
                &[&before],
            )
            .await
            .map_err(|_| unavailable())?;
        Ok(())
    }

    async fn replace(
        &self,
        remove: &[String],
        insert: &[OutboxEvent],
    ) -> std::result::Result<(), String> {
        let mut client = self.pool.get().await.map_err(|_| unavailable())?;
        let tx = client.transaction().await.map_err(|_| unavailable())?;
        let remove: Vec<String> = remove.to_vec();
        tx.execute(
            &format!("DELETE FROM {} WHERE event_id = ANY($1)", self.events),
            &[&remove],
        )
        .await
        .map_err(|_| unavailable())?;
        for event in insert {
            let amount = to_i64(event.amount);
            tx.execute(
                &format!(
                    "INSERT INTO {} (event_id, meter, amount, member, at) VALUES ($1, $2, $3, $4, $5) \
                     ON CONFLICT (event_id) DO NOTHING",
                    self.events
                ),
                &[&event.id, &event.meter, &amount, &event.member, &event.at],
            )
            .await
            .map_err(|_| unavailable())?;
        }
        tx.commit().await.map_err(|_| unavailable())
    }
}

// ---- shipping ------------------------------------------------------------------------------

/// What the control plane did with a batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShipOutcome {
    /// Accepted (2xx).
    Accepted,
    /// Refused for good (400, 413, 422): retrying the same batch cannot help.
    Refused,
    /// No answer, 429, 5xx, or a refusal about the host rather than the batch (401, 403, 404): try
    /// again later; the events stay.
    Retry,
}

/// Sends one batch.
#[async_trait]
pub trait UsageShipper: Send + Sync {
    /// Posts `body` to `/cp/v1/host/usage`.
    async fn ship(&self, body: Vec<u8>) -> ShipOutcome;
}

/// A shipper over a mutual-TLS transport the host built from `host_cert` and `host_key` (pinning the
/// control plane's CA).
pub struct TransportShipper {
    transport: Arc<dyn Transport>,
}

impl TransportShipper {
    /// A shipper over `transport`.
    #[must_use]
    pub fn new(transport: Arc<dyn Transport>) -> Self {
        Self { transport }
    }
}

#[async_trait]
impl UsageShipper for TransportShipper {
    async fn ship(&self, body: Vec<u8>) -> ShipOutcome {
        let request =
            HttpRequest::post("/cp/v1/host/usage", body).with_content_type("application/json");
        match self.transport.request(request).await {
            Ok(response) if response.is_success() => ShipOutcome::Accepted,
            Ok(response) => match response.status {
                400 | 413 | 422 => ShipOutcome::Refused,
                _ => ShipOutcome::Retry,
            },
            Err(_) => ShipOutcome::Retry,
        }
    }
}

// ---- the service ---------------------------------------------------------------------------

/// What the emitter needs from the rest of the host. Provide one with
/// `SiteCtx::provide::<MeteringPorts>`; enabled with no provider the site refuses to start.
pub struct MeteringPorts {
    /// The counters.
    pub sources: Arc<dyn UsageSources>,
    /// The per-site salt.
    pub salt: Arc<dyn SiteSalt>,
    /// The local outbox.
    pub outbox: Arc<dyn UsageOutbox>,
    /// The mutual-TLS shipper.
    pub shipper: Arc<dyn UsageShipper>,
}

/// What one pass did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PassReport {
    /// Events written to the outbox.
    pub recorded: usize,
    /// Events the control plane accepted.
    pub shipped: usize,
    /// Events dropped because the control plane refused their batch.
    pub refused: usize,
    /// Events left for later (no answer, or batches not yet tried).
    pub waiting: usize,
}

/// The emitter.
pub struct MeteringService {
    ports: Arc<MeteringPorts>,
    settings: Settings,
    clock: Arc<dyn Clock>,
    degraded: AtomicBool,
}

impl MeteringService {
    /// An emitter over `ports`, reading time from `clock`.
    #[must_use]
    pub fn new(ports: Arc<MeteringPorts>, settings: Settings, clock: Arc<dyn Clock>) -> Self {
        Self {
            ports,
            settings,
            clock,
            degraded: AtomicBool::new(false),
        }
    }

    /// Whether the outbox is full and cannot shrink.
    #[must_use]
    pub fn degraded(&self) -> bool {
        self.degraded.load(Ordering::SeqCst)
    }

    fn now(&self) -> i64 {
        self.clock.now_utc().unix_timestamp()
    }

    /// One pass at `now` (Unix seconds): sample, bound the outbox, ship. Never panics; a failing
    /// source or store skips that step and the pass reports what it did.
    pub async fn pass_at(&self, now: i64) -> PassReport {
        let recorded = self.record(now).await;
        self.bound().await;
        let (shipped, refused) = self.ship().await;
        let _ = self
            .ports
            .outbox
            .purge_shipped(now.saturating_sub(SHIPPED_KEEP_SECS))
            .await;
        PassReport {
            recorded,
            shipped,
            refused,
            waiting: self.ports.outbox.unshipped().await.unwrap_or(0),
        }
    }

    /// One pass now.
    pub async fn pass(&self) -> PassReport {
        self.pass_at(self.now()).await
    }

    async fn record(&self, now: i64) -> usize {
        let Ok(cursors) = self.ports.outbox.cursors().await else {
            return 0;
        };
        let Ok(counters) = self.ports.sources.counters() else {
            return 0;
        };
        let day_start = now.div_euclid(DAY).saturating_mul(DAY);
        let active = self
            .ports
            .sources
            .active_users(day_start, now)
            .unwrap_or_default();
        let interval = i64::try_from(self.settings.interval_seconds).unwrap_or(3_600);
        let planned = plan(
            &self.settings.tenant,
            now,
            interval,
            &cursors,
            counters,
            &active,
            &self.ports.salt.salt(),
        );
        self.ports.outbox.append(&planned).await.unwrap_or(0)
    }

    /// Keeps the outbox within `outbox_max`: summarises the oldest amount events, degrades when it
    /// cannot.
    async fn bound(&self) {
        let max = self.settings.outbox_max;
        let Ok(count) = self.ports.outbox.unshipped().await else {
            return;
        };
        if count <= max {
            self.degraded.store(false, Ordering::SeqCst);
            return;
        }
        let excess = count - max;
        let Ok(pending) = self.ports.outbox.pending(count).await else {
            return;
        };
        if let Some((remove, insert)) = plan_compaction(&pending, excess) {
            let _ = self.ports.outbox.replace(&remove, &insert).await;
        }
        let after = self.ports.outbox.unshipped().await.unwrap_or(count);
        self.degraded.store(after > max, Ordering::SeqCst);
    }

    async fn ship(&self) -> (usize, usize) {
        let batch = self.settings.batch.clamp(1, MAX_EVENTS);
        let mut shipped = 0;
        let mut refused = 0;
        loop {
            let Ok(mut events) = self.ports.outbox.pending(batch).await else {
                break;
            };
            if events.is_empty() {
                break;
            }
            let mut body = wire_body(&events);
            while body.len() > MAX_BODY && events.len() > 1 {
                events.truncate(events.len() / 2);
                body = wire_body(&events);
            }
            let ids: Vec<String> = events.iter().map(|e| e.id.clone()).collect();
            match self.ports.shipper.ship(body).await {
                ShipOutcome::Accepted => {
                    if self.ports.outbox.mark_shipped(&ids).await.is_err() {
                        break;
                    }
                    shipped += ids.len();
                }
                ShipOutcome::Refused => {
                    if self.ports.outbox.mark_shipped(&ids).await.is_err() {
                        break;
                    }
                    refused += ids.len();
                }
                // The events stay; the next pass tries again. Offline for days is fine.
                ShipOutcome::Retry => break,
            }
        }
        (shipped, refused)
    }
}

impl Service for MeteringService {
    fn name(&self) -> &'static str {
        "metering"
    }

    fn level(&self) -> Level {
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            check_credentials(&self.settings)?;
            if self.degraded() {
                return Err("the usage outbox is full; events are being summarised".to_owned());
            }
            self.ports
                .outbox
                .unshipped()
                .await
                .map(|_| ())
                .map_err(|_| "the usage outbox could not be read".to_owned())
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            let every = Duration::from_secs(self.settings.interval_seconds);
            loop {
                self.pass().await;
                tokio::select! {
                    () = tokio::time::sleep(every) => {}
                    _ = stop.changed() => return Ok(()),
                }
                if *stop.borrow() {
                    return Ok(());
                }
            }
        })
    }
}

/// Registers the emitter when `[services.metering]` is enabled. Disabled (the default), nothing is
/// registered: no task, no table, no network call.
///
/// # Errors
/// A setting out of range, or a missing [`MeteringPorts`].
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("metering")?;
    if !settings.enabled {
        return Ok(());
    }
    settings.validate()?;
    let ports = ctx.require_port::<MeteringPorts>()?;
    let clock = Arc::clone(ctx.clock());
    ctx.add(Arc::new(MeteringService::new(ports, settings, clock)));
    Ok(())
}
