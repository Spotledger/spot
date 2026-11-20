//! The egress policy, hosted once per site (WP-SEC-09.T7). Operator guide:
//! `docs/running-spot/egress.md`.
//!
//! # What is hosted
//!
//! `spot-egress` decides whether an outbound connection may be made. Until this module every
//! caller built its own `Guard`: the mail sender and the integration nodes each made a policy and
//! an in-memory sink, so a denial vanished when the process ended and nobody could say what a
//! tenant may reach. This module builds **one** guard per site from `[services.egress]` of
//! `common.toml`:
//!
//! - `policy_file`: a TOML file read with `EgressPolicy::from_toml` plus the checks of
//!   `policy.rs` (every problem reported at once, the file not world-writable). The tenant
//!   allow-list lives here. Absent means the built-in default, `deny_non_public`: public
//!   addresses on ordinary ports for a caller that names no tenant entry, nothing else.
//! - `denial_log`: the append-only denial file (default `<root>/sites/<site>/egress-denials.log`,
//!   mode 0600), `denial_log_max_bytes` (default 8 MiB, four files kept) and
//!   `denials_per_minute` (default 60 per tenant, purpose, host and port).
//!
//! A later module takes `ctx.port::<Guard>()` instead of inventing a policy, and
//! `ctx.port::<EgressStatus>()` to read the counts. Both are provided at `ORDER` 950, below every
//! module that registers at 1000 or later.
//!
//! # The denial chain
//!
//! `Guard` -> [`CountingSink`] (counts per purpose, `spot_egress_denied_total{purpose}`) ->
//! `RateLimitedSink` (the per-minute budget) -> [`FileDenialSink`] (one JSON line). The counts sit
//! outside the budget, so they stay complete when lines are dropped; the difference is reported
//! as `suppressed`. The purpose is a closed set; no host or tenant ever becomes a metric label.
//!
//! # Level, and what a failure does
//!
//! **Optional.** A table the settings refuse, or a policy file that is unreadable, writable by
//! everyone or invalid, stops the site from starting (the error lists every problem): a site
//! must not run with a policy nobody meant. Once running:
//!
//! - a denial log that cannot be written degrades the site (readiness says so) and counts the
//!   failures; the request is refused exactly as before. A failed write never turns a denial into
//!   an allow, and the caller never sees the difference;
//! - a reload (SIGHUP, or `spot egress reload`) that does not parse keeps the previous policy,
//!   degrades the site with the problem count and logs the problems; the next good reload clears
//!   it.
//!
//! # Reload and the shared handle
//!
//! `Guard` holds its policy by value, so an `Arc<Guard>` cannot change after it is made. The
//! site therefore provides a [`SharedGuard`] (`ctx.port::<SharedGuard>()`): a handle whose
//! identity never changes and whose guard is swapped atomically on a reload. A holder cloned
//! before the reload sees the new policy on its next decision (a removed allow is denied, a new
//! allow works); a decision in flight during a swap uses the old or the new policy, never a mix.
//! A reload that fails validation changes nothing and reports the catalogued error
//! (`egress_host.policy_*`, `egress_host.reload_failed`). Every applied swap writes one audit
//! line (target `spot::audit::egress_policy`: generation, digest prefix and tenant counts, no
//! host or entry) and is kept in [`SharedGuard::swaps`].
//!
//! The plain `Arc<Guard>` is still provided for callers that have not moved; it is the policy
//! **at start** and does not follow reloads. New code takes the [`SharedGuard`].

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde::Deserialize;
use spot_core::Clock;
use spot_egress::{DenialSink, EgressPolicy, Guard, RateLimitedSink, Resolver, SystemResolver};
use tokio::sync::watch;

#[path = "egress/denials.rs"]
mod denials;
#[path = "egress/ops.rs"]
mod ops;
#[path = "egress/policy.rs"]
mod policy;
#[path = "egress/shared.rs"]
mod shared;

pub use denials::{
    CountingSink, DenialLine, DenialQuery, FILE_KEEP, FileDenialSink, LIMIT_MAX, parse_since,
    read_denials,
};
pub use ops::{
    CheckOutcome, CheckRequest, Paths, check, parse_purpose, request_reload, take_reload,
};
pub use policy::{LoadedPolicy, MAX_POLICY_BYTES, PURPOSES, PolicyProblem, load_policy_file};
pub use shared::{AUDIT_TARGET, SWAP_LOG_KEEP, SharedGuard, SwapAudit};

use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

/// Start order: after metadata (300) and customise (400), before every module that registers at
/// 1000 or later and may take the guard (mail, connectors, webhooks, the app runtime, the LLM
/// service).
pub const ORDER: u32 = 950;

/// The module and service name, and the `[services.<name>]` table it reads.
pub const NAME: &str = "egress";

/// The counter of refused requests, labelled by purpose only.
pub const METRIC_DENIED: &str = "spot_egress_denied_total";

/// The stable codes this module answers with (see `catalogue/egress_host.toml`). A refusal is
/// `"<code>: <text>"`; the text names a key, a file kind or a count, never a secret.
pub mod codes {
    /// The `[services.egress]` table has an unknown key, a value of the wrong type or a value
    /// out of range.
    pub const SETTINGS_INVALID: &str = "egress_host.settings_invalid";
    /// A configured path contains `..`, is relative where no root exists, or is shared by sites.
    pub const PATH_REFUSED: &str = "egress_host.path_refused";
    /// The policy file is missing, not a regular file or unreadable.
    pub const POLICY_UNREADABLE: &str = "egress_host.policy_unreadable";
    /// The policy file is writable by everyone.
    pub const POLICY_UNSAFE: &str = "egress_host.policy_unsafe";
    /// The policy file does not parse or holds an entry that is not valid.
    pub const POLICY_INVALID: &str = "egress_host.policy_invalid";
    /// The last policy reload failed; the previous policy is in force.
    pub const RELOAD_FAILED: &str = "egress_host.reload_failed";
    /// The denial log cannot be written.
    pub const DENIAL_LOG_UNWRITABLE: &str = "egress_host.denial_log_unwritable";
    /// `spot egress check` was given something that is not an http or https URL.
    pub const URL_INVALID: &str = "egress_host.url_invalid";
    /// `spot egress check` or `denials` was given a purpose outside the closed set.
    pub const PURPOSE_UNKNOWN: &str = "egress_host.purpose_unknown";
    /// `spot egress denials` was given a duration or a limit it cannot read.
    pub const QUERY_INVALID: &str = "egress_host.query_invalid";
}

fn refuse(code: &str, text: impl std::fmt::Display) -> Error {
    Error(format!("{code}: {text}"))
}

// ---------------------------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------------------------

/// `[services.egress]` of `common.toml`. Unknown keys are refused.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// The policy file (TOML). A relative path is relative to the host root; `{site}` is the
    /// site's name. Absent: the built-in default policy.
    pub policy_file: Option<PathBuf>,
    /// The denial log. Default `<root>/sites/<site>/egress-denials.log`.
    pub denial_log: Option<PathBuf>,
    /// The size at which the denial log rotates, 64 KiB to 1 GiB (default 8 MiB).
    pub denial_log_max_bytes: Option<u64>,
    /// The denials one tenant, purpose, host and port may write per minute, 1 to 100000
    /// (default 60). Denials beyond it are counted, not written.
    pub denials_per_minute: Option<u32>,
}

impl Settings {
    /// The rotation size.
    pub fn max_bytes(&self) -> u64 {
        self.denial_log_max_bytes.unwrap_or(8 * 1024 * 1024)
    }

    /// The per-minute budget.
    pub fn per_minute(&self) -> u32 {
        self.denials_per_minute.unwrap_or(60)
    }

    /// Decodes the table (`None` is the default settings), naming a key and never a value.
    ///
    /// # Errors
    ///
    /// `egress_host.settings_invalid`.
    pub fn from_table(table: Option<&toml::Value>) -> Result<Self> {
        let Some(table) = table else {
            return Ok(Self::default());
        };
        let settings: Self = match table.clone().try_into() {
            Ok(settings) => settings,
            Err(e) => {
                let message = e.message();
                let why = if message.starts_with("unknown field") {
                    message.to_owned()
                } else {
                    "a value has the wrong type".to_owned()
                };
                return Err(refuse(
                    codes::SETTINGS_INVALID,
                    format!("services.egress: {why}"),
                ));
            }
        };
        settings.check()?;
        Ok(settings)
    }

    /// Range checks.
    ///
    /// # Errors
    ///
    /// `egress_host.settings_invalid`.
    pub fn check(&self) -> Result<()> {
        if !(64 * 1024..=1 << 30).contains(&self.max_bytes()) {
            return Err(refuse(
                codes::SETTINGS_INVALID,
                "services.egress.denial_log_max_bytes: use 65536 to 1073741824",
            ));
        }
        if !(1..=100_000).contains(&self.per_minute()) {
            return Err(refuse(
                codes::SETTINGS_INVALID,
                "services.egress.denials_per_minute: use 1 to 100000",
            ));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// The host
// ---------------------------------------------------------------------------------------------

/// What a reload did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reloaded {
    /// A new policy is in force.
    Applied,
    /// The file is as it was (or there is no file to re-read).
    Unchanged,
}

struct State {
    digest: String,
    tenants: Vec<String>,
    generation: u64,
    failure: Option<PolicyProblem>,
}

fn read<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|e| e.into_inner())
}

fn write<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(|e| e.into_inner())
}

/// The site's egress service: the guard, the sinks and the reload loop.
pub struct EgressHost {
    paths: Paths,
    clock: Arc<dyn Clock>,
    resolver: Arc<dyn Resolver>,
    file: Arc<FileDenialSink>,
    counting: Arc<CountingSink>,
    shared: Arc<SharedGuard>,
    state: RwLock<State>,
}

impl EgressHost {
    /// Builds the host: loads the policy file (when configured), opens the denial log and builds
    /// the guard. The resolver is a parameter so tests can script DNS; the server passes
    /// `SystemResolver`.
    ///
    /// # Errors
    ///
    /// The policy file's refusal, rendered with every problem (`egress_host.policy_*`).
    pub fn build(
        settings: &Settings,
        paths: Paths,
        clock: Arc<dyn Clock>,
        resolver: Arc<dyn Resolver>,
    ) -> Result<Arc<Self>> {
        let (policy, digest, tenants) = match &paths.policy_file {
            Some(path) => {
                let loaded = load_policy_file(path).map_err(|p| Error(p.render()))?;
                (loaded.policy, loaded.digest, loaded.tenants)
            }
            None => (
                EgressPolicy::default(),
                policy::digest_of(b"built-in: deny_non_public"),
                Vec::new(),
            ),
        };
        if policy.default == "allow" {
            tracing::warn!(
                "the egress policy default is `allow`: an unlisted tenant may reach any public host"
            );
        }
        let file = Arc::new(FileDenialSink::new(
            &paths.denial_log,
            settings.max_bytes(),
            clock.clone(),
        ));
        let limited = Arc::new(RateLimitedSink::new(
            file.clone() as Arc<dyn DenialSink>,
            settings.per_minute(),
        ));
        let counting = Arc::new(CountingSink::new(limited as Arc<dyn DenialSink>));
        let guard = Arc::new(Guard::new(
            policy,
            resolver.clone(),
            counting.clone() as Arc<dyn DenialSink>,
            clock.clone(),
        ));
        Ok(Arc::new(Self {
            shared: Arc::new(SharedGuard::new(guard)),
            paths,
            clock,
            resolver,
            file,
            counting,
            state: RwLock::new(State {
                digest,
                tenants,
                generation: 1,
                failure: None,
            }),
        }))
    }

    /// The guard in force now. It is a snapshot: it does not follow a later reload. Hold
    /// [`EgressHost::shared`] instead.
    pub fn guard(&self) -> Arc<Guard> {
        self.shared.current()
    }

    /// The handle whose guard is swapped on a reload; it never changes identity.
    pub fn shared(&self) -> Arc<SharedGuard> {
        self.shared.clone()
    }

    /// The files this host uses.
    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    /// Re-reads the policy file. A file that fails keeps the previous policy and is remembered
    /// (readiness shows it) until a later reload succeeds.
    ///
    /// # Errors
    ///
    /// The refusal of the new file; the old policy stays in force.
    pub fn reload(&self) -> std::result::Result<Reloaded, PolicyProblem> {
        let Some(path) = &self.paths.policy_file else {
            return Ok(Reloaded::Unchanged);
        };
        match load_policy_file(path) {
            Ok(loaded) => {
                let mut state = write(&self.state);
                state.failure = None;
                if state.digest == loaded.digest {
                    return Ok(Reloaded::Unchanged);
                }
                let guard = Arc::new(Guard::new(
                    loaded.policy,
                    self.resolver.clone(),
                    self.counting.clone() as Arc<dyn DenialSink>,
                    self.clock.clone(),
                ));
                let previous_tenants = state.tenants.len();
                state.digest = loaded.digest;
                state.tenants = loaded.tenants;
                state.generation += 1;
                // The swap happens while the state lock is held, so two reloads cannot publish
                // out of order and the status always describes the guard in force.
                self.shared.swap(
                    guard,
                    SwapAudit {
                        generation: state.generation,
                        digest: state.digest.chars().take(12).collect(),
                        tenants: state.tenants.len(),
                        previous_tenants,
                    },
                );
                Ok(Reloaded::Applied)
            }
            Err(problem) => {
                write(&self.state).failure = Some(problem.clone());
                Err(problem)
            }
        }
    }

    fn reload_and_log(&self, trigger: &str) {
        match self.reload() {
            Ok(Reloaded::Applied) => {
                tracing::info!(trigger, "the egress policy was reloaded");
            }
            Ok(Reloaded::Unchanged) => {
                tracing::debug!(trigger, "the egress policy file is unchanged");
            }
            Err(problem) => {
                tracing::error!(
                    trigger,
                    code = problem.code,
                    problems = %problem.problems.join("; "),
                    "the egress policy reload failed; the previous policy stays in force"
                );
            }
        }
    }
}

impl Service for EgressHost {
    fn name(&self) -> &'static str {
        NAME
    }

    fn level(&self) -> Level {
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            if let Some(failure) = &read(&self.state).failure {
                return Err(format!(
                    "{}: the last policy reload failed ({} problem(s)) and the previous policy is in force; run spot egress reload for the list",
                    codes::RELOAD_FAILED,
                    failure.problems.len()
                ));
            }
            if !self.file.healthy() {
                return Err(format!(
                    "{}: denials are refused and counted but cannot be recorded",
                    codes::DENIAL_LOG_UNWRITABLE
                ));
            }
            Ok(())
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            #[cfg(unix)]
            let mut hangup =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()).ok();
            loop {
                #[cfg(unix)]
                let hup = async {
                    match hangup.as_mut() {
                        Some(signal) => {
                            signal.recv().await;
                        }
                        None => std::future::pending::<()>().await,
                    }
                };
                #[cfg(not(unix))]
                let hup = std::future::pending::<()>();
                tokio::select! {
                    _ = stop.changed() => break,
                    _ = tick.tick() => {
                        if take_reload(&self.paths.reload_marker) {
                            self.reload_and_log("marker");
                        }
                    }
                    () = hup => self.reload_and_log("signal"),
                }
            }
            Ok(())
        })
    }
}

// ---------------------------------------------------------------------------------------------
// The status port
// ---------------------------------------------------------------------------------------------

/// What a module or an operator tool may read about the site's egress: counts, the tenant names
/// the policy lists and its digest. It cannot change anything.
pub struct EgressStatus {
    host: Arc<EgressHost>,
}

impl EgressStatus {
    /// A view over `host`.
    pub fn new(host: Arc<EgressHost>) -> Self {
        Self { host }
    }

    /// The guard in force now, as a snapshot (see the module documentation).
    pub fn guard(&self) -> Arc<Guard> {
        self.host.guard()
    }

    /// The handle that follows reloads.
    pub fn shared(&self) -> Arc<SharedGuard> {
        self.host.shared()
    }

    /// The most recent policy swaps, oldest first.
    pub fn swaps(&self) -> Vec<SwapAudit> {
        self.host.shared.swaps()
    }

    /// Denials since start, per purpose (every purpose listed).
    pub fn denied_by_purpose(&self) -> BTreeMap<&'static str, u64> {
        self.host.counting.by_purpose()
    }

    /// All denials since start.
    pub fn denied_total(&self) -> u64 {
        self.host.counting.total()
    }

    /// Denials counted but not written because the per-minute budget was spent.
    pub fn suppressed(&self) -> u64 {
        self.host
            .counting
            .total()
            .saturating_sub(self.host.file.written())
            .saturating_sub(self.host.file.failed())
    }

    /// Denials that could not be written to the file.
    pub fn write_failures(&self) -> u64 {
        self.host.file.failed()
    }

    /// The tenant names the policy lists, sorted.
    pub fn tenants(&self) -> Vec<String> {
        read(&self.host.state).tenants.clone()
    }

    /// The SHA-256 of the policy in force (of its file, or of the built-in default).
    pub fn digest(&self) -> String {
        read(&self.host.state).digest.clone()
    }

    /// 1 at start, plus one per applied reload.
    pub fn generation(&self) -> u64 {
        read(&self.host.state).generation
    }

    /// The problems of the last failed reload, empty when the last reload was fine.
    pub fn reload_problems(&self) -> Vec<String> {
        read(&self.host.state)
            .failure
            .as_ref()
            .map(|f| f.problems.clone())
            .unwrap_or_default()
    }

    /// Whether the denial log took the last line.
    pub fn log_healthy(&self) -> bool {
        self.host.file.healthy()
    }
}

// ---------------------------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------------------------

/// Registers the egress service and provides `Arc<SharedGuard>`, `Arc<Guard>` (the start-time
/// snapshot) and `Arc<EgressStatus>`.
///
/// A reload build (`assembling` false) keeps the running service and builds nothing: ports are
/// provided on the assembling build only.
///
/// # Errors
///
/// A refused table, path or policy file (every problem listed).
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx
        .settings_or_default(NAME)
        .map_err(|e| refuse(codes::SETTINGS_INVALID, e))?;
    settings.check()?;
    if !ctx.assembling() {
        return Ok(());
    }
    let (root, sites) = match ctx.env() {
        Ok(env) => (Some(env.root.to_path_buf()), env.loaded.sites.len()),
        Err(_) => (None, 1),
    };
    let paths = Paths::resolve(&settings, root.as_deref(), ctx.label(), sites)?;
    let host = EgressHost::build(
        &settings,
        paths,
        ctx.clock().clone(),
        Arc::new(SystemResolver),
    )?;
    ctx.provide(host.shared())?;
    ctx.provide(host.guard())?;
    ctx.provide(Arc::new(EgressStatus::new(host.clone())))?;
    ctx.add(host);
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn settings_have_bounds_and_refuse_unknown_keys() {
        let table: toml::Value = toml::from_str("denials_per_minute = 0").unwrap();
        assert!(Settings::from_table(Some(&table)).is_err());
        let table: toml::Value = toml::from_str("surprise = 1").unwrap();
        let message = Settings::from_table(Some(&table)).err().unwrap().0;
        assert!(message.starts_with(codes::SETTINGS_INVALID), "{message}");
        assert!(Settings::from_table(None).is_ok());
    }

    #[test]
    fn the_metric_name_has_no_dot() {
        assert!(!METRIC_DENIED.contains('.'));
    }
}
