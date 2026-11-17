//! Personal-field protection and erasure hosted (WP-RTB-01.T8, ADR-061 sections 3 and 6,
//! `docs/running-spot/journal_privacy.md`).
//!
//! # What it does
//!
//! Builds, per site, the subject-key adapter ([`ProviderSubjectKeys`]) over the site's key
//! provider and the PostgreSQL wrapped-key store, the declared personal fields
//! ([`CatalogueFields`]), and from them the protect transform the journal shipper runs every line
//! through. It provides three ports: [`JournalPrivacyHandle`] (the privacy machinery and
//! `erase_subject`), `dyn Protector` (read by `services/journal.rs` in its finaliser) and, through
//! the handle, the pieces snapshots and rebuild (T5, T6) wrap.
//!
//! # Configuration (`[services.journal_privacy]`)
//!
//! Without the table the module is off and the journal ships lines as they are. The catalogue has
//! no `personal` flag today, so the declarations are a configuration list:
//!
//! ```toml
//! [services.journal_privacy]
//! forbid_erase_classes = ["tax_10y"]   # retention classes that forbid erasing a field
//! erasers = ["user-id-of-the-data-protection-officer"]
//! cache_ttl_millis = 60000
//! [[services.journal_privacy.fields]]
//! entity = "contact"
//! path = "email"
//! subject = "person"                   # the field that identifies the data subject (required)
//! purpose = "notifications"
//! retention_class = "contact_5y"
//! ```
//!
//! # Fail closed
//!
//! A personal field without a `subject` refuses the site's start (`journal_privacy.subject_missing`).
//! When fields are declared but protection cannot be built (no `[services.secrets]`, no wrapped-key
//! table) the module provides a protector that refuses every line, so nothing reaches the journal
//! in clear and the shipper degrades. The wrapped-key table is created by `spot migrate`.
//!
//! # Erase
//!
//! [`JournalPrivacyHandle::erase_subject`] checks the requester (the `erasers` list is the interim
//! for the platform permission `journal.erase_subject`), the retention classes, destroys the key
//! (idempotent, a tombstone: a destroyed subject never gets a second key) and writes
//! `sys_journal_admin` (`subject_erased`, the target a salted hash, never the subject). A key a
//! node cached before the destroy is served for at most `cache_ttl_millis` (default 60 s).

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use rand::TryRngCore;
use serde::Deserialize;
use spot_core::Clock;
use spot_journal_pii_secrets::{
    AdapterConfig, AuditPort, Clock as KeyClock, PortError, ProviderSubjectKeys, Rng,
    SubjectKeyDestroyed,
};
use spot_journal_pii_secrets_pg::{PgWrappedKeyStore, ensure_present};
use spot_journal_privacy::{
    AdminAction, AdminAudit, CatalogueFields, ErasePermission, Eraser, ErasureOutcome, FieldDecl,
    Privacy, PrivacyError, RefusingProtector, SubjectHasher,
};
use spot_journal_protect::{SharedKeys, SharedNonces};
use spot_journal_ship::Protector;
use spot_pg_bridge::{Bridge, BridgeConfig};
use spot_postgres::PgBackend;
use spot_secrets::{KeyProvider, KeyRef};
use spot_storage_contract::{
    Catalog, Isolation, Name, NodeRef, Outbox, OutboxEvent, RecordId, StorageBackend, Transaction,
    TxOptions,
};
use tokio::sync::watch;

use crate::services::secrets::SecretsHost;
use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

/// Start order: before the journal module (1100), which reads the `dyn Protector` port.
pub const ORDER: u32 = 1090;

/// `[services.journal_privacy]`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// The personal fields.
    pub fields: Vec<FieldDecl>,
    /// Retention classes that forbid erasing a field.
    pub forbid_erase_classes: Vec<String>,
    /// User ids that may erase a subject (the interim for `journal.erase_subject`).
    pub erasers: Vec<String>,
    /// How long a node may serve a key it cached before a destroy elsewhere (default 60,000).
    pub cache_ttl_millis: u64,
    /// The most unwrapped keys cached (default 1,024).
    pub max_cached_keys: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            fields: Vec::new(),
            forbid_erase_classes: Vec::new(),
            erasers: Vec::new(),
            cache_ttl_millis: 60_000,
            max_cached_keys: 1_024,
        }
    }
}

impl Settings {
    fn forbidden(&self) -> BTreeSet<String> {
        self.forbid_erase_classes.iter().cloned().collect()
    }
}

/// The operating-system random source for new subject keys.
struct OsRandom;

impl Rng for OsRandom {
    fn fill(&self, buf: &mut [u8]) -> std::result::Result<(), PortError> {
        rand::rngs::OsRng
            .try_fill_bytes(buf)
            .map_err(|_| PortError("the operating system random source failed".to_owned()))
    }
}

/// The site clock for the key adapter.
struct SiteKeyClock(Arc<dyn Clock>);

impl KeyClock for SiteKeyClock {
    fn now_unix_ms(&self) -> i64 {
        i64::try_from(self.0.now_utc().unix_timestamp_nanos() / 1_000_000).unwrap_or(0)
    }
}

/// The key adapter's own audit event: logged (the hash only); the platform record is
/// `sys_journal_admin`, written by the eraser.
struct LogDestroy;

impl AuditPort for LogDestroy {
    fn record(&self, event: &SubjectKeyDestroyed) -> std::result::Result<(), PortError> {
        tracing::info!(
            subject_hash = %event.subject_hash,
            already_absent = event.already_absent,
            "journal privacy: a subject key was destroyed"
        );
        Ok(())
    }
}

type Keys = ProviderSubjectKeys<Arc<dyn KeyProvider>, PgWrappedKeyStore, LogDestroy>;

struct Hasher(Arc<Keys>);

impl SubjectHasher for Hasher {
    fn hash(&self, subject: &str) -> String {
        self.0.subject_hash(subject)
    }
}

struct Erasers(Vec<String>);

impl ErasePermission for Erasers {
    fn may_erase(&self, requester: &str) -> bool {
        self.0.iter().any(|allowed| allowed == requester)
    }
}

/// One `sys_journal_admin` record waiting to be written to the outbox.
struct AdminRecord {
    action: AdminAction,
    subject_hash: String,
    requester: String,
}

/// Queues the eraser's audit records; [`QueuedAudit::flush`] writes them (one outbox event each).
/// The eraser is synchronous, the outbox write is not, so the handle flushes after the erase.
#[derive(Default)]
struct QueuedAudit {
    queue: Mutex<Vec<AdminRecord>>,
}

impl AdminAudit for QueuedAudit {
    fn record(
        &self,
        action: &AdminAction,
        subject_hash: &str,
        requester: &str,
    ) -> std::result::Result<(), &'static str> {
        self.queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(AdminRecord {
                action: action.clone(),
                subject_hash: subject_hash.to_owned(),
                requester: requester.to_owned(),
            });
        Ok(())
    }
}

fn admin_event(record: &AdminRecord, now_rfc3339: &str) -> Option<OutboxEvent> {
    let (action, outcome, code) = match &record.action {
        AdminAction::SubjectErased { .. } => ("subject_erased", "ok", None),
        AdminAction::SubjectEraseRefused { code } => ("subject_erased", "refused", Some(*code)),
    };
    let mut data = serde_json::Map::new();
    data.insert("action".to_owned(), action.into());
    data.insert("actor".to_owned(), record.requester.clone().into());
    data.insert("target".to_owned(), record.subject_hash.clone().into());
    data.insert("outcome".to_owned(), outcome.into());
    if let Some(code) = code {
        data.insert("code".to_owned(), code.into());
    }
    data.insert("dry_run".to_owned(), false.into());
    let id = RecordId::new_v7();
    let payload = serde_json::json!({
        "meta": {
            "occurred_at": now_rfc3339,
            "actor": {"kind": "system", "component": "journal_privacy"},
            "correlation_id": id.to_string(),
        },
        "data": data,
    })
    .to_string();
    Some(OutboxEvent {
        id,
        topic: Name::new("sys_journal_admin").ok()?,
        subject: NodeRef {
            entity: Name::new("journal_admin").ok()?,
            id: RecordId::new_v7(),
        },
        subject_version: None,
        schema_version: 1,
        payload,
    })
}

impl QueuedAudit {
    async fn flush(&self, backend: &PgBackend, catalog: &Arc<Catalog>, clock: &dyn Clock) {
        let pending =
            std::mem::take(&mut *self.queue.lock().unwrap_or_else(PoisonError::into_inner));
        if pending.is_empty() {
            return;
        }
        let now = clock
            .now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned());
        let mut unwritten = Vec::new();
        for record in pending {
            let written = async {
                let name = Name::new("journal_privacy_admin").ok()?;
                let mut options = TxOptions::new(name, Isolation::ReadCommitted);
                options.max_writes = 10;
                let mut tx = backend.begin(Arc::clone(catalog), options).await.ok()?;
                tx.enqueue(&admin_event(&record, &now)?).await.ok()?;
                tx.commit().await.ok()
            }
            .await;
            if written.is_none() {
                unwritten.push(record);
            }
        }
        if !unwritten.is_empty() {
            tracing::warn!(
                count = unwritten.len(),
                "journal privacy: admin records not written yet; retrying"
            );
            self.queue
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .extend(unwritten);
        }
    }
}

/// What other modules read: the privacy machinery and `erase_subject`.
pub struct JournalPrivacyHandle {
    privacy: Privacy,
    eraser: Arc<Eraser>,
    audit: Arc<QueuedAudit>,
    backend: PgBackend,
    catalog: Arc<Catalog>,
    clock: Arc<dyn Clock>,
}

impl JournalPrivacyHandle {
    /// The declared fields, the keys and the transforms built over them (T5 and T6 wrap these).
    #[must_use]
    pub fn privacy(&self) -> &Privacy {
        &self.privacy
    }

    /// Erases `subject` on behalf of `requester` (a user id): the permission check, the retention
    /// classes, the key destroyed, `sys_journal_admin` written.
    ///
    /// # Errors
    /// [`PrivacyError`]; nothing is claimed as erased on an error.
    pub async fn erase_subject(
        &self,
        subject: &str,
        requester: &str,
    ) -> std::result::Result<ErasureOutcome, PrivacyError> {
        let eraser = Arc::clone(&self.eraser);
        let (subject_owned, requester_owned) = (subject.to_owned(), requester.to_owned());
        // The key store reaches the database through a bridge that blocks the calling thread.
        let outcome = off_runtime(move || eraser.erase_subject(&subject_owned, &requester_owned))
            .await
            .ok_or(PrivacyError::KeyStore("the erase task failed"))?;
        self.audit
            .flush(&self.backend, &self.catalog, self.clock.as_ref())
            .await;
        outcome
    }
}

/// Runs `work` on a plain thread and waits for it, or `None` when it panicked. The shared
/// blocking bridge refuses every thread that belongs to a tokio runtime, the blocking pool
/// included (ADR-082 addendum 1), so `spawn_blocking` would not do.
async fn off_runtime<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("journal-privacy".to_owned())
        .spawn(move || {
            let _ = tx.send(work());
        })
        .ok()?;
    rx.await.ok()
}

/// The module's service: checks the wrapped-key table exists and flushes queued admin records.
pub struct PrivacyService {
    bridge: Arc<Bridge>,
    handle: Arc<JournalPrivacyHandle>,
}

impl Service for PrivacyService {
    fn name(&self) -> &'static str {
        "journal_privacy"
    }

    fn level(&self) -> Level {
        // Losing it makes the shipper refuse lines (fail closed), which degrades the journal; it
        // does not stop the site serving.
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            let bridge = Arc::clone(&self.bridge);
            off_runtime(move || ensure_present(&bridge))
                .await
                .ok_or_else(|| "the privacy check task failed".to_owned())?
                .map_err(|e| {
                    format!(
                        "{}; the wrapped-key table is created by `spot migrate`",
                        e.code()
                    )
                })
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            loop {
                tokio::select! {
                    changed = stop.changed() => {
                        if changed.is_err() || *stop.borrow() {
                            self.flush().await;
                            return Ok(());
                        }
                    }
                    () = tokio::time::sleep(Duration::from_secs(1)) => self.flush().await,
                }
            }
        })
    }
}

impl PrivacyService {
    async fn flush(&self) {
        let h = &self.handle;
        h.audit
            .flush(&h.backend, &h.catalog, h.clock.as_ref())
            .await;
    }
}

fn provided() -> &'static Mutex<HashMap<String, Arc<JournalPrivacyHandle>>> {
    static PROVIDED: OnceLock<Mutex<HashMap<String, Arc<JournalPrivacyHandle>>>> = OnceLock::new();
    PROVIDED.get_or_init(Mutex::default)
}

/// The privacy handle of a running site (for a harness).
#[must_use]
pub fn hosted(site: &str) -> Option<Arc<JournalPrivacyHandle>> {
    provided()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(site)
        .cloned()
}

/// Builds a bridge on a plain thread (it starts its own runtime, which must not happen on one of
/// the host's runtime threads).
fn build_bridge(backend: &PgBackend, pool_size: usize) -> Result<Arc<Bridge>> {
    let largest = BridgeConfig::largest_in_flight(pool_size);
    if largest < 1 {
        return Err(Error(format!(
            "services.journal_privacy needs a PostgreSQL pool of at least 3 connections \
             (postgres.pool_max_size is {pool_size})"
        )));
    }
    let config = BridgeConfig {
        name: "journal_privacy".to_owned(),
        max_in_flight: largest.min(4),
        workers: 2,
        queue_capacity: 8,
        default_deadline: Duration::from_secs(10),
        max_deadline: Duration::from_secs(30),
    };
    let backend = backend.clone();
    std::thread::spawn(move || {
        Bridge::for_backend(config, &backend).map_err(|e| Error(format!("{e}")))
    })
    .join()
    .map_err(|_| Error("the journal privacy bridge thread failed".to_owned()))?
    .map(Arc::new)
}

/// Registers the module (see the module docs). Off without `[services.journal_privacy]`.
pub async fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("journal_privacy")?;
    let site = ctx.label().to_owned();
    if !ctx.assembling() {
        if let Some(handle) = hosted(&site) {
            ctx.provide::<dyn Protector>(handle.privacy.protector())?;
            ctx.provide::<JournalPrivacyHandle>(handle)?;
        }
        return Ok(());
    }
    if settings.fields.is_empty() {
        return Ok(());
    }
    // A personal field without a subject refuses the site's start: it could not be sealed.
    let fields = CatalogueFields::new(&settings.fields, &settings.forbidden())
        .map_err(|e| Error(format!("services.journal_privacy: {}: {e}", e.code())))?;
    let Some(secrets) = ctx.port::<SecretsHost>() else {
        tracing::error!(
            site = %site,
            "journal privacy: personal fields are declared but there is no [services.secrets]; the journal will refuse every line"
        );
        ctx.provide::<dyn Protector>(Arc::new(RefusingProtector))?;
        return Ok(());
    };
    let env = *ctx.env()?;
    let provider: Arc<dyn KeyProvider> = secrets.keyring().clone();
    // The tenant salt must never change (it names every subject's key): derived from version 1 of
    // the provider's key, not from the current one.
    let current = provider.current();
    let salt = provider
        .derive_key(
            &KeyRef {
                id: current.id.clone(),
                version: 1,
            },
            b"journal-subject-salt",
            32,
        )
        .await
        .map_err(|_| {
            Error(
                "services.journal_privacy: the key provider cannot derive the tenant salt"
                    .to_owned(),
            )
        })?
        .expose()
        .to_vec();
    let bridge = build_bridge(env.backend, env.loaded.common.postgres.pool_max_size)?;
    let mut config = AdapterConfig::new(site.clone(), salt);
    config.max_cached = settings.max_cached_keys;
    config.cache_ttl_millis = settings.cache_ttl_millis;
    let keys: Arc<Keys> = Arc::new(
        ProviderSubjectKeys::new(
            config,
            provider,
            PgWrappedKeyStore::new(Arc::clone(&bridge)),
            LogDestroy,
            Box::new(OsRandom),
            Box::new(SiteKeyClock(Arc::clone(ctx.clock()))),
        )
        .map_err(|e| Error(format!("services.journal_privacy: {}", e.code())))?,
    );
    let shared: SharedKeys = Arc::<Keys>::clone(&keys);
    // Nonces come from the OS random source, one `NonceSource` behind a lock.
    let nonces: SharedNonces = Arc::new(Mutex::new(OsNonces));
    let privacy = Privacy::new(Arc::clone(&fields), shared.clone(), nonces);
    let audit = Arc::new(QueuedAudit::default());
    let eraser = Arc::new(Eraser::new(
        fields,
        shared,
        Arc::new(Erasers(settings.erasers.clone())),
        Arc::new(Hasher(keys)),
        audit.clone(),
    ));
    let handle = Arc::new(JournalPrivacyHandle {
        privacy,
        eraser,
        audit,
        backend: env.backend.clone(),
        catalog: Arc::clone(&env.permission.catalog),
        clock: Arc::clone(ctx.clock()),
    });
    ctx.add(Arc::new(PrivacyService {
        bridge,
        handle: Arc::clone(&handle),
    }));
    ctx.provide::<dyn Protector>(handle.privacy.protector())?;
    ctx.provide::<JournalPrivacyHandle>(Arc::clone(&handle))?;
    provided()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(site, handle);
    Ok(())
}

/// Nonces for sealed envelopes: random from the operating system, never a counter.
struct OsNonces;

impl spot_journal_pii::NonceSource for OsNonces {
    fn next_nonce(&mut self) -> Option<[u8; spot_journal_pii::NONCE_LEN]> {
        let mut nonce = [0u8; spot_journal_pii::NONCE_LEN];
        rand::rngs::OsRng.try_fill_bytes(&mut nonce).ok()?;
        Some(nonce)
    }
}
