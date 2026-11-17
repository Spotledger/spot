//! Journal snapshots hosted (WP-RTB-01.T5, ADR-061 section 6,
//! `docs/running-spot/journal_snapshots.md`).
//!
//! # What it does
//!
//! Per site, one task: daily at a quiet hour (`at`, default `02:30`) and right after a migration
//! (the metadata swap counter changed), it reads a consistent image of every captured record
//! ([`spot_journal_snapshot_pg::ImageReader`]: one repeatable-read read-only transaction, the
//! outbox head read in it), writes the parts beside the journal, **waits until the journal's
//! shipped watermark has reached the snapshot's position**, and only then writes the signed index
//! (a snapshot with no index is not listed). One at a time per site: a coordinator lease.
//!
//! # Verification and retention
//!
//! A snapshot is listed unverified. [`SnapshotHandle::mark_verified`] is what the audit run calls
//! when a scratch rebuild from the snapshot is clean; a rebuild trusts only verified snapshots (the
//! planner decides, this module does not weaken it). Retention is **planned**
//! ([`spot_journal_snapshot_pg::retention::plan`]: the last 7 daily and 4 weekly, the newest of each
//! generation, never the only verified one) and the plan is audited; the journal store refuses
//! deletes under `snapshots/`, so nothing is removed here (see the task report).
//!
//! # Failure
//!
//! A failed snapshot leaves nothing listed and writes a `snapshot_failed` record
//! (`sys_journal_admin`); two failures in a row raise the amber condition `SNAPSHOT_FAILING`
//! (subject kind `source`, parameter `reason`), cleared by the next success. `JOURNAL_GAP` stays
//! for chain damage.
//!
//! # Configuration
//!
//! `[services.journal_snapshots]`, on whenever `[services.journal]` is: `at`, `utc_offset_minutes`
//! (the site time zone as a fixed offset: no zone database is read), `keep_daily`, `keep_weekly`,
//! `page_size`, `poll_seconds`, `reach_timeout_seconds`, `statement_timeout_seconds`, `enabled`.

use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use serde::Deserialize;
use spot_conditions::{ConditionDef, ConditionParams, Millis, Severity};
use spot_core::Clock;
use spot_journal_manifest::{SignedManifest, verify};
use spot_journal_snapshot::{IdentityTransform, list_snapshots, mark_verified};
use spot_journal_snapshot_pg::retention::{Retention, plan};
use spot_journal_snapshot_pg::{
    JournalReach, SnapshotKeyring, SnapshotPgError, TakeSettings, take_snapshot,
};
use spot_secrets::KeyProvider;
use spot_storage_contract::{
    Catalog, Isolation, Name, NodeRef, Outbox, OutboxEvent, RecordId, StorageBackend, Transaction,
    TxOptions,
};
use tokio::sync::watch;

use crate::services::conditions::{ConditionRegistrations, ConditionsHandle};
use crate::services::journal::{
    CoordinatorLease, FolderStore, JournalHandle, JournalStore, ProgressHandle, TenantScoped,
    WriterLease,
};
use crate::services::secrets::SecretsHost;
use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

/// After the journal module (1100), whose handle and settings it reads, and after the conditions
/// module (1000), so it may register its own condition definition.
pub const ORDER: u32 = 1120;

/// The condition a failing snapshot raises.
pub const CONDITION_ID: &str = "SNAPSHOT_FAILING";

/// `[services.journal_snapshots]`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// `false` turns snapshots off while the journal stays on.
    pub enabled: bool,
    /// The daily time, `HH:MM`, in the site's offset (default `02:30`).
    pub at: String,
    /// The site's offset from UTC in minutes (default 0).
    pub utc_offset_minutes: i32,
    /// Daily snapshots kept (default 7).
    pub keep_daily: usize,
    /// Weekly snapshots kept (default 4).
    pub keep_weekly: usize,
    /// Records per page (default 500).
    pub page_size: i64,
    /// How often the schedule is looked at, in seconds (default 30).
    pub poll_seconds: u64,
    /// How long a snapshot waits for the journal to reach its position, in seconds (default 300).
    pub reach_timeout_seconds: u64,
    /// The read's statement timeout, in seconds (default 600).
    pub statement_timeout_seconds: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            at: "02:30".to_owned(),
            utc_offset_minutes: 0,
            keep_daily: 7,
            keep_weekly: 4,
            page_size: 500,
            poll_seconds: 30,
            reach_timeout_seconds: 300,
            statement_timeout_seconds: 600,
        }
    }
}

impl Settings {
    /// The daily time as minutes after local midnight.
    ///
    /// # Errors
    /// A time that is not `HH:MM`.
    pub fn at_minutes(&self) -> Result<u32> {
        let bad = || {
            Error(format!(
                "services.journal_snapshots: `at` is not HH:MM ({})",
                self.at
            ))
        };
        let (hours, minutes) = self.at.split_once(':').ok_or_else(bad)?;
        let (hours, minutes): (u32, u32) = (
            hours.parse().map_err(|_| bad())?,
            minutes.parse().map_err(|_| bad())?,
        );
        if hours > 23 || minutes > 59 {
            return Err(bad());
        }
        Ok(hours * 60 + minutes)
    }
}

/// Milliseconds since the Unix epoch of `clock`'s now.
fn now_millis(clock: &dyn Clock) -> i64 {
    i64::try_from(clock.now_utc().unix_timestamp_nanos() / 1_000_000).unwrap_or(0)
}

/// Whether the daily snapshot is due: local time is at or past `at_minutes` and nothing was taken
/// since this day's `at` instant.
#[must_use]
pub fn daily_due(
    now_millis: i64,
    offset_minutes: i32,
    at_minutes: u32,
    last_taken_millis: Option<i64>,
) -> bool {
    const MINUTE: i64 = 60_000;
    const DAY: i64 = 1_440 * MINUTE;
    let local = now_millis + i64::from(offset_minutes) * MINUTE;
    let day_start = local.div_euclid(DAY) * DAY;
    let due_local = day_start + i64::from(at_minutes) * MINUTE;
    if local < due_local {
        return false;
    }
    let due_utc = due_local - i64::from(offset_minutes) * MINUTE;
    last_taken_millis.is_none_or(|taken| taken < due_utc)
}

fn hex(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The shipped watermark as the snapshot's gate.
struct ProgressReach(ProgressHandle);

impl JournalReach for ProgressReach {
    fn reached<'a>(
        &'a self,
        generation: u64,
        seq: u64,
        timeout: Duration,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        Box::pin(async move {
            self.0
                .wait_durable(
                    spot_events_contract::envelope::Position { generation, seq },
                    timeout,
                )
                .await
        })
    }
}

#[derive(Default)]
struct Status {
    last: Option<(u64, u64, i64, bool)>,
    failures: u32,
    next: String,
    planned_prune: usize,
}

/// What other modules read: readiness and `mark_verified`.
pub struct SnapshotHandle {
    site: String,
    status: Mutex<Status>,
    target: PathBuf,
    keyring: Arc<SnapshotKeyring>,
    clock: Arc<dyn Clock>,
}

impl SnapshotHandle {
    fn store(&self) -> std::result::Result<TenantScoped<FolderStore>, SnapshotPgError> {
        let folder = FolderStore::new(&self.target)
            .map_err(|_| SnapshotPgError::Write("journal_store.io"))?;
        TenantScoped::new(folder, &self.site)
            .map_err(|_| SnapshotPgError::OptionsInvalid("the site is not a usable journal tenant"))
    }

    /// `snapshots: last g1:s20931 4h ago (verified), next 02:30`.
    #[must_use]
    pub fn readiness_text(&self) -> String {
        let status = self.status.lock().unwrap_or_else(PoisonError::into_inner);
        let now = now_millis(self.clock.as_ref());
        let last = match status.last {
            Some((generation, seq, at, verified)) => {
                let hours = now.saturating_sub(at) / 3_600_000;
                let state = if verified { "verified" } else { "unverified" };
                format!("last g{generation}:s{seq} {hours}h ago ({state})")
            }
            None => "none taken yet".to_owned(),
        };
        let failing = if status.failures > 0 {
            format!(", failing x{}", status.failures)
        } else {
            String::new()
        };
        format!("snapshots: {last}, next {}{failing}", status.next)
    }

    /// Records that the rebuild test verified the snapshot at `(generation, position)`: writes the
    /// signed marker beside its index. Idempotent.
    ///
    /// # Errors
    /// [`SnapshotPgError`] when the snapshot is not there or the store fails.
    pub async fn mark_verified(
        &self,
        generation: u64,
        position: u64,
    ) -> std::result::Result<(), SnapshotPgError> {
        let store = self.store()?;
        let signer = self.keyring.signer();
        let keys = self.keyring.keys();
        mark_verified(&store, &self.site, generation, position, &keys, &signer).await?;
        let mut status = self.status.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(last) = status.last.as_mut()
            && (last.0, last.1) == (generation, position)
        {
            last.3 = true;
        }
        Ok(())
    }

    /// How many snapshots the last retention plan said may be pruned.
    #[must_use]
    pub fn planned_prune(&self) -> usize {
        self.status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .planned_prune
    }
}

struct Parts {
    site: String,
    settings: Settings,
    at_minutes: u32,
    backend: spot_postgres::PgBackend,
    catalog: Arc<Catalog>,
    live: Arc<crate::live::SiteLive>,
    journal: Arc<JournalHandle>,
    conditions: Arc<ConditionsHandle>,
    lease: Arc<dyn WriterLease>,
    handle: Arc<SnapshotHandle>,
    verifying: spot_journal_manifest::VerifyingKeys,
    clock: Arc<dyn Clock>,
}

/// The snapshot service of one site.
pub struct SnapshotService {
    parts: Parts,
    swaps: Mutex<Option<watch::Receiver<u64>>>,
}

impl SnapshotService {
    fn clock(&self) -> &dyn Clock {
        self.parts.clock.as_ref()
    }

    async fn manifest_hash(&self, store: &dyn JournalStore, generation: u64) -> Option<String> {
        let key = spot_journal_store::Layout::manifest(&self.parts.site, generation)
            .ok()?
            .to_key();
        let object = store.get(&key).await.ok()??;
        let signed = SignedManifest::from_bytes(&object.bytes).ok()?;
        let verified = verify(&signed, &self.parts.verifying).ok()?;
        Some(hex(&verified.manifest().manifest_hash()))
    }

    async fn audit(&self, action: &str, outcome: &str, code: Option<&str>, target: &str) {
        let p = &self.parts;
        let id = RecordId::new_v7();
        let now = p
            .clock
            .now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned());
        let mut data = serde_json::Map::new();
        data.insert("action".to_owned(), action.into());
        data.insert("actor".to_owned(), "system".into());
        data.insert("target".to_owned(), target.into());
        data.insert("outcome".to_owned(), outcome.into());
        if let Some(code) = code {
            data.insert("code".to_owned(), code.into());
        }
        data.insert("dry_run".to_owned(), false.into());
        let payload = serde_json::json!({
            "meta": {"occurred_at": now,
                     "actor": {"kind": "system", "component": "journal_snapshots"},
                     "correlation_id": id.to_string()},
            "data": data,
        })
        .to_string();
        let written = async {
            let event = OutboxEvent {
                id,
                topic: Name::new("sys_journal_admin").ok()?,
                subject: NodeRef {
                    entity: Name::new("journal_admin").ok()?,
                    id: RecordId::new_v7(),
                },
                subject_version: None,
                schema_version: 1,
                payload,
            };
            let mut options = TxOptions::new(
                Name::new("journal_snapshot_audit").ok()?,
                Isolation::ReadCommitted,
            );
            options.max_writes = 10;
            let mut tx = p
                .backend
                .begin(Arc::clone(&p.catalog), options)
                .await
                .ok()?;
            tx.enqueue(&event).await.ok()?;
            tx.commit().await.ok()
        }
        .await;
        if written.is_none() {
            tracing::warn!(site = %p.site, action, "journal snapshots: the admin record could not be written");
        }
    }

    async fn raise(&self, reason: &str) {
        let params = ConditionParams::new().with("reason", reason);
        let now = Millis::new(u64::try_from(now_millis(self.clock())).unwrap_or(0));
        let _ = self
            .parts
            .conditions
            .raise(CONDITION_ID, &self.parts.site, params, now)
            .await;
    }

    async fn clear(&self) {
        let now = Millis::new(u64::try_from(now_millis(self.clock())).unwrap_or(0));
        let _ = self
            .parts
            .conditions
            .observe_cleared(CONDITION_ID, &self.parts.site, now)
            .await;
    }

    /// One snapshot, then the retention plan. Returns whether one was written.
    async fn snapshot(
        &self,
        store: &TenantScoped<FolderStore>,
    ) -> std::result::Result<bool, SnapshotPgError> {
        let p = &self.parts;
        let generation = p.journal.progress().durable().generation;
        if generation == 0 {
            // Nothing is in the journal yet: a snapshot would have nothing to be consistent with.
            return Ok(false);
        }
        let Some(manifest_hash) = self.manifest_hash(store, generation).await else {
            return Err(SnapshotPgError::ReadFailed(
                "the journal manifest cannot be read",
            ));
        };
        let excluded: BTreeSet<String> = p
            .live
            .engine()
            .map(|engine| {
                engine
                    .capture_exclusions()
                    .iter()
                    .map(|e| e.doctype.clone())
                    .collect()
            })
            .unwrap_or_default();
        let settings = TakeSettings {
            tenant: p.site.clone(),
            manifest_hash,
            taken_at_millis: now_millis(self.clock()),
            page_size: p.settings.page_size,
            statement_timeout: Duration::from_secs(p.settings.statement_timeout_seconds),
            reach_timeout: Duration::from_secs(p.settings.reach_timeout_seconds),
            excluded,
        };
        let signer = p.handle.keyring.signer();
        let reach = ProgressReach(p.journal.progress().clone());
        let info = take_snapshot(
            &p.backend,
            &p.catalog,
            store,
            &signer,
            &IdentityTransform,
            &reach,
            &settings,
        )
        .await?;
        {
            let mut status = p
                .handle
                .status
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            status.last = Some((
                info.generation,
                info.position,
                info.taken_at_millis,
                info.verified,
            ));
        }
        self.audit(
            "snapshot_taken",
            "ok",
            None,
            &format!("g{}:s{}", info.generation, info.position),
        )
        .await;
        if let Ok(listed) = list_snapshots(store, &p.site, &p.handle.keyring.keys()).await {
            let retention = Retention {
                daily: p.settings.keep_daily,
                weekly: p.settings.keep_weekly,
            };
            let planned = plan(&listed, retention);
            p.handle
                .status
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .planned_prune = planned.prune.len();
        }
        Ok(true)
    }

    async fn tick(&self, swapped: bool) {
        let p = &self.parts;
        if !p.lease.hold().await {
            return;
        }
        let Ok(store) = p.handle.store() else {
            return;
        };
        let last_taken = match list_snapshots(&store, &p.site, &p.handle.keyring.keys()).await {
            Ok(listed) => {
                if let Some(newest) = listed.last() {
                    let mut status = p
                        .handle
                        .status
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    if status.last.is_none() {
                        status.last = Some((
                            newest.generation,
                            newest.position,
                            newest.taken_at_millis,
                            newest.verified,
                        ));
                    }
                }
                listed.iter().map(|s| s.taken_at_millis).max()
            }
            Err(_) => return,
        };
        let now = now_millis(self.clock());
        let due =
            swapped || daily_due(now, p.settings.utc_offset_minutes, p.at_minutes, last_taken);
        if !due {
            return;
        }
        match self.snapshot(&store).await {
            Ok(true) => {
                p.handle
                    .status
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .failures = 0;
                self.clear().await;
            }
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(site = %p.site, code = error.code(), "journal snapshots: a snapshot failed");
                let failures = {
                    let mut status = p
                        .handle
                        .status
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    status.failures = status.failures.saturating_add(1);
                    status.failures
                };
                self.audit("snapshot_failed", "failed", Some(error.code()), "snapshot")
                    .await;
                if failures >= 2 {
                    self.raise(error.code()).await;
                }
            }
        }
    }
}

impl Service for SnapshotService {
    fn name(&self) -> &'static str {
        "journal_snapshots"
    }

    fn level(&self) -> Level {
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async { Ok(()) })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let taken = self
                .swaps
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
            let Some(mut swaps) = taken else {
                return Err("the snapshot service was already started".to_owned());
            };
            let mut stop = stop;
            let poll = Duration::from_secs(self.parts.settings.poll_seconds.max(1));
            loop {
                let swapped = swaps.has_changed().unwrap_or(false);
                if swapped {
                    swaps.borrow_and_update();
                }
                self.tick(swapped).await;
                {
                    let mut status = self
                        .parts
                        .handle
                        .status
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    status.next = self.parts.settings.at.clone();
                }
                tokio::select! {
                    changed = stop.changed() => {
                        if changed.is_err() || *stop.borrow() {
                            self.parts.lease.release().await;
                            return Ok(());
                        }
                    }
                    () = tokio::time::sleep(poll) => {}
                }
            }
        })
    }
}

fn provided() -> &'static Mutex<HashMap<String, Arc<SnapshotHandle>>> {
    static PROVIDED: OnceLock<Mutex<HashMap<String, Arc<SnapshotHandle>>>> = OnceLock::new();
    PROVIDED.get_or_init(Mutex::default)
}

/// The snapshot handle of a running site (for a harness).
#[must_use]
pub fn hosted(site: &str) -> Option<Arc<SnapshotHandle>> {
    provided()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(site)
        .cloned()
}

/// Registers the snapshot service: off without `[services.journal]` or with `enabled = false`.
pub async fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let site = ctx.label().to_owned();
    if !ctx.assembling() {
        if let Some(handle) = hosted(&site) {
            ctx.provide::<SnapshotHandle>(handle)?;
        }
        return Ok(());
    }
    let own: Settings = ctx.settings_or_default("journal_snapshots")?;
    let at_minutes = own.at_minutes()?;
    let journal_settings: crate::services::journal::Settings =
        ctx.settings_or_default("journal")?;
    let Some(resolved) = journal_settings.resolve(&site)? else {
        return Ok(());
    };
    if !own.enabled {
        return Ok(());
    }
    if let Some(registrations) = ctx.port::<ConditionRegistrations>() {
        registrations.register(
            ConditionDef::new(
                CONDITION_ID,
                Severity::Amber,
                "source",
                "condition.SNAPSHOT_FAILING.explanation",
            )
            .with_param("reason"),
        );
    }
    let secrets = ctx.port::<SecretsHost>().ok_or_else(|| {
        Error("services.journal_snapshots needs [services.secrets]: the snapshot key is derived from the site's key provider".to_owned())
    })?;
    let provider: Arc<dyn KeyProvider> = secrets.keyring().clone();
    let keyring = Arc::new(
        SnapshotKeyring::derive(&provider)
            .await
            .map_err(|e| Error(format!("services.journal_snapshots: {}", e.code())))?,
    );
    let env = *ctx.env()?;
    let backend = env.backend.clone();
    let catalog = Arc::clone(&env.permission.catalog);
    let live = Arc::clone(env.live);
    let clock: Arc<dyn Clock> = Arc::clone(ctx.clock());
    let coordinator = coordinator_for(
        env.loaded.common.services.profile.as_str(),
        &backend,
        &site,
        &clock,
    )
    .await?;
    let handle = Arc::new(SnapshotHandle {
        site: site.clone(),
        status: Mutex::new(Status {
            next: own.at.clone(),
            ..Status::default()
        }),
        target: resolved.target.clone(),
        keyring: Arc::clone(&keyring),
        clock: Arc::clone(&clock),
    });
    let verifying = {
        let mut keys = spot_journal_manifest::VerifyingKeys::new();
        // The same verifying set the journal uses: re-derived, never copied.
        let current = provider.current();
        let derived = provider
            .derive_key(&current, spot_journal_ship::keys::SIGNING_PURPOSE, 32)
            .await
            .map_err(|_| {
                Error(
                    "services.journal_snapshots: the key provider cannot derive the journal key"
                        .to_owned(),
                )
            })?;
        let seed: [u8; 32] = derived.expose().try_into().map_err(|_| {
            Error("services.journal_snapshots: the derived key has an unusable length".to_owned())
        })?;
        let signing = ed25519_dalek_signing(&seed);
        keys.add_key(
            &format!("{}-v{}", current.id, current.version),
            signing,
            1,
            u64::MAX,
        )
        .map_err(|_| {
            Error("services.journal_snapshots: the verifying key is unusable".to_owned())
        })?;
        keys
    };
    let swaps = live.swaps();
    ctx.finally(move |ctx| {
        let conditions = ctx.require_port::<ConditionsHandle>()?;
        let journal = ctx.require_port::<JournalHandle>()?;
        let lease: Arc<dyn WriterLease> = Arc::new(CoordinatorLease::new(
            coordinator,
            format!("journal-snapshot:{site}"),
            format!("journal-snapshot-{site}"),
            Duration::from_secs(60),
        ));
        let service = Arc::new(SnapshotService {
            parts: Parts {
                site: site.clone(),
                settings: own,
                at_minutes,
                backend,
                catalog,
                live,
                journal,
                conditions,
                lease,
                handle: Arc::clone(&handle),
                verifying,
                clock,
            },
            swaps: Mutex::new(Some(swaps)),
        });
        ctx.add(service);
        ctx.provide::<SnapshotHandle>(Arc::clone(&handle))?;
        provided()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(site, handle);
        Ok(())
    });
    Ok(())
}

fn ed25519_dalek_signing(seed: &[u8; 32]) -> ed25519_dalek::VerifyingKey {
    ed25519_dalek::SigningKey::from_bytes(seed).verifying_key()
}

/// The coordinator the snapshot lease lives in (see `services/journal.rs`).
async fn coordinator_for(
    profile: &str,
    backend: &spot_postgres::PgBackend,
    site: &str,
    clock: &Arc<dyn Clock>,
) -> Result<Arc<dyn spot_coordinator::Coordinator>> {
    if profile == "cluster" {
        let coordinator = spot_coordinator_pg::PgCoordinator::new(
            backend.pool(),
            spot_coordinator_pg::PgCoordinatorConfig::default(),
        );
        coordinator.prepare().await.map_err(|e| {
            Error(format!(
                "services.journal_snapshots: the coordinator: {}",
                e.code
            ))
        })?;
        return Ok(Arc::new(coordinator));
    }
    let node = spot_coordinator::NodeId::new(format!("journal-snapshot-{site}")).map_err(|e| {
        Error(format!(
            "services.journal_snapshots: the node name: {}",
            e.code
        ))
    })?;
    let coordinator = spot_coordinator::InProcessCoordinator::start(
        Arc::new(spot_coordinator::InMemoryPresence::new()),
        node,
        Arc::clone(clock),
    )
    .await
    .map_err(|e| {
        Error(format!(
            "services.journal_snapshots: the coordinator: {}",
            e.code
        ))
    })?;
    Ok(coordinator)
}
