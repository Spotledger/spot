//! The journal service: the per-site shipper hosted (WP-RTB-01.T3, ADR-061 sections 1, 4, 5 and
//! 10, the project process notes).
//!
//! # What it does
//!
//! "The journal is the outbox stream, sealed; there is no second pipeline." Per site, one task
//! (the [`spot_journal_ship::Shipper`]) reads the outbox from a persisted watermark and makes it
//! durable through the relay: signed manifest, sealed segments, tail chunks. This module builds
//! that shipper from the site (its backend, its key provider, a coordinator lease, the
//! conditions handle) and runs it on a loop that ends on the stop signal.
//!
//! # Configuration
//!
//! `[services.journal]` of the host, with an optional table per site
//! (`[services.journal.sites.<site>]`; the site file has no `[journal]` table of its own in this
//! build, see the task report). Unknown keys are refused. **Without a `target` the service is
//! off**: the site serves, readiness says `journal: off (no off-site history)` and the platform's
//! standing warning applies. With a `target` the policy defaults to `remote-sync` and
//! `on_unreachable` to `degrade`.
//!
//! ```toml
//! [services.journal]
//! policy = "remote-sync"        # local | replica-sync | remote-sync
//! on_unreachable = "degrade"    # degrade | halt
//! region = "eu-1"
//! [services.journal.target]
//! kind = "folder"
//! path = "/srv/spot-journal"
//! ```
//!
//! # Keys
//!
//! The manifest signing key and the tail-chunk keys come from the site's key provider
//! ([`SecretsHost`], `[services.secrets]`): the signing key is derived from it at start and never
//! stored, logged, returned or shown in readiness. A site without `[services.secrets]` cannot
//! ship a journal and says so.
//!
//! # Failure behaviour
//!
//! The service is **optional** (it must not stop trading) except under
//! `policy = "remote-sync"` with `on_unreachable = "halt"`, where it is required. Under
//! `degrade` a store outage is retried with backoff (at most 30 s), the outbox keeps everything
//! (the barrier in `feed::cleanup`), and `RPO_BREACH` is raised once the outage outlasts
//! `rpo_grace_seconds`. A broken chain stops the shipper: it never writes on top of one.
//!
//! # Not here
//!
//! The acknowledgement gate (WP-RTB-01.T4) reads [`JournalHandle::progress`]; snapshots, rebuild,
//! audit and erasure keys are other tasks.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::Duration;

use serde::Deserialize;
use spot_conditions::{ConditionParams, Millis};
use spot_core::Clock;
use spot_events_contract::SourceId;
use spot_journal_relay::{Durability, RelayConfig};
use spot_journal_segment::SealLimits;
use spot_journal_ship::memory::MemoryLease;
// Public: the host-level surface `harness/rust/l3-journal-sync/tests/journal_ship.rs` needs
// (that crate owns no dependency on the journal crates of its own), and what a test that builds
// [`JournalService`] directly uses.
pub use spot_journal_ship::{
    Alarm, AlarmKind, Alarms, ChainChecker, ChainState, CoordinatorLease, JournalKeys, PassThrough,
    PortFuture, Ports, Progress, ProgressHandle, Protector, RelayFactory,
    ShipFault, Shipper, ShipperConfig, Step, StoreChecker, StoreRelayFactory, TenantScoped,
    WriterLease, check_folder_target,
};
pub use spot_journal_ship::pg::{PgOutbox, PgWatermarks};
pub use spot_journal_store::{FolderStore, JournalStore, TenantId};
use spot_secrets::KeyProvider;
pub use spot_secrets::LocalKeyring;
use tokio::sync::watch;

use crate::services::conditions::ConditionsHandle;
use crate::services::secrets::SecretsHost;
use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

/// This module's start order: after the conditions module (1000), whose handle it needs.
pub const ORDER: u32 = 1100;

/// The durability policy (ADR-061 section 4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PolicyName {
    /// The local database commit is the durability; the journal is shipped in segments.
    Local,
    /// A synchronous standby is the durability.
    ReplicaSync,
    /// A tail chunk in the object store is the durability.
    RemoteSync,
}

impl PolicyName {
    /// The text readiness, `RPO_BREACH` and the docs use.
    #[must_use]
    pub fn text(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::ReplicaSync => "replica-sync",
            Self::RemoteSync => "remote-sync",
        }
    }

    fn durability(self) -> Durability {
        match self {
            Self::Local => Durability::Local,
            Self::ReplicaSync => Durability::ReplicaSync,
            Self::RemoteSync => Durability::RemoteSync,
        }
    }
}

/// What happens when the journal store cannot be reached.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OnUnreachable {
    /// Keep trading, raise `RPO_BREACH` after the grace period.
    #[default]
    Degrade,
    /// Refuse writes (the gate of WP-RTB-01.T4) until the store returns.
    Halt,
}

/// Where the journal's objects go. Only a folder in this task; `JournalStore` is the seam for S3
/// or Azure later.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    /// `folder`.
    pub kind: String,
    /// The directory (must exist and be writable; canonicalised once at start).
    pub path: PathBuf,
}

/// A site's own overrides (`[services.journal.sites.<site>]`).
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SiteOverride {
    /// `false` turns the journal off for this site.
    pub enabled: Option<bool>,
    /// The site's policy.
    pub policy: Option<PolicyName>,
    /// The site's unreachable-store behaviour.
    pub on_unreachable: Option<OnUnreachable>,
    /// The site's target.
    pub target: Option<Target>,
}

/// `[services.journal]`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// The policy; `remote-sync` when a target is configured and this is absent.
    pub policy: Option<PolicyName>,
    /// What to do when the store is unreachable.
    pub on_unreachable: OnUnreachable,
    /// The journal target. Absent: the service is off.
    pub target: Option<Target>,
    /// The region, recorded for the operator (the manifest has no region field yet).
    pub region: Option<String>,
    /// Seal at this many MiB (default 64).
    pub seal_max_mib: u64,
    /// Seal when the open segment is this old, in seconds (default 300).
    pub seal_max_seconds: u64,
    /// The most outbox entries one step reads (default 2,000).
    pub batch_max_entries: usize,
    /// Pause between steps when idle, in milliseconds (default 250).
    pub ship_interval_ms: u64,
    /// How long a failing shipper may take before `RPO_BREACH`, in seconds (default 900).
    pub rpo_grace_seconds: u64,
    /// How often the chain is verified when nothing sealed, in seconds (default 600).
    pub verify_every_seconds: u64,
    /// The writer lease's time to live, in seconds (default 30).
    pub lease_seconds: u64,
    /// Per-site overrides by site name.
    pub sites: BTreeMap<String, SiteOverride>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            policy: None,
            on_unreachable: OnUnreachable::Degrade,
            target: None,
            region: None,
            seal_max_mib: 64,
            seal_max_seconds: 300,
            batch_max_entries: 2_000,
            ship_interval_ms: 250,
            rpo_grace_seconds: 900,
            verify_every_seconds: 600,
            lease_seconds: 30,
            sites: BTreeMap::new(),
        }
    }
}

/// One site's settings after the per-site table is applied.
#[derive(Clone, Debug)]
pub struct Resolved {
    /// The policy in force.
    pub policy: PolicyName,
    /// The unreachable-store behaviour.
    pub on_unreachable: OnUnreachable,
    /// The folder the journal is written to.
    pub target: PathBuf,
    /// Region text for the operator.
    pub region: Option<String>,
    /// Sealing limits.
    pub limits: SealLimits,
    /// The shipper's settings.
    pub batch_max_entries: usize,
    /// The idle pause.
    pub interval: Duration,
    /// Grace before `RPO_BREACH`.
    pub rpo_grace: Duration,
    /// Verification cadence.
    pub verify_every: Duration,
    /// The writer lease's time to live.
    pub lease: Duration,
}

impl Settings {
    /// The settings that apply to `site`, or `None` when the journal is off for it.
    ///
    /// # Errors
    /// An unknown target kind, a zero limit, or a policy named without a target.
    pub fn resolve(&self, site: &str) -> Result<Option<Resolved>> {
        let own = self.sites.get(site).cloned().unwrap_or_default();
        if own.enabled == Some(false) {
            return Ok(None);
        }
        let target = own.target.clone().or_else(|| self.target.clone());
        let policy = own.policy.or(self.policy);
        let Some(target) = target else {
            if policy.is_some() {
                return Err(Error(format!(
                    "services.journal: a policy is set for site `{site}` but there is no target"
                )));
            }
            return Ok(None);
        };
        if target.kind != "folder" {
            return Err(Error(
                "services.journal: only target kind `folder` is supported".to_owned(),
            ));
        }
        if self.batch_max_entries == 0 || self.seal_max_mib == 0 || self.lease_seconds == 0 {
            return Err(Error(
                "services.journal: batch_max_entries, seal_max_mib and lease_seconds must be positive"
                    .to_owned(),
            ));
        }
        Ok(Some(Resolved {
            policy: policy.unwrap_or(PolicyName::RemoteSync),
            on_unreachable: own.on_unreachable.unwrap_or(self.on_unreachable),
            target: target.path,
            region: self.region.clone(),
            limits: SealLimits {
                max_bytes: self.seal_max_mib.saturating_mul(1024 * 1024),
                max_age_millis: self.seal_max_seconds.saturating_mul(1_000),
            },
            batch_max_entries: self.batch_max_entries,
            interval: Duration::from_millis(self.ship_interval_ms.max(10)),
            rpo_grace: Duration::from_secs(self.rpo_grace_seconds),
            verify_every: Duration::from_secs(self.verify_every_seconds.max(1)),
            lease: Duration::from_secs(self.lease_seconds),
        }))
    }
}

/// What other services read: whether the journal is on, its policy and its progress. The
/// acknowledgement gate (WP-RTB-01.T4) waits on [`JournalHandle::progress`].
#[derive(Clone, Debug)]
pub struct JournalHandle {
    enabled: bool,
    policy: PolicyName,
    on_unreachable: OnUnreachable,
    progress: ProgressHandle,
}

impl JournalHandle {
    /// The handle of a site whose journal is off.
    #[must_use]
    pub fn off() -> Self {
        Self {
            enabled: false,
            policy: PolicyName::Local,
            on_unreachable: OnUnreachable::Degrade,
            progress: ProgressHandle::idle(),
        }
    }

    /// Whether a shipper runs for this site.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// The policy in force (meaningful only when [`JournalHandle::enabled`]).
    #[must_use]
    pub fn policy(&self) -> PolicyName {
        self.policy
    }

    /// The unreachable-store behaviour.
    #[must_use]
    pub fn on_unreachable(&self) -> OnUnreachable {
        self.on_unreachable
    }

    /// The shipper's durable position, lag and state.
    #[must_use]
    pub fn progress(&self) -> &ProgressHandle {
        &self.progress
    }

    /// `journal: remote-sync ok (lag 0s, shipped g1:s20931)`, `journal: degraded (...)`, or
    /// `journal: off (no off-site history)`. Never names a key, a path or an entry.
    #[must_use]
    pub fn readiness_text(&self) -> String {
        if !self.enabled {
            return "journal: off (no off-site history)".to_owned();
        }
        let text = self.progress.now().readiness(self.policy.text());
        match self.on_unreachable {
            OnUnreachable::Halt if self.policy == PolicyName::RemoteSync => {
                format!("{text} [halt]")
            }
            _ => text,
        }
    }
}

/// The start-up check: what must hold before the shipper may run.
pub trait StartupCheck: Send + Sync {
    /// `Err` carries a sentence an operator can act on.
    fn check(&self) -> PortFuture<'_, std::result::Result<(), String>>;
}

/// The production start-up check: the state table exists and the target is safe.
pub struct PgStartup {
    marks: PgWatermarks,
    target: PathBuf,
    forbidden: Vec<PathBuf>,
    canonical: Arc<OnceLock<PathBuf>>,
}

impl StartupCheck for PgStartup {
    fn check(&self) -> PortFuture<'_, std::result::Result<(), String>> {
        Box::pin(async move {
            if !self.marks.present().await {
                return Err(
                    "the journal's state table is missing or unreadable; the table is created by `spot migrate`"
                        .to_owned(),
                );
            }
            let canonical = check_folder_target(&self.target, &self.forbidden)
                .map_err(|e| format!("{}: {e}", e.code()))?;
            let _ = self.canonical.set(canonical);
            Ok(())
        })
    }
}

/// The journal service of one site.
pub struct JournalService<F: RelayFactory + 'static> {
    site: String,
    level: Level,
    interval: Duration,
    startup: Arc<dyn StartupCheck>,
    shipper: Mutex<Option<Shipper<F>>>,
    clock: Arc<dyn Clock>,
}

fn now_millis(clock: &dyn Clock) -> i64 {
    i64::try_from(clock.now_utc().unix_timestamp_nanos() / 1_000_000).unwrap_or(0)
}

impl<F: RelayFactory + 'static> Service for JournalService<F> {
    fn name(&self) -> &'static str {
        "journal"
    }

    fn level(&self) -> Level {
        self.level
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move { self.startup.check().await })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let Some(mut shipper) = self
                .shipper
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
            else {
                return Err("the journal service started without passing its check".to_owned());
            };
            let mut stop = stop;
            loop {
                let now = now_millis(self.clock.as_ref());
                let step = shipper.step(now).await;
                let pause = match &step {
                    Step::Shipped { .. } => Duration::ZERO,
                    Step::Degraded { code } => {
                        tracing::warn!(site = %self.site, code, "the journal shipper is degraded; retrying");
                        Shipper::<F>::backoff(self.interval, shipper.consecutive_failures())
                    }
                    Step::Stopped { code } => {
                        tracing::error!(site = %self.site, code, "the journal shipper stopped; it never writes on a broken chain");
                        shipper.shutdown(now).await;
                        return Err(format!("the journal shipper stopped: {code}"));
                    }
                    Step::Idle | Step::NotWriter => self.interval,
                };
                tokio::select! {
                    changed = stop.changed() => {
                        if changed.is_err() || *stop.borrow() {
                            shipper.shutdown(now_millis(self.clock.as_ref())).await;
                            return Ok(());
                        }
                    }
                    () = tokio::time::sleep(pause) => {}
                }
            }
        })
    }
}

/// Everything [`build_service`] needs besides the settings.
pub struct Parts<F: RelayFactory> {
    /// The site name (also the journal tenant and source).
    pub site: String,
    /// The settings in force.
    pub resolved: Resolved,
    /// The shipper's ports.
    pub ports: Ports<F>,
    /// The start-up check.
    pub startup: Arc<dyn StartupCheck>,
    /// The site clock.
    pub clock: Arc<dyn Clock>,
}

/// Builds the service and its handle. Exposed so a test drives the plain [`Service`] trait
/// without a `SiteCtx`.
///
/// # Errors
/// The shipper's configuration refusals.
pub fn build_service<F: RelayFactory + 'static>(
    parts: Parts<F>,
) -> Result<(Arc<JournalService<F>>, Arc<JournalHandle>)> {
    let Parts {
        site,
        resolved,
        ports,
        startup,
        clock,
    } = parts;
    let policy = resolved.policy;
    let mut cfg = ShipperConfig::new(&site, policy.text(), policy == PolicyName::RemoteSync);
    cfg.batch_max_entries = resolved.batch_max_entries;
    cfg.rpo_grace_millis = i64::try_from(resolved.rpo_grace.as_millis()).unwrap_or(i64::MAX);
    cfg.verify_every_millis = i64::try_from(resolved.verify_every.as_millis()).unwrap_or(i64::MAX);
    let (shipper, progress) =
        Shipper::new(cfg, ports).map_err(|e| Error(format!("services.journal: {}", e.code())))?;
    let required =
        policy == PolicyName::RemoteSync && resolved.on_unreachable == OnUnreachable::Halt;
    let service = Arc::new(JournalService {
        site,
        level: if required {
            Level::Required
        } else {
            Level::Optional
        },
        interval: resolved.interval,
        startup,
        shipper: Mutex::new(Some(shipper)),
        clock,
    });
    let handle = Arc::new(JournalHandle {
        enabled: true,
        policy,
        on_unreachable: resolved.on_unreachable,
        progress,
    });
    Ok((service, handle))
}

/// Raises and clears the shipper's conditions on the site's conditions handle.
pub struct ConditionsAlarms {
    handle: Arc<ConditionsHandle>,
    clock: Arc<dyn Clock>,
}

impl ConditionsAlarms {
    /// Alarms over `handle`, stamped with `clock`.
    #[must_use]
    pub fn new(handle: Arc<ConditionsHandle>, clock: Arc<dyn Clock>) -> Self {
        Self { handle, clock }
    }

    fn now(&self) -> Millis {
        Millis::new(u64::try_from(now_millis(self.clock.as_ref())).unwrap_or(0))
    }
}

impl Alarms for ConditionsAlarms {
    fn raise<'a>(&'a self, subject: &'a str, alarm: Alarm) -> PortFuture<'a, ()> {
        Box::pin(async move {
            let id = alarm.kind().condition_id();
            let params = match &alarm {
                Alarm::RpoBreach {
                    policy,
                    behind_secs,
                } => ConditionParams::new()
                    .with("policy", *policy)
                    .with("behind_for", format!("{behind_secs}s")),
                Alarm::JournalGap {
                    source,
                    generation,
                    range,
                } => ConditionParams::new()
                    .with("source", source.as_str())
                    .with("generation", generation.to_string())
                    .with("range", range.as_str()),
                Alarm::KeyBackupMissing { key } => ConditionParams::new().with("key", key.as_str()),
            };
            match self.handle.raise(id, subject, params, self.now()).await {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => {
                    tracing::warn!(
                        condition = id,
                        code = error.code(),
                        "the journal condition was refused"
                    );
                }
                Err(_) => tracing::warn!(condition = id, "the conditions service is not running"),
            }
        })
    }

    fn clear<'a>(&'a self, subject: &'a str, kind: AlarmKind) -> PortFuture<'a, ()> {
        Box::pin(async move {
            let id = kind.condition_id();
            if let Ok(Err(error)) = self.handle.observe_cleared(id, subject, self.now()).await {
                tracing::debug!(
                    condition = id,
                    code = error.code(),
                    "clearing a journal condition"
                );
            }
        })
    }
}

/// Builds the chain checker over its own handle on the folder each time it is asked, so the
/// folder only has to exist once the start-up check passed.
struct LazyChecker {
    canonical: Arc<OnceLock<PathBuf>>,
    keys: Arc<JournalKeys>,
    site: String,
}

impl ChainChecker for LazyChecker {
    fn check(&self, generation: u64) -> PortFuture<'_, std::result::Result<ChainState, ShipFault>> {
        Box::pin(async move {
            let path = self
                .canonical
                .get()
                .ok_or(ShipFault::Config("the journal target was not checked"))?;
            let folder = FolderStore::new(path).map_err(|_| ShipFault::StoreDown {
                code: "journal_store.io",
            })?;
            let scoped = TenantScoped::new(folder, &self.site)
                .map_err(|_| ShipFault::Config("the site name is not a usable journal tenant"))?;
            StoreChecker::new(scoped, self.keys.verifying(), &self.site, &self.site)?
                .check(generation)
                .await
        })
    }
}

/// What this module provides a site's ports on every build, kept across a reload (the running
/// service is not rebuilt, so the reloaded ports map needs the same handle).
fn provided() -> &'static Mutex<HashMap<String, Arc<JournalHandle>>> {
    static PROVIDED: OnceLock<Mutex<HashMap<String, Arc<JournalHandle>>>> = OnceLock::new();
    PROVIDED.get_or_init(Mutex::default)
}

/// The handle of the journal a running site hosts, by site name (for a harness).
#[must_use]
pub fn hosted(site: &str) -> Option<Arc<JournalHandle>> {
    provided()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(site)
        .cloned()
}

/// Registers the journal service. Provides [`JournalHandle`] always (off when no target is
/// configured). A reload re-provides the running handle and adds nothing.
pub async fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("journal")?;
    let site = ctx.label().to_owned();
    if !ctx.assembling() {
        let existing = hosted(&site);
        ctx.provide::<JournalHandle>(existing.unwrap_or_else(|| Arc::new(JournalHandle::off())))?;
        return Ok(());
    }
    let Some(resolved) = settings.resolve(&site)? else {
        tracing::info!(site = %site, "journal: off (no off-site history)");
        ctx.provide::<JournalHandle>(Arc::new(JournalHandle::off()))?;
        return Ok(());
    };
    let secrets = ctx.port::<SecretsHost>().ok_or_else(|| {
        Error(
            "services.journal needs [services.secrets]: the journal keys are derived from the site's key provider"
                .to_owned(),
        )
    })?;
    let provider: Arc<dyn KeyProvider> = secrets.keyring().clone();
    let keys = Arc::new(
        JournalKeys::derive(provider)
            .await
            .map_err(|e| Error(format!("services.journal: {}", e.code())))?,
    );
    let env = *ctx.env()?;
    let backend = env.backend.clone();
    // The journal must not live inside what it protects: the site data under `<root>/sites` and
    // the application directories.
    let mut forbidden = vec![env.root.join("sites")];
    for app in &env.loaded.common.apps {
        forbidden.push(if app.is_absolute() {
            app.clone()
        } else {
            env.root.join(app)
        });
    }
    let clock: Arc<dyn Clock> = Arc::clone(ctx.clock());
    let coordinator = coordinator_for(
        env.loaded.common.services.profile.as_str(),
        &backend,
        &site,
        &clock,
    )
    .await?;
    if let Some(region) = &resolved.region {
        tracing::info!(site = %site, region = %region, "journal: region recorded");
    }
    ctx.finally(move |ctx| {
        let conditions = ctx.require_port::<ConditionsHandle>()?;
        let protector: Arc<dyn Protector> = ctx.port::<dyn Protector>().unwrap_or_else(|| {
            tracing::warn!(
                site = %site,
                "journal: no personal-field protector is provided; lines ship as they are (WP-RTB-01.T8)"
            );
            Arc::new(PassThrough)
        });
        let source = SourceId::new(&site)
            .map_err(|_| Error(format!("the site name `{site}` is not a valid journal source")))?;
        let canonical: Arc<OnceLock<PathBuf>> = Arc::new(OnceLock::new());
        let make_store = {
            let canonical = Arc::clone(&canonical);
            let site = site.clone();
            Arc::new(move || {
                let path = canonical
                    .get()
                    .ok_or(ShipFault::Config("the journal target was not checked"))?;
                let folder = FolderStore::new(path).map_err(|_| ShipFault::StoreDown {
                    code: "journal_store.io",
                })?;
                TenantScoped::new(folder, &site)
                    .map_err(|_| ShipFault::Config("the site name is not a usable journal tenant"))
            })
        };
        let signer_keys = Arc::clone(&keys);
        let mut relay_config = RelayConfig::new(&site, &site, resolved.policy.durability())
            .with_tail_keys(keys.tail_keys());
        relay_config.limits = resolved.limits;
        let factory = StoreRelayFactory::new(
            make_store,
            Arc::new(move || signer_keys.signer()),
            keys.verifying(),
            relay_config,
        );
        let holder = format!("journal-{site}");
        let lease: Arc<dyn WriterLease> = Arc::new(CoordinatorLease::new(
            coordinator,
            format!("journal-ship:{site}"),
            holder,
            resolved.lease,
        ));
        let ports = Ports {
            outbox: Arc::new(PgOutbox::new(backend.clone(), source)),
            marks: Arc::new(PgWatermarks::new(backend.pool())),
            lease,
            protector,
            alarms: Arc::new(ConditionsAlarms::new(conditions, Arc::clone(&clock))),
            factory,
            checker: Some(Arc::new(LazyChecker {
                canonical: Arc::clone(&canonical),
                keys: Arc::clone(&keys),
                site: site.clone(),
            })),
            backup: None,
        };
        let startup = Arc::new(PgStartup {
            marks: PgWatermarks::new(backend.pool()),
            target: resolved.target.clone(),
            forbidden,
            canonical,
        });
        let (service, handle) = build_service(Parts {
            site: site.clone(),
            resolved,
            ports,
            startup,
            clock,
        })?;
        ctx.add(service);
        ctx.provide::<JournalHandle>(Arc::clone(&handle))?;
        provided()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(site, handle);
        Ok(())
    });
    Ok(())
}

/// The coordinator the writer lease lives in: PostgreSQL in the cluster profile, in process on
/// a single node (exactly one node, so exactly one writer).
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
        coordinator
            .prepare()
            .await
            .map_err(|e| Error(format!("services.journal: the coordinator: {}", e.code)))?;
        return Ok(Arc::new(coordinator));
    }
    let node = spot_coordinator::NodeId::new(format!("journal-{site}"))
        .map_err(|e| Error(format!("services.journal: the node name: {}", e.code)))?;
    let coordinator = spot_coordinator::InProcessCoordinator::start(
        Arc::new(spot_coordinator::InMemoryPresence::new()),
        node,
        Arc::clone(clock),
    )
    .await
    .map_err(|e| Error(format!("services.journal: the coordinator: {}", e.code)))?;
    Ok(coordinator)
}

/// A lease that is always held, for a harness that runs one shipper on its own.
#[must_use]
pub fn always_held() -> Arc<dyn WriterLease> {
    Arc::new(MemoryLease::held())
}
