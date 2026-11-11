//! [`FunctionRegistryBuilder`]: collects what apps declare, validates it together and reports
//! every problem at once.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::Value as Json;
use spot_core::clock::{Clock, SystemClock};
use spot_manifest::{FunctionSub, HttpVerb};

use crate::args::FunctionHandler;
use crate::codes;
use crate::key::FunctionKey;
use crate::registry::FunctionRegistry;

/// Why a set of app manifests cannot be installed together.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Conflict {
    /// A stable machine code, for example `function.duplicate`.
    pub code: &'static str,
    /// A plain message naming the apps and functions involved.
    pub message: String,
    /// The apps involved, sorted and without repeats.
    pub apps: Vec<String>,
}

impl Conflict {
    fn new(code: &'static str, message: String, apps: &[&str]) -> Self {
        let mut apps: Vec<String> = apps.iter().map(|a| (*a).to_owned()).collect();
        apps.sort();
        apps.dedup();
        Self {
            code,
            message,
            apps,
        }
    }
}

impl std::fmt::Display for Conflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for Conflict {}

/// A function's per-`(function, caller)` rate limit: `permits` calls per `per` window,
/// refilled continuously.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RateLimit {
    /// How many calls the bucket starts with.
    pub permits: u32,
    /// The refill window.
    pub per: std::time::Duration,
}

/// One declared function, as the registry stores it: the manifest's declaration plus the
/// limit and argument schema the manifest does not carry.
#[derive(Clone, Debug)]
pub struct FunctionDecl {
    /// The function's `app/module/name` address.
    pub key: FunctionKey,
    /// The HTTP verbs it answers; `None` for a function no transport exposes over HTTP.
    pub verbs: Option<Vec<HttpVerb>>,
    /// Whether an unauthenticated guest may call it.
    pub guest: bool,
    /// The per-`(function, caller)` rate limit, when one is set.
    pub limit: Option<RateLimit>,
    /// The argument schema (`type`/`required`/`properties` only), when one is declared.
    pub arguments: Option<Json>,
}

/// One registry entry: the declaration, its declaring app and the handler that runs.
pub(crate) struct Entry {
    pub(crate) app: String,
    pub(crate) decl: FunctionDecl,
    pub(crate) handler: Arc<dyn FunctionHandler>,
    pub(crate) overridden_by: Option<(String, Arc<dyn FunctionHandler>, String)>,
}

/// What [`FunctionRegistry::list`] reports for one function.
///
/// [`FunctionRegistry::list`]: crate::FunctionRegistry::list
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionInfo {
    /// The function's address.
    pub key: FunctionKey,
    /// The app that declares the function.
    pub app: String,
    /// The verbs it answers, when it answers HTTP.
    pub verbs: Option<Vec<HttpVerb>>,
    /// Whether guests may call it.
    pub guest: bool,
    /// Its rate limit, when one is set.
    pub limit: Option<RateLimit>,
    /// Its argument schema, when one is declared.
    pub arguments: Option<Json>,
    /// The app that overrides it and why, when an override is installed.
    pub overridden_by: Option<(String, String)>,
}

struct Registered {
    app: String,
    decl: FunctionDecl,
    handler: Arc<dyn FunctionHandler>,
}

struct OverrideDecl {
    app: String,
    key: FunctionKey,
    handler: Arc<dyn FunctionHandler>,
    reason: String,
}

/// Collects what apps declare, then validates and builds it all at once.
///
/// Registration order never matters: [`build`](Self::build) sorts everything first, so any
/// order gives the same registry and the same conflicts.
#[derive(Default)]
pub struct FunctionRegistryBuilder {
    functions: Vec<Registered>,
    overrides: Vec<OverrideDecl>,
    clock: Option<Arc<dyn Clock>>,
}

impl FunctionRegistryBuilder {
    /// An empty builder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the clock the registry's rate limiter reads (tests swap in a manual clock).
    /// Defaults to the system clock.
    pub fn clock(&mut self, clock: Arc<dyn Clock>) -> &mut Self {
        self.clock = Some(clock);
        self
    }

    /// Registers `app`'s declared function `sub` (the manifest path): the manifest supplies
    /// module, name, HTTP verbs and the guest flag; the limit and argument schema are
    /// declared here because the manifest does not carry them.
    pub fn register(
        &mut self,
        app: &str,
        sub: FunctionSub,
        handler: Arc<dyn FunctionHandler>,
        limit: Option<RateLimit>,
        arguments: Option<Json>,
    ) -> &mut Self {
        let key = FunctionKey::from_parts(app, sub.module.as_str(), sub.name.as_str());
        self.register_decl(
            app,
            FunctionDecl {
                key,
                verbs: sub.http,
                guest: sub.guest,
                limit,
                arguments,
            },
            handler,
        )
    }

    /// Registers a host-built declaration under `app` with the handler that implements it.
    pub fn register_decl(
        &mut self,
        app: &str,
        decl: FunctionDecl,
        handler: Arc<dyn FunctionHandler>,
    ) -> &mut Self {
        self.functions.push(Registered {
            app: app.to_owned(),
            decl,
            handler,
        });
        self
    }

    /// Makes `app` replace whoever handles `key` with `handler`, on purpose, for `reason`
    /// (the admin override table, as with step overrides).
    pub fn override_function(
        &mut self,
        app: &str,
        key: FunctionKey,
        handler: Arc<dyn FunctionHandler>,
        reason: &str,
    ) -> &mut Self {
        self.overrides.push(OverrideDecl {
            app: app.to_owned(),
            key,
            handler,
            reason: reason.to_owned(),
        });
        self
    }

    /// Validates everything and builds the registry, or reports **all** conflicts, sorted.
    ///
    /// # Rules this crate fixes
    ///
    /// - An app may override another app's function, but never its own (that would be a
    ///   chain; the app should simply register the better handler) — `function.override_chain`.
    /// - Overrides do not stack: two overrides of one function are refused, whichever order
    ///   they arrive in — `function.override_duplicate`.
    /// - An override must name a function somebody declares and carry a non-empty reason —
    ///   `function.override_unknown`.
    pub fn build(&self) -> Result<FunctionRegistry, Vec<Conflict>> {
        let mut conflicts = Vec::new();
        let mut functions: Vec<&Registered> = self.functions.iter().collect();
        functions.sort_by(|a, b| (&a.decl.key, &a.app).cmp(&(&b.decl.key, &b.app)));
        let mut entries: BTreeMap<FunctionKey, Entry> = BTreeMap::new();
        for f in functions {
            let key = &f.decl.key;
            let http = f.decl.verbs.as_deref().unwrap_or(&[]);
            if f.decl.verbs.is_some() && http.is_empty() {
                conflicts.push(Conflict::new(
                    codes::VERB_EMPTY,
                    format!(
                        "app `{}` declares `{key}` over HTTP with an empty verb list",
                        f.app
                    ),
                    &[&f.app],
                ));
            }
            if f.decl.guest && f.decl.verbs.as_ref().is_none_or(Vec::is_empty) {
                conflicts.push(Conflict::new(
                    codes::GUEST_VERBS,
                    format!(
                        "app `{}` declares `{key}` guest-callable without an explicit verb list; a guest function must name its verbs",
                        f.app
                    ),
                    &[&f.app],
                ));
            }
            if let Some(prev) = entries.get(key).map(|e| e.app.clone()) {
                conflicts.push(Conflict::new(
                    codes::DUPLICATE,
                    format!(
                        "apps `{prev}` and `{}` both declare function `{key}`",
                        f.app
                    ),
                    &[&prev, &f.app],
                ));
                continue;
            }
            entries.insert(
                key.clone(),
                Entry {
                    app: f.app.clone(),
                    decl: f.decl.clone(),
                    handler: f.handler.clone(),
                    overridden_by: None,
                },
            );
        }

        let mut overrides: Vec<&OverrideDecl> = self.overrides.iter().collect();
        overrides.sort_by(|a, b| (&a.key, &a.app).cmp(&(&b.key, &b.app)));
        for o in overrides {
            let o_key = o.key.to_string();
            if o.reason.trim().is_empty() {
                conflicts.push(Conflict::new(
                    codes::OVERRIDE_UNKNOWN,
                    format!(
                        "the override of `{o_key}` by app `{}` carries no reason",
                        o.app
                    ),
                    &[&o.app],
                ));
                continue;
            }
            let Some(entry) = entries.get_mut(&o.key) else {
                conflicts.push(Conflict::new(
                    codes::OVERRIDE_UNKNOWN,
                    format!(
                        "app `{}` overrides `{o_key}`, which no installed app declares",
                        o.app
                    ),
                    &[&o.app],
                ));
                continue;
            };
            let mut placed = true;
            if entry.app == o.app {
                // An app never chains through the override table; it registers the better
                // handler.
                conflicts.push(Conflict::new(
                    codes::OVERRIDE_CHAIN,
                    format!(
                        "app `{}` overrides its own `{o_key}`; an app never chains through the override table, it registers the better handler",
                        o.app
                    ),
                    &[&o.app],
                ));
                placed = false;
            }
            if let Some(prev) = entry.overridden_by.as_ref().map(|(a, _, _)| a.clone()) {
                conflicts.push(Conflict::new(
                    codes::OVERRIDE_DUPLICATE,
                    format!(
                        "apps `{prev}` and `{}` both override `{o_key}`; overrides do not stack",
                        o.app
                    ),
                    &[&prev, &o.app],
                ));
                placed = false;
            }
            if placed {
                entry.overridden_by = Some((o.app.clone(), o.handler.clone(), o.reason.clone()));
            }
        }

        if conflicts.is_empty() {
            Ok(FunctionRegistry::new(
                entries,
                self.clock
                    .clone()
                    .unwrap_or_else(|| Arc::new(SystemClock::new())),
            ))
        } else {
            conflicts.sort();
            conflicts.dedup();
            Err(conflicts)
        }
    }
}
