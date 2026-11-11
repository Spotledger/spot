//! Module-level whitelisted functions of one site: `POST|GET /api/v1/method/{app}/{module}/{function}`
//! (`function_call`, `function_call_get`; ADR-16, WP-PLT-17), over `spot-functions`.
//!
//! `spot-functions` owns the rules (the registry, the verb and guest checks, the token-bucket rate
//! limits, the argument schemas). This module supplies the rest:
//!
//! - **the registry**: built from the installed apps' `[[functions]]` declarations and the native
//!   handlers of [`crate::natives`] (`register_function`), once when the site is assembled and again
//!   on every reload, so an app change reaches the next call. Every conflict (two apps declaring
//!   one function, an override of nothing) is listed at once and stops the site start; a failed
//!   reload keeps the registry that was serving. A function an app declares but no handler
//!   implements is registered with a handler that answers `function.not_found` (it is on the list
//!   and is logged once); a handler nobody declares is never reachable;
//! - **who**: the site's sign-in service resolves the bearer credential. A request with no
//!   credential is a guest and reaches only functions declared `guest`, and only over a verb that
//!   does not change things; a credential that does not resolve, or a session that has not chosen
//!   its password yet, is refused, never downgraded to a guest. The function runs for that
//!   identity: it is handed the user id and nothing wider;
//! - **limits on by default**: a guest function with no limit of its own runs under the site's guest
//!   limit (`services.function.guest_permits` per `guest_window_seconds`); the buckets live in the
//!   registry, are idle-evicted by the `platform:function_evict_idle` job and, as a safety net with
//!   no timer of its own, whenever a call finds more than `bucket_cap` of them;
//! - **GET arguments** arrive as query strings; a declared schema decides how each value is read
//!   (`integer`, `number`, `boolean`), a value that does not parse stays a string and fails the
//!   schema by name.
//!
//! # Level
//!
//! Required, with nothing to degrade (a second provider of `FunctionDelegate` stops the site start,
//! as the host serves one delegate; a test that substitutes its own sets `services.function.enabled
//! = false`): the registry is in memory and is checked while it is built. A site whose apps declare conflicting functions does not start. `services.function.enabled =
//! false` leaves the engine's answer as it was (`404 method.not_found`). The settings are
//! `services.function`.
//!
//! # Audit
//!
//! Target `spot::audit::function`: a line for every call that reaches the registry (who, the
//! function address, the verb, the outcome code). Never an argument, a result, a token or a header.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, RwLock};
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value as Json;
use spot_api_services::FunctionDelegate;
use spot_api_services::resources::function::function_error;
use spot_app_api::AppError;
use spot_app_api::error::ErrorClass;
use spot_functions::{
    Caller, Conflict, Context, FunctionHandler, FunctionInfo, FunctionKey, FunctionRegistry,
    FunctionRegistryBuilder, HandlerFuture, HttpVerb, RateLimit,
};
use spot_http::api::registry::Code;
use spot_http::api::service::{ApiError, BoxFuture, Message, RequestCtx};
use spot_jobs::worker::HandlerResult;

use crate::audit::PathClass;
use crate::live::SiteLive;
use crate::services::{Idle, Level, SiteCtx};
use crate::{Error, Result, appdir, natives};

use super::controllers;

/// Start order: after the framework's own services (they use 100 to 900).
pub const ORDER: u32 = 1020;

/// The audit target of this service.
const AUDIT_TARGET: &str = "spot::audit::function";

/// The job that drops idle rate-limit buckets. A schedule that enqueues it is the operator's (or
/// the platform app's); the registry also bounds itself, see `bucket_cap`.
pub const EVICT_JOB: &str = "platform:function_evict_idle";

fn default_enabled() -> bool {
    true
}
fn default_guest_permits() -> u32 {
    30
}
fn default_guest_window() -> u64 {
    60
}
fn default_idle_after() -> u64 {
    600
}
fn default_bucket_cap() -> usize {
    10_000
}

/// `services.function`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// Serve function calls. Default true.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Calls a guest function without a limit of its own allows per window. 1 to 100,000.
    #[serde(default = "default_guest_permits")]
    pub guest_permits: u32,
    /// The guest window in seconds. 1 to 86,400.
    #[serde(default = "default_guest_window")]
    pub guest_window_seconds: u64,
    /// A bucket unused for this long is dropped by an eviction. 1 to 86,400.
    #[serde(default = "default_idle_after")]
    pub idle_after_seconds: u64,
    /// More buckets than this make the next call evict idle ones. 16 to 10,000,000.
    #[serde(default = "default_bucket_cap")]
    pub bucket_cap: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            enabled: true,
            guest_permits: default_guest_permits(),
            guest_window_seconds: default_guest_window(),
            idle_after_seconds: default_idle_after(),
            bucket_cap: default_bucket_cap(),
        }
    }
}

impl Settings {
    /// The first setting that is out of range.
    pub fn validate(&self) -> std::result::Result<(), &'static str> {
        if !(1..=100_000).contains(&self.guest_permits) {
            return Err("guest_permits");
        }
        if !(1..=86_400).contains(&self.guest_window_seconds) {
            return Err("guest_window_seconds");
        }
        if !(1..=86_400).contains(&self.idle_after_seconds) {
            return Err("idle_after_seconds");
        }
        if !(16..=10_000_000).contains(&self.bucket_cap) {
            return Err("bucket_cap");
        }
        Ok(())
    }
}

/// A function an app declares and no handler implements: answers like an unknown function.
struct Unimplemented(FunctionKey);

impl FunctionHandler for Unimplemented {
    fn call<'a>(&'a self, _ctx: &'a Context, _args: Json) -> HandlerFuture<'a> {
        let key = self.0.to_string();
        Box::pin(async move {
            Err(AppError::new(
                ErrorClass::MethodNotFound,
                spot_functions::codes::NOT_FOUND,
                format!("no function `{key}`"),
            ))
        })
    }
}

/// One built registry and the argument schemas GET decoding reads.
struct Built {
    registry: FunctionRegistry,
    schemas: BTreeMap<FunctionKey, Json>,
}

/// Builds the registry of one site from its installed apps and the native handlers.
///
/// # Errors
///
/// An unreadable manifest, or every conflict the registry finds, at once.
fn build_registry(
    ctx: &SiteCtx<'_>,
    settings: &Settings,
    handlers: &[natives::NativeFunction],
) -> Result<Built> {
    let env = *ctx.env()?;
    let mut builder = FunctionRegistryBuilder::new();
    builder.clock(Arc::clone(ctx.clock()));
    let guest = RateLimit {
        permits: settings.guest_permits,
        per: Duration::from_secs(settings.guest_window_seconds),
    };
    let by_key: HashMap<(&str, &str, &str), &natives::NativeFunction> = handlers
        .iter()
        .map(|h| ((h.app.as_str(), h.module.as_str(), h.name.as_str()), h))
        .collect();
    let mut declared = BTreeSet::new();
    let mut missing = Vec::new();
    let mut schemas = BTreeMap::new();
    let mut errors = Vec::new();
    for source in controllers::installed(env.root, env.loaded, env.site)? {
        let manifest = match appdir::manifest(&source.dir) {
            Ok(manifest) => manifest,
            Err(e) => {
                errors.push(e.to_string());
                continue;
            }
        };
        let app = manifest.app.name.as_str().to_owned();
        for sub in &manifest.functions {
            let key = FunctionKey::from_parts(&app, sub.module.as_str(), sub.name.as_str());
            declared.insert(key.to_string());
            let native = by_key.get(&(app.as_str(), sub.module.as_str(), sub.name.as_str()));
            let (handler, spec) = match native {
                Some(native) => (Arc::clone(&native.handler), native.spec.clone()),
                None => {
                    missing.push(key.to_string());
                    (
                        Arc::new(Unimplemented(key.clone())) as Arc<dyn FunctionHandler>,
                        natives::FunctionSpec::default(),
                    )
                }
            };
            let limit = spec
                .limit
                .map(|(permits, per)| RateLimit { permits, per })
                .or_else(|| sub.guest.then_some(guest));
            if let Some(schema) = &spec.arguments {
                schemas.insert(key, schema.clone());
            }
            builder.register(&app, sub.clone(), handler, limit, spec.arguments);
        }
    }
    if !errors.is_empty() {
        return Err(Error(errors.join("\n")));
    }
    if !missing.is_empty() {
        tracing::warn!(
            site = %env.site.site,
            functions = ?missing,
            "declared functions have no native handler and answer function.not_found"
        );
    }
    let unreachable: Vec<String> = handlers
        .iter()
        .map(|h| format!("{}/{}/{}", h.app, h.module, h.name))
        .filter(|key| !declared.contains(key))
        .collect();
    if !unreachable.is_empty() && ctx.assembling() {
        tracing::debug!(
            site = %env.site.site,
            functions = ?unreachable,
            "native functions no installed app declares are not callable"
        );
    }
    let registry = builder.build().map_err(|conflicts| {
        Error(format!(
            "site `{}`: functions: {}",
            env.site.site,
            list_conflicts(&conflicts)
        ))
    })?;
    Ok(Built { registry, schemas })
}

fn list_conflicts(conflicts: &[Conflict]) -> String {
    conflicts
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

/// The function service of one site: the current registry and what it needs to serve a call.
pub struct FunctionService {
    site: String,
    live: Arc<SiteLive>,
    settings: Settings,
    built: RwLock<Arc<Built>>,
}

impl FunctionService {
    fn current(&self) -> Arc<Built> {
        Arc::clone(&self.built.read().unwrap_or_else(|p| p.into_inner()))
    }

    fn swap(&self, built: Built) {
        *self.built.write().unwrap_or_else(|p| p.into_inner()) = Arc::new(built);
    }

    /// Every function of the site: declaring app, verbs, guest flag, limit, schema and override.
    /// The listing the admin app's `list` serves.
    pub fn list(&self) -> Vec<FunctionInfo> {
        self.current().registry.list()
    }

    /// Drops the rate-limit buckets idle for longer than `services.function.idle_after_seconds`;
    /// returns how many.
    pub fn evict_idle(&self) -> usize {
        self.current()
            .registry
            .evict_idle_buckets(Duration::from_secs(self.settings.idle_after_seconds))
    }

    async fn caller(&self, ctx: &RequestCtx) -> std::result::Result<Caller, ApiError> {
        if ctx.credential.is_none() {
            return Ok(Caller::Anonymous);
        }
        let auth = self.live.auth().ok_or_else(unavailable)?;
        let who = auth.authenticate_on(ctx, PathClass::Api).await?;
        if auth.is_restricted(&who) {
            return Err(ApiError::from_code(Code::PermissionDenied));
        }
        Ok(Caller::User(who.user_id.to_string()))
    }

    async fn run(
        &self,
        ctx: &RequestCtx,
        key: FunctionKey,
        args: Json,
        via_get: bool,
    ) -> std::result::Result<(Json, Vec<Message>), ApiError> {
        if ctx.site.as_str() != self.site {
            return Err(ApiError::from_code(Code::PermissionDenied));
        }
        let caller = self.caller(ctx).await?;
        let built = self.current();
        if built.registry.bucket_count() > self.settings.bucket_cap {
            built
                .registry
                .evict_idle_buckets(Duration::from_secs(self.settings.idle_after_seconds));
        }
        let verb = if via_get { HttpVerb::Get } else { HttpVerb::Post };
        let args = match (via_get, built.schemas.get(&key)) {
            (true, Some(schema)) => decode_query(schema, args),
            _ => args,
        };
        let context = Context {
            caller: caller.clone(),
            verb: Some(verb),
            tenant: self.site.clone(),
            command_id: ctx.idempotency_key.clone(),
        };
        let outcome = built.registry.dispatch(&context, &key, args).await;
        audit(&key, &caller, verb, &outcome);
        match outcome {
            Ok(value) => Ok((value, Vec::new())),
            Err(error) => Err(function_error(error)),
        }
    }
}

fn unavailable() -> ApiError {
    ApiError::from_code(Code::ServerUnavailable).detail("retry_after", Json::from(5))
}

fn audit(
    key: &FunctionKey,
    caller: &Caller,
    verb: HttpVerb,
    outcome: &std::result::Result<Json, AppError>,
) {
    let code = match outcome {
        Ok(_) => "ok",
        Err(error) => error.code.as_str(),
    };
    tracing::info!(
        target: AUDIT_TARGET,
        function = %key,
        caller = %caller.bucket(),
        verb = spot_functions::verb_name(verb),
        outcome = code,
        "function call"
    );
}

/// Reads each query-string value of a GET the way the function's schema declares it.
fn decode_query(schema: &Json, args: Json) -> Json {
    let Json::Object(mut map) = args else {
        return args;
    };
    let Some(properties) = schema.get("properties").and_then(Json::as_object) else {
        return Json::Object(map);
    };
    for (name, sub) in properties {
        let Some(Json::String(text)) = map.get(name) else {
            continue;
        };
        let decoded = match sub.get("type").and_then(Json::as_str) {
            Some("integer" | "number") => text
                .parse::<i64>()
                .map(Json::from)
                .or_else(|_| text.parse::<u64>().map(Json::from))
                .ok(),
            Some("boolean") => match text.as_str() {
                "true" => Some(Json::Bool(true)),
                "false" => Some(Json::Bool(false)),
                _ => None,
            },
            _ => None,
        };
        if let Some(value) = decoded {
            map.insert(name.clone(), value);
        }
    }
    Json::Object(map)
}

impl FunctionDelegate for FunctionService {
    fn call<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        app: &'a str,
        module: &'a str,
        function: &'a str,
        args: Json,
        via_get: bool,
    ) -> BoxFuture<'a, std::result::Result<(Json, Vec<Message>), ApiError>> {
        Box::pin(async move {
            // A path segment that is not a storage-style name cannot name a function: the same
            // answer as an unknown one.
            let key = FunctionKey::parse(app, module, function).map_err(|_| {
                function_error(AppError::new(
                    ErrorClass::MethodNotFound,
                    spot_functions::codes::NOT_FOUND,
                    "no such function",
                ))
            })?;
            self.run(ctx, key, args, via_get).await
        })
    }
}

/// The services of the sites built so far, by site name: a reload builds the site again without
/// building the service again, and swaps the registry inside the same service.
fn built() -> MutexGuard<'static, HashMap<String, Arc<FunctionService>>> {
    static BUILT: OnceLock<Mutex<HashMap<String, Arc<FunctionService>>>> = OnceLock::new();
    BUILT
        .get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

/// Registers the function service: the registry, its start-up check, its eviction job and the
/// delegate `call_function` reaches.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("function")?;
    settings
        .validate()
        .map_err(|key| Error(format!("services.function.{key}: out of range")))?;
    if !settings.enabled {
        return Ok(());
    }
    let site = ctx.env()?.site.site.clone();
    let handlers = natives::function_handlers();
    // Every build reads the manifests again: a conflict stops the start, and on a reload it
    // fails the reload (the previous registry keeps serving).
    let fresh = build_registry(ctx, &settings, &handlers)?;
    let service = if ctx.assembling() {
        let live = Arc::clone(ctx.env()?.live);
        let service = Arc::new(FunctionService {
            site: site.clone(),
            live,
            settings,
            built: RwLock::new(Arc::new(fresh)),
        });
        built().insert(site, Arc::clone(&service));
        ctx.add(Idle::new("function", Level::Required, || {
            Box::pin(async { Ok(()) })
        }));
        service
    } else {
        let Some(service) = built().get(&site).cloned() else {
            return Ok(());
        };
        service.swap(fresh);
        service
    };
    let evictor = Arc::clone(&service);
    ctx.job_handlers().register(EVICT_JOB, move |_job| {
        let evictor = Arc::clone(&evictor);
        Box::pin(async move {
            let dropped = evictor.evict_idle();
            tracing::debug!(dropped, "idle function rate-limit buckets dropped");
            Ok(HandlerResult::Ok)
        })
    });
    let delegate: Arc<dyn FunctionDelegate> = service;
    ctx.provide(delegate)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_query_value_is_read_as_its_schema_declares() {
        let schema = json!({"type": "object", "properties": {
            "n": {"type": "integer"}, "ok": {"type": "boolean"}, "s": {"type": "string"},
            "bad": {"type": "integer"}}});
        let args = json!({"n": "42", "ok": "true", "s": "7", "bad": "4x", "extra": "1"});
        assert_eq!(
            decode_query(&schema, args),
            json!({"n": 42, "ok": true, "s": "7", "bad": "4x", "extra": "1"})
        );
    }

    #[test]
    fn the_settings_refuse_out_of_range_values_and_unknown_keys() {
        assert!(Settings::default().validate().is_ok());
        let zero = Settings {
            guest_permits: 0,
            ..Settings::default()
        };
        assert_eq!(zero.validate(), Err("guest_permits"));
        assert!(serde_json::from_value::<Settings>(json!({"nope": 1})).is_err());
    }
}
