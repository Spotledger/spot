//! The sandbox TTL reaper, hosted (WP-ALM-05.T5): the reap pass of `spot sandbox reap` run on an
//! interval by the running product, so a sandbox never outlives its time to live because nobody
//! remembered to run a command. Guide: `docs/running-spot/sandbox_reaper.md`.
//!
//! # Off by default
//!
//! `[services.sandbox_reaper] enabled` defaults to **false**. A sandbox holds a masked copy of
//! production, and the reaper can drop databases: only a host that is meant to own sandboxes turns
//! it on, on purpose. Without it, `spot sandbox reap` keeps working by hand.
//!
//! # What a pass does
//!
//! Under the lease `sandbox-reaper:<site>` (a PostgreSQL coordinator in the cluster profile;
//! without a coordinator the node is alone and the service runs without a lease and says so), a
//! pass lists the sandboxes **cloned from this site** (`sites/<name>/sandbox.toml` is the
//! registry: its record is the authority, never a name pattern), takes those past their expiry
//! (strictly before now; a creation unfinished for an hour counts too) oldest first, at most
//! `max_per_pass`, and destroys each: the database, then the site folder (and with it the
//! record). Destroying is **idempotent**: a half-destroyed sandbox is finished, not an error. One
//! result per sandbox is recorded (id, outcome `destroyed`, `already_gone` or `failed`, and on
//! failure a reason code: never a database name, path or connection string). A failure does not
//! stop the pass; a sandbox that failed three passes in a row is skipped for one hour and
//! reported as `stuck`.
//!
//! # What it never does
//!
//! It never destroys a database that is not named by a sandbox record and starts `sandbox_`; never
//! the source site of any registered sandbox, nor this site, nor any real site's database; never a
//! record marked `keep`, or with no expiry or one in the future; and it skips a pass whose clock
//! has gone backwards by more than the interval. A pass with nothing expired does one listing and
//! no database work.
//!
//! # Tenancy
//!
//! One site's service touches only the sandboxes whose record names that site as the source.
//!
//! The service is [`Level::Optional`]: it never stops a site.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use spot_coordinator::{Coordinator, Lease};
use spot_core::Clock;
use tokio::sync::watch;

use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

/// The module's name.
pub const NAME: &str = "sandbox_reaper";
/// After the modules it does not depend on; the registry's convention is 1000 and above.
pub const ORDER: u32 = 1950;
/// The file that marks a site folder as a sandbox (the same file `spot sandbox` writes).
pub const RECORD_FILE: &str = "sandbox.toml";
/// A creation that never finished is reaped after this many seconds.
pub const STALE_CREATING: i64 = 3_600;
/// After this many failed passes in a row a sandbox is skipped for [`STUCK_FOR`].
pub const STUCK_AFTER: u32 = 3;
/// How long a stuck sandbox is skipped, in seconds.
pub const STUCK_FOR: i64 = 3_600;
/// The lease's time to live; it is renewed before every destruction.
pub const LEASE_TTL: Duration = Duration::from_secs(120);
/// How many results the service remembers.
pub const HISTORY: usize = 100;

/// `[services.sandbox_reaper]`.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// Whether the service runs. Default **false**: see the module docs.
    pub enabled: bool,
    /// Seconds between passes, 30 to 86400.
    pub interval_seconds: u64,
    /// The most sandboxes one pass destroys, 1 to 50.
    pub max_per_pass: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_seconds: 300,
            max_per_pass: 5,
        }
    }
}

impl Settings {
    /// Checks the ranges.
    ///
    /// # Errors
    ///
    /// A message naming the setting.
    pub fn validate(&self) -> Result<()> {
        if !(30..=86_400).contains(&self.interval_seconds) {
            return Err(Error(
                "services.sandbox_reaper.interval_seconds must be between 30 and 86400".to_owned(),
            ));
        }
        if !(1..=50).contains(&self.max_per_pass) {
            return Err(Error(
                "services.sandbox_reaper.max_per_pass must be between 1 and 50".to_owned(),
            ));
        }
        Ok(())
    }
}

// ---- the registry and the destroyer (ports) ----

/// One sandbox of the registry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The sandbox's site name.
    pub name: String,
    /// The site it was cloned from.
    pub source: String,
    /// Its database.
    pub database: String,
    /// When it was created, Unix seconds.
    pub created_at: i64,
    /// When it expires, Unix seconds; 0 or less means "no expiry".
    pub expires_at: i64,
    /// `creating` or `active`.
    pub state: String,
    /// Marked to be kept: never reaped.
    pub keep: bool,
}

/// What the registry holds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Listing {
    /// Every sandbox of the root, of every source (the safety rules look at all of them).
    pub entries: Vec<Entry>,
    /// The databases of the root's real (non-sandbox) sites: never destroyed.
    pub real_databases: BTreeSet<String>,
}

/// The sandbox registry.
#[async_trait]
pub trait Registry: Send + Sync {
    /// Lists the registry. An error is a reason code, never a path.
    async fn list(&self) -> std::result::Result<Listing, String>;
}

/// What destroying a sandbox found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Destroyed {
    /// The database or the folder was there and is now gone.
    Gone,
    /// Neither was there: someone finished the job already.
    AlreadyGone,
}

/// Destroys one sandbox, idempotently.
#[async_trait]
pub trait Destroyer: Send + Sync {
    /// Drops the database and removes the site folder. An error is a reason code (`[a-z_]+`).
    async fn destroy(&self, entry: &Entry) -> std::result::Result<Destroyed, &'static str>;
}

// ---- results ----

/// How one sandbox ended in a pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Destroyed now.
    Destroyed,
    /// Already gone.
    AlreadyGone,
    /// Failed, with a reason code.
    Failed(&'static str),
}

/// One recorded result: the sandbox's id (its site name) and the outcome. Nothing else.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recorded {
    /// The sandbox's site name.
    pub id: String,
    /// What happened.
    pub outcome: Outcome,
    /// When, Unix seconds.
    pub at: i64,
}

/// Why a pass did nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Skipped {
    /// The clock is behind what this service saw earlier by more than the interval.
    ClockWentBack,
    /// Another node holds the lease.
    LeaseHeld,
    /// The coordinator could not be asked.
    CoordinatorDown,
    /// The registry could not be read.
    RegistryUnreadable,
}

/// What one pass did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PassReport {
    /// Set when the pass did no work.
    pub skipped: Option<Skipped>,
    /// One result per sandbox the pass tried.
    pub results: Vec<Recorded>,
    /// The pass lost its lease and stopped before the next destruction.
    pub lease_lost: bool,
    /// Sandboxes left for a later pass because of `max_per_pass`.
    pub remaining: usize,
}

#[derive(Default)]
struct Failures {
    in_a_row: u32,
    skip_until: i64,
}

#[derive(Default)]
struct State {
    last_now: Option<i64>,
    failures: HashMap<String, Failures>,
    next_expiry: Option<i64>,
    history: Vec<Recorded>,
    last_pass: Option<i64>,
}

/// The reaper: the pass, over ports.
pub struct Reaper {
    site: String,
    holder: String,
    settings: Settings,
    registry: Arc<dyn Registry>,
    destroyer: Arc<dyn Destroyer>,
    coordinator: Option<Arc<dyn Coordinator>>,
    clock: Arc<dyn Clock>,
    state: Mutex<State>,
}

impl Reaper {
    /// A reaper for `site`. `holder` is this node's identity in the lease.
    pub fn new(
        site: impl Into<String>,
        holder: impl Into<String>,
        settings: Settings,
        registry: Arc<dyn Registry>,
        destroyer: Arc<dyn Destroyer>,
        coordinator: Option<Arc<dyn Coordinator>>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            site: site.into(),
            holder: holder.into(),
            settings,
            registry,
            destroyer,
            coordinator,
            clock,
            state: Mutex::new(State::default()),
        }
    }

    fn lease_name(&self) -> String {
        format!("sandbox-reaper:{}", self.site)
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The results recorded so far, oldest first (at most [`HISTORY`]).
    #[must_use]
    pub fn history(&self) -> Vec<Recorded> {
        self.state().history.clone()
    }

    /// Whether `entry` is due, strictly before `now`.
    fn due(entry: &Entry, now: i64) -> bool {
        let expired = entry.expires_at > 0 && entry.expires_at < now;
        let stale =
            entry.state == "creating" && entry.created_at.saturating_add(STALE_CREATING) < now;
        expired || stale
    }

    /// The safety rules: whether `entry` may be destroyed at all, given the whole listing.
    fn allowed(&self, entry: &Entry, listing: &Listing) -> bool {
        if entry.keep || entry.source != self.site {
            return false;
        }
        // A sandbox with no expiry is only reaped when its creation never finished.
        if entry.expires_at <= 0 && entry.state != "creating" {
            return false;
        }
        // Never a site that is the source of any registered sandbox (an inconsistent registry),
        // never this site, never a real site's database, never a database that is not a sandbox's.
        let is_a_source = listing.entries.iter().any(|e| e.source == entry.name);
        let is_real_database = listing.real_databases.contains(&entry.database);
        entry.name != self.site
            && !is_a_source
            && !is_real_database
            && entry.database.starts_with("sandbox_")
    }

    /// The text readiness shows: `sandbox_reaper: next expiry in 3d 4h, 0 stuck`.
    #[must_use]
    pub fn status_line(&self) -> String {
        let now = self.clock.now_utc().unix_timestamp();
        let state = self.state();
        let stuck = state
            .failures
            .values()
            .filter(|f| f.skip_until > now)
            .count();
        let next = match state.next_expiry {
            None => "no sandbox".to_owned(),
            Some(at) if at < now => "overdue".to_owned(),
            Some(at) => format!("in {}", span(at - now)),
        };
        let lease = if self.coordinator.is_some() {
            ""
        } else {
            ", no lease (single node)"
        };
        format!("sandbox_reaper: next expiry {next}, {stuck} stuck{lease}")
    }

    /// Runs one pass.
    pub async fn run_pass(&self) -> PassReport {
        let now = self.clock.now_utc().unix_timestamp();
        // A clock that went backwards by more than the interval is suspicious: never reap on it.
        let interval = i64::try_from(self.settings.interval_seconds).unwrap_or(i64::MAX);
        {
            let mut state = self.state();
            if let Some(last) = state.last_now
                && now.saturating_add(interval) < last
            {
                tracing::warn!(site = %self.site, "sandbox_reaper: the clock went backwards; this pass is skipped");
                return PassReport {
                    skipped: Some(Skipped::ClockWentBack),
                    ..PassReport::default()
                };
            }
            state.last_now = Some(state.last_now.map_or(now, |last| last.max(now)));
            state.last_pass = Some(now);
        }
        let lease = match self.take_lease().await {
            Ok(lease) => lease,
            Err(skipped) => {
                return PassReport {
                    skipped: Some(skipped),
                    ..PassReport::default()
                };
            }
        };
        let report = self.pass(now, lease.as_ref()).await;
        if let (Some(coordinator), Some(lease)) = (&self.coordinator, &lease) {
            // Released even when the pass ended early or failed.
            let _ = coordinator.release(lease).await;
        }
        report
    }

    async fn take_lease(&self) -> std::result::Result<Option<Lease>, Skipped> {
        let Some(coordinator) = &self.coordinator else {
            return Ok(None);
        };
        match coordinator
            .acquire(&self.lease_name(), &self.holder, LEASE_TTL)
            .await
        {
            Ok(lease) => Ok(Some(lease)),
            Err(e) if e.code == spot_coordinator::error::LEASE_HELD => Err(Skipped::LeaseHeld),
            Err(_) => Err(Skipped::CoordinatorDown),
        }
    }

    /// `true` while the lease (if any) is still ours: renewed and fenced.
    async fn still_leading(&self, lease: Option<&Lease>) -> bool {
        let (Some(coordinator), Some(lease)) = (&self.coordinator, lease) else {
            return true;
        };
        coordinator.renew(lease, LEASE_TTL).await.is_ok()
            && coordinator
                .check_fence(&lease.name, lease.token)
                .await
                .is_ok()
    }

    async fn pass(&self, now: i64, lease: Option<&Lease>) -> PassReport {
        let mut report = PassReport::default();
        let listing = match self.registry.list().await {
            Ok(listing) => listing,
            Err(_) => {
                tracing::warn!(site = %self.site, "sandbox_reaper: the registry could not be read");
                report.skipped = Some(Skipped::RegistryUnreadable);
                return report;
            }
        };
        let mine: Vec<&Entry> = listing
            .entries
            .iter()
            .filter(|e| e.source == self.site && !e.keep && e.expires_at > 0)
            .collect();
        self.state().next_expiry = mine.iter().map(|e| e.expires_at).min();

        let mut due: Vec<&Entry> = listing
            .entries
            .iter()
            .filter(|e| Self::due(e, now))
            .filter(|e| {
                let allowed = self.allowed(e, &listing);
                if !allowed && e.source == self.site && !e.keep {
                    tracing::warn!(site = %self.site, sandbox = %e.name, "sandbox_reaper: a sandbox failed a safety rule and is left alone");
                }
                allowed
            })
            .filter(|e| {
                self.state()
                    .failures
                    .get(&e.name)
                    .is_none_or(|f| f.skip_until <= now)
            })
            .collect();
        due.sort_by(|a, b| (a.expires_at, &a.name).cmp(&(b.expires_at, &b.name)));
        let total = due.len();
        due.truncate(self.settings.max_per_pass);
        report.remaining = total - due.len();

        for entry in due {
            if !self.still_leading(lease).await {
                tracing::warn!(site = %self.site, "sandbox_reaper: the lease was lost; the pass stops");
                report.lease_lost = true;
                break;
            }
            let outcome = match self.destroyer.destroy(entry).await {
                Ok(Destroyed::Gone) => Outcome::Destroyed,
                Ok(Destroyed::AlreadyGone) => Outcome::AlreadyGone,
                Err(code) => Outcome::Failed(code),
            };
            self.record(entry, outcome, now);
            report.results.push(Recorded {
                id: entry.name.clone(),
                outcome,
                at: now,
            });
        }
        report
    }

    fn record(&self, entry: &Entry, outcome: Outcome, now: i64) {
        let mut state = self.state();
        match outcome {
            Outcome::Failed(code) => {
                let failures = state.failures.entry(entry.name.clone()).or_default();
                failures.in_a_row += 1;
                if failures.in_a_row >= STUCK_AFTER {
                    failures.skip_until = now.saturating_add(STUCK_FOR);
                }
                tracing::warn!(site = %self.site, sandbox = %entry.name, reason = code, "sandbox_reaper: a sandbox could not be destroyed");
            }
            Outcome::Destroyed | Outcome::AlreadyGone => {
                state.failures.remove(&entry.name);
                tracing::info!(site = %self.site, sandbox = %entry.name, "sandbox_reaper: a sandbox was destroyed");
            }
        }
        state.history.push(Recorded {
            id: entry.name.clone(),
            outcome,
            at: now,
        });
        let over = state.history.len().saturating_sub(HISTORY);
        state.history.drain(..over);
    }
}

/// `3d 4h`, `5h 12m` or `7m`.
#[must_use]
pub fn span(seconds: i64) -> String {
    let seconds = seconds.max(0);
    let (days, hours, minutes) = (
        seconds / 86_400,
        seconds % 86_400 / 3_600,
        seconds % 3_600 / 60,
    );
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else {
        format!("{minutes}m")
    }
}

// ---- the real registry and destroyer: the files `spot sandbox` writes ----

/// A sandbox record as `spot sandbox create` writes it. Lenient on purpose: a record the reaper
/// cannot read is left alone, and a future `keep` flag is honoured.
#[derive(Deserialize)]
struct RecordFile {
    source: String,
    database: String,
    created_at: i64,
    #[serde(default)]
    expires_at: i64,
    #[serde(default)]
    state: String,
    #[serde(default)]
    keep: bool,
}

/// The registry over `<root>/sites/*/sandbox.toml`.
pub struct FileRegistry {
    root: PathBuf,
    real_databases: BTreeSet<String>,
}

impl FileRegistry {
    /// A registry of the sandboxes under `root`. `real_databases` are the databases of the real
    /// sites of the configuration.
    #[must_use]
    pub fn new(root: PathBuf, real_databases: BTreeSet<String>) -> Self {
        Self {
            root,
            real_databases,
        }
    }
}

#[async_trait]
impl Registry for FileRegistry {
    async fn list(&self) -> std::result::Result<Listing, String> {
        let sites = self.root.join("sites");
        let mut entries = Vec::new();
        let Ok(dirs) = std::fs::read_dir(&sites) else {
            return Ok(Listing {
                entries,
                real_databases: self.real_databases.clone(),
            });
        };
        for dir in dirs.flatten() {
            let Some(name) = dir.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Ok(text) = std::fs::read_to_string(dir.path().join(RECORD_FILE)) else {
                continue;
            };
            // A record that does not parse is not a registry row: it is left alone.
            let Ok(record) = toml::from_str::<RecordFile>(&text) else {
                continue;
            };
            entries.push(Entry {
                name,
                source: record.source,
                database: record.database,
                created_at: record.created_at,
                expires_at: record.expires_at,
                state: record.state,
                keep: record.keep,
            });
        }
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Listing {
            entries,
            real_databases: self.real_databases.clone(),
        })
    }
}

/// Drops a sandbox's database and removes its folder.
pub struct PgDestroyer {
    root: PathBuf,
    admin: spot_postgres::PgConfig,
}

impl PgDestroyer {
    /// A destroyer over `root`, with the maintenance connection `admin`.
    #[must_use]
    pub fn new(root: PathBuf, admin: spot_postgres::PgConfig) -> Self {
        Self { root, admin }
    }
}

/// `name` as a quoted identifier when it is a plain storage name.
fn plain(name: &str) -> Option<String> {
    let ok = !name.is_empty()
        && name.len() <= 63
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    ok.then(|| format!("\"{name}\""))
}

#[async_trait]
impl Destroyer for PgDestroyer {
    async fn destroy(&self, entry: &Entry) -> std::result::Result<Destroyed, &'static str> {
        // The record names the database; refuse anything that is not a plain sandbox database
        // and any site name that is not a plain folder name.
        let quoted = plain(&entry.database).ok_or("invalid_database")?;
        if !entry.database.starts_with("sandbox_") {
            return Err("not_a_sandbox_database");
        }
        if spot_core::SiteId::new(&entry.name).is_err() {
            return Err("invalid_site");
        }
        let admin = self.admin.clone().with_dbname("postgres");
        let backend = spot_postgres::PgBackend::connect(admin)
            .await
            .map_err(|_| "database_unreachable")?;
        let client = backend
            .pool()
            .get()
            .await
            .map_err(|_| "database_unreachable")?;
        let existed = client
            .query_opt(
                "SELECT 1 FROM pg_database WHERE datname = $1",
                &[&entry.database],
            )
            .await
            .map_err(|_| "database_unreachable")?
            .is_some();
        client
            .batch_execute(&format!("DROP DATABASE IF EXISTS {quoted} WITH (FORCE)"))
            .await
            .map_err(|_| "drop_failed")?;
        let dir = self.root.join("sites").join(&entry.name);
        let folder = dir.exists();
        if folder {
            // The record is inside: removing the folder removes the registry row last.
            std::fs::remove_dir_all(&dir).map_err(|_| "folder_not_removed")?;
        }
        Ok(if existed || folder {
            Destroyed::Gone
        } else {
            Destroyed::AlreadyGone
        })
    }
}

// ---- the service ----

struct ReaperService {
    reaper: Arc<Reaper>,
    interval: Duration,
}

impl Service for ReaperService {
    fn name(&self) -> &'static str {
        NAME
    }

    fn level(&self) -> Level {
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async { Ok(()) })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            tracing::info!("{}", self.reaper.status_line());
            loop {
                // A pass runs in its own task: a panic in it is a failed pass, not a stopped
                // service.
                let reaper = Arc::clone(&self.reaper);
                let _ = tokio::spawn(async move { reaper.run_pass().await }).await;
                tokio::select! {
                    () = tokio::time::sleep(self.interval) => {}
                    _ = stop.changed() => return Ok(()),
                }
                if *stop.borrow() {
                    return Ok(());
                }
            }
        })
    }
}

/// Registers the service when `[services.sandbox_reaper] enabled = true`.
///
/// # Errors
///
/// A setting out of range, or a context without a site.
pub async fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    if !ctx.assembling() {
        return Ok(());
    }
    let settings: Settings = ctx.settings_or_default(NAME)?;
    if !settings.enabled {
        return Ok(());
    }
    settings.validate()?;
    let env = *ctx.env()?;
    let coordinator: Option<Arc<dyn Coordinator>> =
        if env.loaded.common.services.profile == "cluster" {
            let coordinator = spot_coordinator_pg::PgCoordinator::new(
                env.backend.pool(),
                spot_coordinator_pg::PgCoordinatorConfig::default(),
            );
            coordinator.prepare().await.map_err(crate::error)?;
            Some(Arc::new(coordinator))
        } else {
            None
        };
    let real_databases = env
        .loaded
        .sites
        .iter()
        .filter(|s| {
            !Path::new(env.root)
                .join("sites")
                .join(&s.site)
                .join(RECORD_FILE)
                .exists()
        })
        .map(|s| s.database.clone())
        .collect();
    let holder = {
        let id = spot_storage_contract::RecordId::new_v7().to_string();
        let part = id.rsplit('-').next().unwrap_or(&id).to_owned();
        format!("node-{}-{part}", env.site.site)
    };
    let interval = Duration::from_secs(settings.interval_seconds);
    let reaper = Arc::new(Reaper::new(
        env.site.site.clone(),
        holder,
        settings,
        Arc::new(FileRegistry::new(env.root.to_path_buf(), real_databases)),
        Arc::new(PgDestroyer::new(
            env.root.to_path_buf(),
            env.loaded.admin.clone(),
        )),
        coordinator,
        Arc::clone(ctx.clock()),
    ));
    ctx.add(Arc::new(ReaperService { reaper, interval }));
    Ok(())
}
