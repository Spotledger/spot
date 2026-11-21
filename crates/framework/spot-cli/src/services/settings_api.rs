//! The settings management service (WP-ADM-07.T2). Operator guide:
//! `docs/running-spot/settings_api.md`.
//!
//! `spot-settings` (WP-ADM-07.T1) is the typed schema and the resolution; `spot-settings-store-pg`
//! keeps scoped values with compare-and-set versions and a history of who, when and the digests of
//! the old and new value. This module hosts them as management operations through the
//! [`MgmtHandlers`](crate::services::mgmt_api::MgmtHandlers) port:
//! `settings.list`, `settings.get`, `settings.set`, `settings.set_high_risk` and `settings.history`.
//!
//! * `set` validates against the setting's type and bounds (the shared check, through
//!   `ChangeSet::validate`), refuses an unknown key, a read-only setting and a scope the setting
//!   does not allow, and refuses a scope that is not this site's (`tenant:<site>` and the
//!   company and user scopes under it): a site's settings are written only for this site.
//! * A setting whose schema risk is `High` or `Critical` is refused by `settings.set` and accepted
//!   only by `settings.set_high_risk`, which the catalogue declares `Critical`, so the management
//!   API demands a signed envelope for it.
//! * A setting that needs a restart is applied atomically and the result carries `restart`:
//!   what must restart (`RestartRequired`); nothing is applied partially.
//! * A secret setting holds the **name of a secret-store entry**, never a value: `set` takes the
//!   name, `get` and `list` return the name only, and the history holds digests only.
//! * Every command is audited by the management API (channel and principal); the history row
//!   carries the principal and the channel `mgmt`.
//!
//! The service is registered only when the management API is (it needs its handlers port) and
//! only when `[services.settings_api] enabled = true`.

use std::sync::Arc;

use serde_json::{Value, json};
use spot_mgmt_api::handlers::{HandlerError, HandlerRequest, HandlerResult, handler_fn};
use spot_mgmt_ops::{OpId, OperationSpec, ParamSpec, ParamType, ParamValue, Risk, TargetKind};
use spot_settings::{
    Chain, Change, ChangeSet, EffectiveValue, MemoryStore, Origin, ScopeRef, SecretValue,
    SettingSpec, SettingsRegistry, StoredValue, resolve,
};
use spot_settings_store_pg::{PgSettingsStore, SetRequest, SettingsBackend, StoreError};

use crate::services::SiteCtx;
use crate::{Error, Result};

/// After the management API (`mgmt_api::ORDER` is 1000), whose handlers port this adds to.
pub const ORDER: u32 = 1010;

/// The module name the operations are catalogued under.
pub const MODULE: &str = "settings";

const SCOPE_FORBIDDEN: &str = "settings_api.scope_forbidden";
const SCOPE_INVALID: &str = "settings_api.scope_invalid";
const VALUE_INVALID: &str = "settings_api.value_invalid";
const VERSION_CONFLICT: &str = "settings_api.version_conflict";
const STORAGE_FAILED: &str = "settings_api.storage_failed";
const HIGH_RISK: &str = "settings_api.use_high_risk_operation";
const NOT_HIGH_RISK: &str = "settings_api.not_a_high_risk_setting";
const SECRET_REFERENCE_INVALID: &str = "settings_api.secret_reference_invalid";

/// The service: the schema, the store, this site and its principal-facing rules.
pub struct SettingsApi {
    registry: Arc<SettingsRegistry>,
    store: Arc<dyn SettingsBackend>,
    site: String,
    clock: Arc<dyn spot_core::Clock>,
}

fn text(name: &str, max_len: usize, required: bool, doc: &str, default: Option<&str>) -> ParamSpec {
    ParamSpec {
        name: name.to_owned(),
        ty: ParamType::Text {
            max_len,
            pattern: None,
        },
        required,
        default: default.map(|d| ParamValue::Text(d.to_owned())),
        doc: doc.to_owned(),
        secret: false,
    }
}

fn int(
    name: &str,
    min: i64,
    max: i64,
    required: bool,
    doc: &str,
    default: Option<i64>,
) -> ParamSpec {
    ParamSpec {
        name: name.to_owned(),
        ty: ParamType::Int { min, max },
        required,
        default: default.map(ParamValue::Int),
        doc: doc.to_owned(),
        secret: false,
    }
}

fn spec(
    id: &str,
    title: &str,
    summary: &str,
    params: Vec<ParamSpec>,
    risk: Risk,
) -> Option<OperationSpec> {
    Some(OperationSpec {
        id: OpId::new(id)?,
        title: title.to_owned(),
        summary: summary.to_owned(),
        params,
        risk,
        target: TargetKind::Tenant,
    })
}

/// The operations this module contributes (empty only if an id fails its own shape check).
#[must_use]
pub fn operations() -> Vec<OperationSpec> {
    let scope = || {
        text(
            "scope",
            200,
            false,
            "`fleet`, `group:<id>`, `server:<id>`, `tenant:<site>`, `company:<site>/<id>` or `user:<site>/<id>`; default: this site.",
            None,
        )
    };
    let key = || {
        text(
            "key",
            200,
            true,
            "The setting key, for example `runtime.http.listen_address`.",
            None,
        )
    };
    let value = || {
        text(
            "value",
            4000,
            true,
            "The new value in its type's text form; for a secret setting, the NAME of the secret-store entry.",
            None,
        )
    };
    let expected = || {
        int(
            "expected_version",
            0,
            i64::MAX,
            false,
            "The version you read (0 for none); a mismatch is refused.",
            None,
        )
    };
    [
        spec(
            "settings.list",
            "List settings",
            "The settings and their values at one scope.",
            vec![
                scope(),
                text("area", 100, false, "Only keys under this area.", None),
            ],
            Risk::Read,
        ),
        spec(
            "settings.get",
            "Read a setting",
            "One setting's effective value with where it came from.",
            vec![key(), scope()],
            Risk::Read,
        ),
        spec(
            "settings.set",
            "Change a setting",
            "Sets one value; refuses high-risk settings.",
            vec![key(), scope(), value(), expected()],
            Risk::Medium,
        ),
        spec(
            "settings.set_high_risk",
            "Change a high-risk setting",
            "Sets one high-risk value; needs a signed envelope.",
            vec![key(), scope(), value(), expected()],
            Risk::Critical,
        ),
        spec(
            "settings.history",
            "Setting history",
            "Who changed a setting and when, with digests only.",
            vec![
                key(),
                scope(),
                int("limit", 1, 100, false, "How many entries.", Some(20)),
            ],
            Risk::Read,
        ),
    ]
    .into_iter()
    .flatten()
    .collect()
}

fn fail(code: &'static str) -> HandlerError {
    HandlerError::failed(code)
}

fn param<'a>(request: &'a HandlerRequest, name: &str) -> Option<&'a ParamValue> {
    request
        .params
        .params
        .iter()
        .find(|p| p.name == name)
        .map(|p| &p.value)
}

fn param_text<'a>(request: &'a HandlerRequest, name: &str) -> Option<&'a str> {
    match param(request, name) {
        Some(ParamValue::Text(t)) => Some(t.as_str()),
        _ => None,
    }
}

fn store_error(error: &StoreError) -> HandlerError {
    if error.code == spot_settings_store_pg::VERSION_CONFLICT {
        fail(VERSION_CONFLICT)
    } else {
        fail(STORAGE_FAILED)
    }
}

/// Parses `scope` text; `None` is this site's tenant scope.
fn parse_scope(raw: Option<&str>, site: &str) -> std::result::Result<ScopeRef, HandlerError> {
    let Some(raw) = raw else {
        return Ok(ScopeRef::Tenant(site.to_owned()));
    };
    let (level, id) = raw.split_once(':').map_or((raw, ""), |(l, i)| (l, i));
    let id_ok = !id.is_empty() && id.len() <= 128 && !id.chars().any(char::is_control);
    match (level, id_ok) {
        ("fleet", false) if id.is_empty() => Ok(ScopeRef::Fleet),
        ("group", true) => Ok(ScopeRef::Group(id.to_owned())),
        ("server", true) => Ok(ScopeRef::Server(id.to_owned())),
        ("tenant", true) => Ok(ScopeRef::Tenant(id.to_owned())),
        ("company", true) => Ok(ScopeRef::Company(id.to_owned())),
        ("user", true) => Ok(ScopeRef::User(id.to_owned())),
        _ => Err(fail(SCOPE_INVALID)),
    }
}

/// Whether a write at `scope` is for this site: the tenant scope must be this site, and the
/// company and user scopes must sit under it. Fleet, group and server scopes belong to the
/// operator, who reaches them through the same signed management channel.
fn owned_by(scope: &ScopeRef, site: &str) -> bool {
    match scope {
        ScopeRef::Fleet | ScopeRef::Group(_) | ScopeRef::Server(_) => true,
        ScopeRef::Tenant(id) => id == site,
        ScopeRef::Company(id) | ScopeRef::User(id) => id
            .split_once('/')
            .is_some_and(|(tenant, rest)| tenant == site && !rest.is_empty()),
    }
}

/// Reads one text value into the setting's type.
fn parse_value(ty: &ParamType, raw: &str) -> Option<ParamValue> {
    match ty {
        ParamType::Bool => match raw {
            "true" => Some(ParamValue::Bool(true)),
            "false" => Some(ParamValue::Bool(false)),
            _ => None,
        },
        ParamType::Int { .. } => raw.parse::<i64>().ok().map(ParamValue::Int),
        ParamType::Text { .. } | ParamType::Enum(_) => Some(ParamValue::Text(raw.to_owned())),
        ParamType::Duration => {
            let digits = raw.strip_suffix('s').unwrap_or(raw);
            digits.parse::<u64>().ok().map(ParamValue::Duration)
        }
        ParamType::List(inner) => {
            let items: Option<Vec<ParamValue>> = raw
                .split(',')
                .map(|item| parse_value(inner, item.trim()))
                .collect();
            items.map(ParamValue::List)
        }
    }
}

fn shown(value: &ParamValue) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

/// A stored value as shown: a plain value, or the reference name of a secret.
fn stored_json(value: &StoredValue) -> Value {
    match value {
        StoredValue::Value(plain) => json!({ "value": shown(plain) }),
        StoredValue::Secret(reference) => json!({ "secret_reference": reference.expose() }),
    }
}

fn valid_reference(name: &str) -> bool {
    (1..=128).contains(&name.len())
        && name.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
        })
}

/// The chain a scope inherits through, broad to narrow.
fn chain_for(scope: &ScopeRef) -> Chain {
    match scope {
        ScopeRef::Fleet => Chain::for_context(true, None, None, None, None, None),
        ScopeRef::Group(g) => Chain::for_context(true, Some(g), None, None, None, None),
        ScopeRef::Server(s) => Chain::for_context(true, None, Some(s), None, None, None),
        ScopeRef::Tenant(t) => Chain::for_context(true, None, None, Some(t), None, None),
        ScopeRef::Company(c) => {
            let tenant = c.split_once('/').map(|(t, _)| t);
            Chain::for_context(true, None, None, tenant, Some(c), None)
        }
        ScopeRef::User(u) => {
            let tenant = u.split_once('/').map(|(t, _)| t);
            Chain::for_context(true, None, None, tenant, None, Some(u))
        }
    }
}

impl SettingsApi {
    /// A service over `registry` and `store` for `site`.
    #[must_use]
    pub fn new(
        registry: Arc<SettingsRegistry>,
        store: Arc<dyn SettingsBackend>,
        site: impl Into<String>,
        clock: Arc<dyn spot_core::Clock>,
    ) -> Self {
        Self {
            registry,
            store,
            site: site.into(),
            clock,
        }
    }

    fn spec_for(&self, key: &str) -> std::result::Result<&SettingSpec, HandlerError> {
        self.registry
            .get(key)
            .ok_or_else(|| fail("settings.unknown_setting"))
    }

    /// `settings.list`.
    ///
    /// # Errors
    /// A scope that does not parse, or a store failure.
    pub async fn list(&self, request: &HandlerRequest) -> HandlerResult {
        let scope = parse_scope(param_text(request, "scope"), &self.site)?;
        let area = param_text(request, "area");
        let stored = self.store.list(&scope).await.map_err(|e| store_error(&e))?;
        let mut rows = Vec::new();
        for spec in self.registry.specs() {
            if area.is_some_and(|a| !spec.key.starts_with(&format!("{a}."))) {
                continue;
            }
            if !spec.scopes.contains(&scope.scope()) {
                continue;
            }
            let mut row = json!({
                "key": spec.key,
                "title": spec.title,
                "risk": format!("{:?}", spec.risk).to_lowercase(),
                "restart_required": spec.restart.needed(),
                "read_only": spec.read_only,
                "secret": spec.secret,
            });
            if let Some((_, s)) = stored.iter().find(|(k, _)| *k == spec.key) {
                row["set"] = stored_json(&s.value);
                row["version"] = json!(s.version);
            }
            if let Some(default) = &spec.default {
                row["default"] = shown(default);
            }
            rows.push(row);
        }
        Ok(json!({ "scope": scope.canonical(), "settings": rows }))
    }

    /// `settings.get`: the effective value across the scope's inheritance chain.
    ///
    /// # Errors
    /// An unknown key, a scope that does not parse, or a store failure.
    pub async fn get(&self, request: &HandlerRequest) -> HandlerResult {
        let key = param_text(request, "key").ok_or_else(|| fail(VALUE_INVALID))?;
        let spec = self.spec_for(key)?;
        let scope = parse_scope(param_text(request, "scope"), &self.site)?;
        let chain = chain_for(&scope);
        let mut entries = Vec::new();
        let mut version = None;
        for s in chain.scopes() {
            if let Some(stored) = self.store.get(s, key).await.map_err(|e| store_error(&e))? {
                if *s == scope {
                    version = Some(stored.version);
                }
                entries.push((s.clone(), key.to_owned(), stored.value));
            }
        }
        let resolved = resolve(
            key,
            &chain,
            &MemoryStore::with_entries(entries.clone()),
            &self.registry,
        )
        .map_err(|_| fail("settings.unknown_setting"))?;
        let value = match &resolved.value {
            EffectiveValue::Value(v) => json!({ "value": shown(v) }),
            EffectiveValue::Redacted => entries
                .iter()
                .rev()
                .find_map(|(_, _, v)| match v {
                    StoredValue::Secret(r) => Some(json!({ "secret_reference": r.expose() })),
                    StoredValue::Value(_) => None,
                })
                .unwrap_or(Value::Null),
        };
        let origin = match &resolved.origin {
            Origin::Default => "default".to_owned(),
            Origin::Set(s) => s.canonical(),
        };
        Ok(json!({
            "key": spec.key,
            "scope": scope.canonical(),
            "effective": value,
            "origin": origin,
            "version": version.unwrap_or(0),
            "restart_required": spec.restart.needed(),
        }))
    }

    /// `settings.set` / `settings.set_high_risk`.
    ///
    /// # Errors
    /// An unknown key, a read-only setting, a scope the setting or this site does not allow, a
    /// value that fails the type, a high-risk setting on the plain operation, a lost version race
    /// or a store failure. Nothing is applied on any of them.
    pub async fn set(&self, request: &HandlerRequest, high_risk_op: bool) -> HandlerResult {
        let key = param_text(request, "key").ok_or_else(|| fail(VALUE_INVALID))?;
        let spec = self.spec_for(key)?;
        let high = matches!(spec.risk, Risk::High | Risk::Critical);
        if high && !high_risk_op {
            return Err(fail(HIGH_RISK));
        }
        if !high && high_risk_op {
            return Err(fail(NOT_HIGH_RISK));
        }
        let scope = parse_scope(param_text(request, "scope"), &self.site)?;
        if !owned_by(&scope, &self.site) {
            return Err(fail(SCOPE_FORBIDDEN));
        }
        let raw = param_text(request, "value").ok_or_else(|| fail(VALUE_INVALID))?;
        let change = if spec.secret {
            if !valid_reference(raw) {
                return Err(fail(SECRET_REFERENCE_INVALID));
            }
            Change::set_secret(scope.clone(), key, raw)
        } else {
            let value = parse_value(&spec.value, raw).ok_or_else(|| fail(VALUE_INVALID))?;
            Change::set(scope.clone(), key, value)
        };
        ChangeSet::new()
            .push_change(change.clone())
            .validate(&self.registry)
            .map_err(|report| {
                let code = report
                    .errors()
                    .first()
                    .map_or("settings.validation_failed", |e| e.code());
                fail(code)
            })?;
        let value = if spec.secret {
            StoredValue::Secret(SecretValue::new(raw))
        } else {
            StoredValue::Value(parse_value(&spec.value, raw).ok_or_else(|| fail(VALUE_INVALID))?)
        };
        let expected = match param(request, "expected_version") {
            Some(ParamValue::Int(n)) => u64::try_from(*n).ok(),
            _ => None,
        };
        let version = self
            .store
            .set(&SetRequest {
                scope: scope.clone(),
                key: key.to_owned(),
                value: Some(value),
                expected,
                actor: request.principal.clone(),
                channel: "mgmt".to_owned(),
                at: self.clock.now_utc().unix_timestamp(),
            })
            .await
            .map_err(|e| store_error(&e))?;
        let mut result = json!({
            "key": key,
            "scope": scope.canonical(),
            "version": version,
            "applied": true,
        });
        if let Some(restart) = spec.restart.describe() {
            result["restart_required"] = json!(restart);
        }
        Ok(result)
    }

    /// `settings.history`.
    ///
    /// # Errors
    /// An unknown key, a scope that does not parse, or a store failure.
    pub async fn history(&self, request: &HandlerRequest) -> HandlerResult {
        let key = param_text(request, "key").ok_or_else(|| fail(VALUE_INVALID))?;
        self.spec_for(key)?;
        let scope = parse_scope(param_text(request, "scope"), &self.site)?;
        let limit = match param(request, "limit") {
            Some(ParamValue::Int(n)) => usize::try_from(*n).unwrap_or(20),
            _ => 20,
        };
        let rows = self
            .store
            .history(&scope, key, limit)
            .await
            .map_err(|e| store_error(&e))?;
        let rows: Vec<Value> = rows
            .iter()
            .map(|r| {
                json!({
                    "version": r.version,
                    "actor": r.actor,
                    "channel": r.channel,
                    "at": r.at,
                    "old_digest": r.old_digest,
                    "new_digest": r.new_digest,
                })
            })
            .collect();
        Ok(json!({ "key": key, "scope": scope.canonical(), "history": rows }))
    }
}

/// Adds the five operations and their handlers to the management API's handlers port.
pub fn add_to(handlers: &crate::services::mgmt_api::MgmtHandlers, api: &Arc<SettingsApi>) {
    for spec in operations() {
        let id = spec.id.as_str().to_owned();
        let api = Arc::clone(api);
        let handler = handler_fn(move |_context, request: HandlerRequest| {
            let api = Arc::clone(&api);
            let id = id.clone();
            async move {
                match id.as_str() {
                    "settings.list" => api.list(&request).await,
                    "settings.get" => api.get(&request).await,
                    "settings.set" => api.set(&request, false).await,
                    "settings.set_high_risk" => api.set(&request, true).await,
                    _ => api.history(&request).await,
                }
            }
        });
        handlers.add(MODULE, spec, handler, false);
    }
}

/// `[services.settings_api]`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Settings {
    /// Off by default.
    enabled: bool,
}

fn refuse(message: impl std::fmt::Display) -> Error {
    Error(format!("services.settings_api: {message}"))
}

/// Registers the service: off by default; when enabled it needs the management API.
///
/// # Errors
/// A setting that does not decode, a management API that is not enabled, or a database that
/// cannot hold the tables.
pub async fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("settings_api")?;
    if !settings.enabled || !ctx.assembling() {
        return Ok(());
    }
    let Some(handlers) = ctx.port::<crate::services::mgmt_api::MgmtHandlers>() else {
        return Err(refuse("the management API must be enabled first"));
    };
    let env = *ctx.env()?;
    let store = PgSettingsStore::new(env.backend.pool().clone());
    store.prepare().await.map_err(|e| {
        refuse(format!(
            "the settings tables are not available ({})",
            e.code
        ))
    })?;
    let specs = spot_settings::load_seed_catalogue()
        .map_err(|_| refuse("the seed settings catalogue is invalid"))?;
    let registry = spot_settings::RegistryBuilder::new()
        .register_all(specs)
        .build()
        .map_err(|_| refuse("the settings registry is invalid"))?;
    let api = Arc::new(SettingsApi::new(
        Arc::new(registry),
        Arc::new(store),
        ctx.label().to_owned(),
        Arc::clone(ctx.clock()),
    ));
    add_to(&handlers, &api);
    Ok(())
}
