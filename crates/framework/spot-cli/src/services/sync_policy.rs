//! The hosted sync policy: what syncs, decided from the site's metadata (WP-RTB-02.T10; ADR-063;
//! guide: `docs/running-spot/sync_policy.md`).
//!
//! Runs in **every** role: a hub needs it to decide what goes down and to build its destinations,
//! an edge to decide what is pushed up and which DocTypes it may not write. It owns one
//! [`spot_sync_policy_host::PolicyHost`] for the site, loads the generation of the metadata the
//! site serves, **re-loads it on every metadata swap**, and provides:
//!
//! * [`PolicyHandle`] (port): the verified generation for the stream seams (`may_leave`,
//!   `edge_may_write`, the snapshot the hub's providers are built from, a watch of the generation
//!   number so the hub rebuilds its destinations and re-reads its subscriptions on a new one);
//! * [`PolicyReadiness`] (port): the operator's line, for example
//!   `sync_policy: generation 14` or `sync_policy: refused generation 15 (sync_policy.closed_link
//!   on Invoice), serving 14`.
//!
//! # Settings
//!
//! ```toml
//! [services.sync_policy]
//! default_preset = "local"         # local | master | record | handover; runtime-created DocTypes only
//! default_owner_scope = "warehouse" # the key type a record/handover default is owned by
//!
//! [services.sync_policy.declare.Customer]   # the metadata text form, one key per line
//! sync_class = "master"
//! sync_direction = "down"
//!
//! [[services.sync_policy.scope_field]]      # which field of a record carries a scope key's value
//! doctype = "Invoice"
//! key = "warehouse"
//! field = "warehouse"
//! ```
//!
//! # Fail closed
//!
//! Until a generation has loaded nothing leaves the node. A generation with any lint problem is
//! refused whole; the previous one keeps serving and the readiness line says why. The default
//! preset is declared into the generation before the lints run, so one that cannot hold refuses
//! the generation like any other policy problem.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};

use serde::Deserialize;
use spot_http::health::{CheckResult, ReadinessCheck};
use spot_sync_policy::{PolicySet, Preset, ScopeKeyType};
use spot_sync_policy_host::scope::{DestinationFactory, FieldReader, PolicyScopeKeys};
use spot_sync_policy_host::{Defaults, PolicyHost, Snapshot};
use tokio::sync::watch;

use crate::live::SiteLive;
use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

/// The module's name and its `[services.<name>]` table.
pub const NAME: &str = "sync_policy";
/// After `node_role` (1005), before the modules that read the port (1150 and above).
pub const ORDER: u32 = 1100;

/// Stable code: the default preset is not one of the four.
pub const DEFAULT_PRESET_UNKNOWN: &str = "sync_policy_host.default_preset_unknown";
/// Stable code: a declaration of `[services.sync_policy.declare.*]` does not parse.
pub const DECLARATION_INVALID: &str = "sync_policy_host.declaration_invalid";

/// One scope field of the configuration.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ScopeField {
    /// The DocType.
    pub doctype: String,
    /// The scope key type.
    pub key: String,
    /// The field of the DocType whose value is the key's value.
    pub field: String,
}

/// `[services.sync_policy]`.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// The preset an undeclared runtime-created DocType follows; default `local`.
    pub default_preset: Option<String>,
    /// The scope key type a `record` or `handover` default is owned by.
    pub default_owner_scope: Option<String>,
    /// Declared policies, by DocType, in the metadata text form (`sync_class`, `sync_direction`, ...).
    pub declare: BTreeMap<String, BTreeMap<String, String>>,
    /// Which field supplies each scope key's value.
    pub scope_field: Vec<ScopeField>,
}

impl Settings {
    /// The defaults the settings state.
    ///
    /// # Errors
    ///
    /// [`Error`] (`sync_policy_host.default_preset_unknown`) naming the accepted presets.
    pub fn defaults(&self) -> Result<Defaults> {
        let preset = match self.default_preset.as_deref() {
            None => Preset::Local,
            Some(text) => Preset::parse(text).ok_or_else(|| {
                Error(format!(
                    "{DEFAULT_PRESET_UNKNOWN}: default_preset must be one of {}",
                    Preset::TEXTS.join(", ")
                ))
            })?,
        };
        let owner_scope = self
            .default_owner_scope
            .as_deref()
            .map(ScopeKeyType::new)
            .transpose()
            .map_err(|e| {
                Error(format!(
                    "{DEFAULT_PRESET_UNKNOWN}: default_owner_scope: {e}"
                ))
            })?;
        Ok(Defaults {
            preset,
            owner_scope,
        })
    }

    /// The declared policies, parsed from their text form.
    ///
    /// # Errors
    ///
    /// [`Error`] (`sync_policy_host.declaration_invalid`) naming the DocType and the key, never
    /// the value.
    pub fn declared(&self) -> Result<PolicySet> {
        let mut set = PolicySet::new();
        for (doctype, properties) in &self.declare {
            for (key, value) in properties {
                set.parse(doctype, key, value).map_err(|e| {
                    Error(format!(
                        "{DECLARATION_INVALID}: services.sync_policy.declare.{doctype}.{key}: {}",
                        e.code()
                    ))
                })?;
            }
        }
        Ok(set)
    }

    /// `(DocType, key type)` to the field that supplies the value.
    #[must_use]
    pub fn scope_fields(&self) -> BTreeMap<(String, String), String> {
        self.scope_field
            .iter()
            .map(|entry| {
                (
                    (entry.doctype.clone(), entry.key.clone()),
                    entry.field.clone(),
                )
            })
            .collect()
    }
}

/// The verified generation, as a port.
pub struct PolicyHandle {
    host: Mutex<PolicyHost>,
    fields: BTreeMap<(String, String), String>,
    generation: watch::Sender<u64>,
}

impl PolicyHandle {
    /// A handle over `host`.
    #[must_use]
    pub fn new(host: PolicyHost, fields: BTreeMap<(String, String), String>) -> Self {
        Self {
            host: Mutex::new(host),
            fields,
            generation: watch::channel(0).0,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, PolicyHost> {
        self.host.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Loads a generation; the generation watch moves only when it loaded.
    ///
    /// # Errors
    ///
    /// The refusal record's readiness text.
    pub fn load(
        &self,
        generation: u64,
        meta: &spot_meta::EffectiveMeta,
        declared: &PolicySet,
    ) -> std::result::Result<(), String> {
        let loaded = self.lock().load(generation, meta, declared);
        match loaded {
            Ok(()) => {
                let _ = self.generation.send(generation);
                Ok(())
            }
            Err(_) => Err(self.readiness()),
        }
    }

    /// Whether a record of `doctype` may leave this node (`up`) or the hub.
    #[must_use]
    pub fn may_leave(&self, doctype: &str, up: bool) -> bool {
        self.lock().may_leave(doctype, up)
    }

    /// Whether an edge may author `doctype` (`master` is hub-authored).
    ///
    /// # Errors
    ///
    /// `sync_policy.master_edge_write`.
    pub fn edge_may_write(&self, doctype: &str) -> std::result::Result<(), &'static str> {
        self.lock().edge_may_write(doctype)
    }

    /// The serving generation, for the hub's providers.
    #[must_use]
    pub fn snapshot(&self) -> Option<Arc<Snapshot>> {
        self.lock().snapshot()
    }

    /// The readiness line.
    #[must_use]
    pub fn readiness(&self) -> String {
        self.lock().readiness()
    }

    /// Whether the last load was refused.
    #[must_use]
    pub fn refused(&self) -> bool {
        self.lock().refused().is_some()
    }

    /// A receiver of the serving generation number (0 until one loads): the hub rebuilds its
    /// destinations and re-reads its subscriptions when it changes.
    #[must_use]
    pub fn changes(&self) -> watch::Receiver<u64> {
        self.generation.subscribe()
    }

    /// The factory the hub relay builds its destinations with, over the serving generation and the
    /// configured scope fields. `None` until a generation has loaded.
    #[must_use]
    pub fn destination_factory(&self, reader: Arc<dyn FieldReader>) -> Option<DestinationFactory> {
        let snapshot = self.snapshot()?;
        Some(DestinationFactory::new(PolicyScopeKeys::new(
            snapshot,
            self.fields.clone(),
            reader,
        )))
    }
}

/// The operator's readiness line.
pub struct PolicyReadiness {
    handle: Arc<PolicyHandle>,
}

impl ReadinessCheck for PolicyReadiness {
    fn check(&self) -> Pin<Box<dyn Future<Output = CheckResult> + Send + '_>> {
        Box::pin(async move {
            CheckResult {
                // A refused generation degrades the line; the site keeps running on the last good
                // one (or on nothing syncing).
                ok: !self.handle.refused(),
                detail: self.handle.readiness(),
            }
        })
    }

    fn name(&self) -> &str {
        NAME
    }
}

/// The service: loads the first generation at start and every one after a metadata swap.
pub struct SyncPolicyService {
    handle: Arc<PolicyHandle>,
    declared: PolicySet,
    live: Option<Arc<SiteLive>>,
    first: Mutex<Option<Arc<spot_meta::EffectiveMeta>>>,
}

impl Service for SyncPolicyService {
    fn name(&self) -> &'static str {
        NAME
    }

    fn level(&self) -> Level {
        // Losing policy never stops the site: nothing leaves a node without a generation.
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            let meta = self
                .first
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
            if let Some(meta) = meta {
                // A refusal is on the readiness line; the service goes on running so the next
                // generation (a corrected metadata swap) can load.
                if let Err(line) = self.handle.load(1, &meta, &self.declared) {
                    tracing::warn!(line, "sync policy: the first generation was refused");
                }
            }
            Ok(())
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            let Some(live) = self.live.clone() else {
                let _ = stop.wait_for(|stopped| *stopped).await;
                return Ok(());
            };
            let mut swaps = live.swaps();
            let mut generation = 1_u64;
            loop {
                tokio::select! {
                    _ = stop.wait_for(|stopped| *stopped) => return Ok(()),
                    changed = swaps.changed() => {
                        if changed.is_err() {
                            return Ok(());
                        }
                        let Some(served) = live.served() else { continue };
                        generation += 1;
                        // A refusal keeps the previous generation and is on the readiness line.
                        if let Err(line) = self.handle.load(generation, &served.meta, &self.declared) {
                            tracing::warn!(line, "sync policy: a generation was refused");
                        }
                    }
                }
            }
        })
    }
}

/// Registers the module (every role).
///
/// # Errors
///
/// [`Error`] for settings that do not parse.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default(NAME)?;
    let defaults = settings.defaults()?;
    let declared = settings.declared()?;
    if !ctx.assembling() {
        return Ok(());
    }
    let env = *ctx.env()?;
    let handle = Arc::new(PolicyHandle::new(
        PolicyHost::with_defaults(defaults),
        settings.scope_fields(),
    ));
    ctx.provide::<PolicyHandle>(Arc::clone(&handle))?;
    ctx.provide::<PolicyReadiness>(Arc::new(PolicyReadiness {
        handle: Arc::clone(&handle),
    }))?;
    ctx.add(Arc::new(SyncPolicyService {
        handle,
        declared,
        live: Some(Arc::clone(env.live)),
        first: Mutex::new(Some(Arc::clone(&env.permission.meta))),
    }));
    Ok(())
}

/// The lines `spot sync policy show` prints for the `[services.sync_policy]` table `table`: names
/// only (DocTypes, classes, directions, key types, excluded field **names**), never a value.
///
/// # Errors
///
/// [`Error`] when the table does not parse or a declaration is invalid.
pub fn describe(table: Option<&toml::Value>) -> Result<Vec<String>> {
    let settings: Settings = match table {
        Some(table) => table
            .clone()
            .try_into()
            .map_err(|e| Error(format!("services.sync_policy: {e}")))?,
        None => Settings::default(),
    };
    let defaults = settings.defaults()?;
    let declared = settings.declared()?;
    let mut lines = vec![
        format!("default preset: {}", defaults.preset),
        format!(
            "default owner scope: {}",
            defaults
                .owner_scope
                .as_ref()
                .map_or("none", ScopeKeyType::as_str)
        ),
        format!("declared DocTypes: {}", declared.len()),
    ];
    for doctype in declared.doctypes() {
        let Some(policy) = declared.get(doctype) else {
            continue;
        };
        let excluded: Vec<&str> = policy.excluded().map(|f| f.as_str()).collect();
        lines.push(format!(
            "  {doctype}: {} (direction {}), preset {}, owner scope {}, excluded fields: {}",
            policy.class,
            policy.direction,
            spot_sync_policy_host::preset_of(policy.class),
            policy
                .owner_scope
                .as_ref()
                .map_or("none", ScopeKeyType::as_str),
            if excluded.is_empty() {
                "none".to_owned()
            } else {
                excluded.join(", ")
            }
        ));
    }
    for ((doctype, key), field) in settings.scope_fields() {
        lines.push(format!("  scope field: {doctype}.{key} <- {field}"));
    }
    Ok(lines)
}
