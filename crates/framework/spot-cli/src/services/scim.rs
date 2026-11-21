//! SCIM 2.0 hosting: `/scim/v2` Users and Groups over `spot-iam-scim` (WP-IAM-06.T2; ADR-070
//! sections 7.2 and 7.3). Operator guide: `docs/running-spot/scim.md`.
//!
//! # Shape
//!
//! The core (`spot-iam-scim`) is sans-I/O and its store is synchronous. A request therefore runs
//! as one **unit of work** in **one `Serializable` transaction** on the site's storage, as the
//! system principal (the same direct-to-storage path the operator commands use): load the
//! tenant's directory into a `Unit` (users, groups, the group-to-role table, live assignments,
//! the roles that exist), run [`handle_guarded`] on it, then write only what changed (users, group
//! documents, assignments, the revoked keys of a deactivated user) and the identity events to the
//! outbox, and commit. A refused request, or any failure, rolls back: nothing is half written. A
//! deactivation therefore ends the user's access in the same transaction as the user change: every
//! sign-in path re-reads the user's state on each request (the host runs with a cache TTL of
//! zero), so the session, the key and the invitation stop at commit, and the user's own API keys
//! are marked revoked as well.
//!
//! # Credential
//!
//! The bearer is an API key (`spk_<id>_<secret>`) stored as an `api_key` record whose scope list
//! is **exactly** `scim`, owned by a `service` user, with the digest sealed by the
//! vault. It is never a session. A key of any other scope, a session token, a disabled owner, an
//! expired or revoked key and an unknown key are one answer, `401`. [`ScimKeyIssuer::issue`] makes
//! one (the key text is returned once and never stored). Without a vault every call fails closed.
//!
//! # The field guard (WP-SEC-04.T10 and T16)
//!
//! SCIM is a data channel (`Channel::ApiScim`): every user attribute an answer carries, and every
//! attribute a `filter` or `sortBy` names, is judged by the site's field guard as **the
//! credential owner's principal** (the `service` user that owns the key), over the protection
//! facts of the metadata served now. [`handle_guarded`] is the only way this module runs the
//! core: no request is answered, and no write answer is returned, without it. A site that cannot
//! build the guard (a diverged node, no runtime attached) answers `503`, never an unguarded
//! resource. SCIM **groups** have no DocType (a name and a member list are SCIM-only records),
//! so group resources pass through the guard unchanged; a group DocType would be needed to
//! protect them and none is invented here.
//!
//! # Scope of the directory SCIM sees
//!
//! Only users of type `internal` exist for SCIM: `service`, `agent` and `portal` users (and so the
//! credential's own owner) are not listed, read, changed or deactivated by it.
//!
//! # Config (`[services.scim]`, unknown keys refused)
//!
//! `enabled` (default false: nothing is mounted), `max_body_bytes`, `requests_per_window` (per
//! credential, per minute), `max_count`, `default_count`, `max_filter_chars`.
//!
//! # Level
//!
//! Optional. Without `enabled = true` the service is absent and `/scim/v2` is a 404. Without a
//! secrets vault it is degraded and answers `503 iam_scim.vault_missing`.
//!
//! # Audit
//!
//! Every change writes the `identity_*` events the contract names to the outbox in the same
//! transaction, with the credential's owner as actor, and one line to `spot::audit::scim`: the
//! credential's key id, the operation and the status. Never a token, a body or a secret.

/// The catalogue code of a site without a secrets vault (HTTP 503).
const CODE_VAULT_MISSING: &str = "iam_scim.vault_missing";

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde_json::Value as Json;
use spot_api_services::resources::scim::{EngineScim, ScimOps};
use spot_auth::{ApiKey, TokenDigest};
use spot_core::Clock;
use spot_document::engine::Engine;
use spot_document::engine::PrincipalSource;
use spot_document::secrets::SecretCaller;
use spot_http::api::Ports;
use spot_http::api::ports::scim::{ScimCall, ScimPort, ScimReply};
use spot_http::api::router::routes;
use spot_http::api::service::{BoxFuture, RequestCtx};
use spot_iam_access::{AllowRule, KeyAction, KeyScope};
use spot_iam_directory::UserType;
use spot_iam_scim::{
    Credential as ScimCredential, Limits, Method, ScimContext, ScimError, ScimGuard, ScimRequest,
    handle_guarded, parse_record_id,
};
use spot_platform_app::doctype;
use spot_postgres::PgBackend;
use spot_secrets::SecretString;
use spot_storage_contract::{
    Catalog, ChildRow, DocumentStore as _, Isolation, RecordId, RowLock, StorageBackend,
    Transaction, TxOptions, Value,
};
use time::OffsetDateTime;

use crate::Result;
use crate::live::SiteLive;
use crate::services::scim_store::{
    Rep, at, blank, deny, doc, dt, find_id, hex, idv, keys_of, load, nm, persist, put, reply,
    storage, text, unavailable, unhex,
};
use crate::services::secrets::SecretsHost;
use crate::services::{Idle, Level, SiteCtx};

/// Start order: after the secrets module and the framework's own services.
pub const ORDER: u32 = 1040;

/// The only scope a SCIM credential may carry.
const SCOPE: &str = "scim";

/// What a request may do to storage on a SCIM credential's behalf, as the key scope the core
/// reads (the core only needs it non-empty; the real check is the `scim` scope row).
const KEY_ACTIONS: [KeyAction; 4] = [
    KeyAction::Read,
    KeyAction::Create,
    KeyAction::Update,
    KeyAction::Delete,
];

/// A usable SCIM key record: its id, its owner, the owner's type and the key's expiry.
type KeyRead = (RecordId, RecordId, UserType, Option<OffsetDateTime>);

/// `[services.scim]`.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
struct ScimSettings {
    enabled: bool,
    max_body_bytes: Option<usize>,
    requests_per_window: Option<u32>,
    max_count: Option<usize>,
    default_count: Option<usize>,
    max_filter_chars: Option<usize>,
}

impl ScimSettings {
    fn limits(&self) -> Limits {
        let base = Limits::default();
        Limits {
            max_body_bytes: self.max_body_bytes.unwrap_or(base.max_body_bytes),
            max_filter_chars: self.max_filter_chars.unwrap_or(base.max_filter_chars),
            default_count: self.default_count.unwrap_or(base.default_count),
            max_count: self.max_count.unwrap_or(base.max_count),
            requests_per_window: self.requests_per_window.unwrap_or(base.requests_per_window),
        }
    }
}

/// Makes SCIM credentials. A port so an administration command or API (a later task) can call it
/// without holding the service.
pub trait ScimKeyIssuer: Send + Sync {
    /// Creates a key owned by the `service` user `owner`, scope exactly `scim`, and returns its
    /// text **once**: only the digest is stored (sealed).
    fn issue<'a>(
        &'a self,
        owner: RecordId,
        label: &'a str,
        expires_at: Option<OffsetDateTime>,
    ) -> BoxFuture<'a, std::result::Result<String, String>>;
}

/// The SCIM service of one site.
pub struct ScimService {
    site: String,
    live: Arc<SiteLive>,
    clock: Arc<dyn Clock>,
    catalog: Arc<Catalog>,
    secrets: Option<Arc<SecretsHost>>,
    limits: Limits,
    gate: tokio::sync::Mutex<()>,
    windows: Mutex<BTreeMap<String, (i64, u32)>>,
    /// The site's sign-in throttle (WP-IAM-11.T24). `register` always sets it; a service built
    /// without one (a test that calls [`ScimService::new`]) checks credentials unthrottled.
    throttle: Option<Arc<crate::services::signin_throttle::SigninThrottle>>,
}

impl ScimService {
    /// The service of `site`. `secrets` is the vault's host, `None` when the site has none (every
    /// call then fails closed).
    pub fn new(
        site: impl Into<String>,
        live: Arc<SiteLive>,
        clock: Arc<dyn Clock>,
        catalog: Arc<Catalog>,
        secrets: Option<Arc<SecretsHost>>,
        limits: Limits,
    ) -> Self {
        Self {
            site: site.into(),
            live,
            clock,
            catalog,
            secrets,
            limits,
            gate: tokio::sync::Mutex::new(()),
            windows: Mutex::new(BTreeMap::new()),
            throttle: None,
        }
    }

    /// Puts every credential check behind the site's sign-in throttle (the SCIM door).
    #[must_use]
    pub fn with_throttle(
        mut self,
        throttle: Arc<crate::services::signin_throttle::SigninThrottle>,
    ) -> Self {
        self.throttle = Some(throttle);
        self
    }

    /// Admits one credential check through the throttle's SCIM door, before the key is looked up
    /// or its digest compared. The bucket key is `scim:` and the key's public id (or `scim:`
    /// alone when the token does not parse), never the token. A refusal is the throttle's
    /// answer as a SCIM error (`429` with `Retry-After`, or `503` when the throttle cannot
    /// count), never a `401`.
    async fn admit(&self, ctx: &RequestCtx, key_id: Option<&str>) -> Rep<()> {
        use crate::services::signin_throttle::Door;
        let Some(throttle) = &self.throttle else {
            return Ok(());
        };
        let account = format!("scim:{}", key_id.unwrap_or_default());
        let peer = ctx.peer.map(|ip| ip.to_string());
        let verdict = throttle
            .admit(
                Door::ScimKey,
                &self.site,
                &account,
                peer.as_deref(),
                Some(ctx.request_id.as_str()),
            )
            .await;
        let Err(refusal) = verdict else {
            return Ok(());
        };
        let retry = refusal
            .details
            .get("retry_after")
            .and_then(serde_json::Value::as_u64)
            .map(|seconds| seconds.max(1));
        let limited = matches!(
            refusal.code,
            spot_http::api::service::ErrorCode::Registry(
                spot_http::api::registry::Code::ApiRateLimited
            )
        );
        let mut out = if limited {
            reply(&ScimError::rate_limited(
                "too many attempts for this credential",
            ))
        } else {
            ScimReply::error(503, None, "SCIM credentials cannot be checked right now.")
        };
        out.retry_after = retry.or(Some(5));
        Err(out)
    }

    fn tx_options(&self, label: &str, read_only: bool, isolation: Isolation) -> Rep<TxOptions> {
        let options = TxOptions::new(nm(label)?, isolation);
        Ok(if read_only {
            options.read_only()
        } else {
            options
        })
    }

    fn engine(&self) -> Rep<Arc<Engine<PgBackend>>> {
        self.live.engine().ok_or_else(unavailable)
    }
}

// ---------------------------------------------------------------------------------------------
// Credential and serving
// ---------------------------------------------------------------------------------------------

impl ScimService {
    fn system_invocation(&self) -> Option<spot_app_api::context::Invocation> {
        use spot_app_api::context::{Actor, Invocation};
        Some(Invocation {
            site: spot_core::SiteId::new(&self.site).ok()?,
            actor: Actor::System {
                component: spot_app_api::prelude::Name::new("scim").ok()?,
            },
            command_id: None,
            language: None,
            now: self.clock.now_utc(),
        })
    }

    /// The SCIM credential the request carries, or `401`: unknown, revoked, expired, not scoped
    /// exactly `scim`, owner not a live service user, wrong digest and wrong site are one answer.
    async fn credential(&self, ctx: &RequestCtx) -> Rep<ScimCredential> {
        if ctx.site.as_str() != self.site {
            return Err(deny());
        }
        let token = ctx
            .credential
            .as_ref()
            .map(|c| c.token.as_str())
            .ok_or_else(deny)?;
        let key = match ApiKey::parse(token) {
            Ok(key) => key,
            Err(_) => {
                self.admit(ctx, None).await?;
                return Err(deny());
            }
        };
        self.admit(ctx, Some(key.key_id())).await?;
        if self.secrets.is_none() {
            tracing::warn!(
                code = CODE_VAULT_MISSING,
                "SCIM is degraded: the site has no secrets vault"
            );
            return Err(ScimReply::error(
                503,
                None,
                "SCIM credentials cannot be verified on this site.",
            ));
        }
        let engine = self.engine()?;
        let mut tx = engine
            .backend()
            .begin(
                Arc::clone(&self.catalog),
                self.tx_options("scim_credential", true, Isolation::ReadCommitted)?,
            )
            .await
            .map_err(|e| storage(&e))?;
        let read = self.read_key(&mut tx, key.key_id()).await;
        let _ = tx.rollback().await;
        let (id, owner, owner_type, expires_at) = read?.ok_or_else(deny)?;
        let inv = self.system_invocation().ok_or_else(unavailable)?;
        let access = engine.secret_access(SecretCaller::LoginService, &inv);
        let opened = engine
            .open_secret(&access, doctype::API_KEY, id, "digest")
            .await
            .map_err(|_| unavailable())?;
        let stored = opened
            .and_then(|s| unhex(s.expose()))
            .and_then(|b| TokenDigest::from_bytes(&b).ok());
        if !stored.is_some_and(|d| d.matches(&key.digest())) {
            return Err(deny());
        }
        Ok(ScimCredential {
            key: id,
            owner,
            owner_type,
            owner_enabled: true,
            tenant: self.site.clone(),
            scope: KeyScope {
                allow: vec![AllowRule::new("scim", KEY_ACTIONS)],
                scope: vec![],
                expires_at: None,
            },
            expires_at,
        })
    }

    /// The live SCIM key record `key_id`: `(id, owner, owner type, expiry)`, `None` for anything
    /// that makes it unusable.
    async fn read_key<T: Transaction>(&self, tx: &mut T, key_id: &str) -> Rep<Option<KeyRead>> {
        let Some(id) = find_id(tx, doctype::API_KEY, "key_id", key_id).await? else {
            return Ok(None);
        };
        let Some(key) = tx
            .load(&nm(doctype::API_KEY)?, id, RowLock::None)
            .await
            .map_err(|e| storage(&e))?
        else {
            return Ok(None);
        };
        let f = |k: &str| {
            nm(k)
                .ok()
                .and_then(|n| key.data.fields.get(&n).cloned())
                .unwrap_or(Value::Null)
        };
        let scopes: Vec<String> = key
            .data
            .children
            .get(&nm("scopes")?)
            .map(|rs| {
                rs.iter()
                    .filter_map(|r| r.fields.get(&nm("scope").ok()?).and_then(text))
                    .collect()
            })
            .unwrap_or_default();
        let expires_at = at(&f("expires_at"));
        let live = text(&f("state")).as_deref() == Some("active")
            && scopes == [SCOPE]
            && expires_at.is_none_or(|t| t > self.clock.now_utc());
        let Some(owner) = idv(&f("owner")).filter(|_| live) else {
            return Ok(None);
        };
        let Some(user) = tx
            .load(&nm(doctype::USER)?, owner, RowLock::None)
            .await
            .map_err(|e| storage(&e))?
        else {
            return Ok(None);
        };
        let uf = |k: &str| {
            nm(k)
                .ok()
                .and_then(|n| user.data.fields.get(&n).and_then(text))
        };
        let owner_type = match uf("user_type").as_deref() {
            Some("service") => UserType::Service,
            _ => return Ok(None),
        };
        Ok((uf("state").as_deref() == Some("active"))
            .then_some((id, owner, owner_type, expires_at)))
    }

    /// Counts one request against the credential's one-minute window.
    fn rate(&self, key: RecordId, now: OffsetDateTime) -> std::result::Result<(), u64> {
        let mut windows = self.windows.lock().unwrap_or_else(|p| p.into_inner());
        let secs = now.unix_timestamp();
        let entry = windows.entry(key.to_string()).or_insert((secs, 0));
        if secs - entry.0 >= 60 {
            *entry = (secs, 0);
        }
        entry.1 += 1;
        if entry.1 > self.limits.requests_per_window {
            return Err(u64::try_from((entry.0 + 60 - secs).max(1)).unwrap_or(1));
        }
        Ok(())
    }

    /// The field guard for the credential's owner over the metadata served now: fail closed (a
    /// `503`) when nothing can be served or no principal source is attached.
    async fn guard_for(&self, owner: RecordId) -> Rep<ScimGuard> {
        let served = self.live.current().map_err(|_| unavailable())?;
        let principals = self.live.principals().ok_or_else(unavailable)?;
        let guard = crate::live::field_guard(&served, &self.clock);
        let principal = principals.resolve(&owner).await;
        Ok(ScimGuard::new((*guard).clone(), principal))
    }

    async fn run(&self, ctx: &RequestCtx, call: ScimCall) -> Rep<ScimReply> {
        let cred = self.credential(ctx).await?;
        let now = self.clock.now_utc();
        if let Err(retry) = self.rate(cred.key, now) {
            let mut out = reply(&ScimError::rate_limited(
                "too many requests for this credential",
            ));
            out.retry_after = Some(retry);
            return Ok(out);
        }
        let method = Method::parse(&call.method)
            .ok_or_else(|| reply(&ScimError::method("this method is not supported")))?;
        let body = if call.body.is_empty() {
            None
        } else {
            Some(serde_json::from_slice(&call.body).map_err(|_| {
                reply(&ScimError::invalid_syntax(
                    "the body is not a JSON document",
                ))
            })?)
        };
        let target = call.path.rsplit('/').next().and_then(parse_record_id);
        let guard = self.guard_for(cred.owner).await?;
        let request = ScimRequest {
            method,
            path: call.path.clone(),
            query: call.query,
            body,
            if_match: call.if_match,
            content_length: call.body.len(),
        };
        let _one_at_a_time = self.gate.lock().await;
        let engine = self.engine()?;
        let mut tx = engine
            .backend()
            .begin(
                Arc::clone(&self.catalog),
                self.tx_options("scim", method == Method::Get, Isolation::Serializable)?,
            )
            .await
            .map_err(|e| storage(&e))?;
        let outcome = self
            .unit_of_work(&mut tx, &cred, &guard, request, target, now)
            .await;
        match outcome {
            Ok((response, true)) => match tx.commit().await {
                Ok(()) => Ok(response),
                Err(e) => Err(storage(&e)),
            },
            Ok((response, false)) => {
                let _ = tx.rollback().await;
                Ok(response)
            }
            Err(e) => {
                let _ = tx.rollback().await;
                Err(e)
            }
        }
    }

    /// Loads, runs the core, and writes the difference. The flag is whether to commit.
    async fn unit_of_work<T: Transaction>(
        &self,
        tx: &mut T,
        cred: &ScimCredential,
        guard: &ScimGuard,
        request: ScimRequest,
        target: Option<RecordId>,
        now: OffsetDateTime,
    ) -> Rep<(ScimReply, bool)> {
        let mutating = request.method != Method::Get;
        let mut unit = load(&self.site, tx).await?;
        if mutating && let Some(user) = target {
            let keys = keys_of(tx, user).await?.len();
            unit.live_keys.insert(user, keys);
        }
        let before = unit.clone();
        let operation = format!(
            "{} {}",
            request.method.as_str(),
            request.path.split('/').nth(3).unwrap_or("")
        );
        let response = {
            let mut sctx = ScimContext {
                tenant: self.site.clone(),
                credential: cred,
                now,
                limits: &self.limits,
                store: &mut unit,
            };
            handle_guarded(request, &mut sctx, guard)
        };
        let ok = response.status < 300;
        if mutating && ok {
            persist(tx, &before, &unit, &cred.actor(), now).await?;
            for entry in &unit.audits {
                tracing::info!(target: "spot::audit::scim", site = %self.site, action = %entry.action, subject = %entry.subject, "scim change");
            }
        }
        tracing::info!(target: "spot::audit::scim", site = %self.site, key = %cred.key, operation = %operation, status = response.status, "scim request");
        let body = response
            .body
            .as_ref()
            .map(Json::to_string)
            .unwrap_or_default();
        let out = ScimReply {
            status: response.status,
            body,
            etag: response.etag,
            location: response.location,
            retry_after: response.retry_after,
        };
        Ok((out, mutating && ok))
    }
}

impl ScimOps for ScimService {
    fn serve<'a>(&'a self, ctx: &'a RequestCtx, call: ScimCall) -> BoxFuture<'a, ScimReply> {
        Box::pin(async move { self.run(ctx, call).await.unwrap_or_else(|e| e) })
    }
}

impl ScimKeyIssuer for ScimService {
    fn issue<'a>(
        &'a self,
        owner: RecordId,
        label: &'a str,
        expires_at: Option<OffsetDateTime>,
    ) -> BoxFuture<'a, std::result::Result<String, String>> {
        Box::pin(async move {
            self.issue_key(owner, label, expires_at)
                .await
                .map_err(|e| e.status.to_string())
        })
    }
}

impl ScimService {
    /// Creates the key as `revoked`, seals its digest, then activates it: a failure at any step
    /// leaves an unusable record, never a usable key without a digest.
    async fn issue_key(
        &self,
        owner: RecordId,
        label: &str,
        expires_at: Option<OffsetDateTime>,
    ) -> Rep<String> {
        let engine = self.engine()?;
        if self.secrets.is_none() {
            tracing::warn!(
                code = CODE_VAULT_MISSING,
                "no secrets vault: a SCIM key cannot be sealed"
            );
            return Err(ScimReply::error(503, None, "no secrets vault"));
        }
        let defs = spot_platform_app::definitions().map_err(|_| unavailable())?;
        let (key, _) = ApiKey::generate();
        let id = RecordId::new_v7();
        let backend = engine.backend();
        let mut tx = backend
            .begin(
                Arc::clone(&self.catalog),
                self.tx_options("scim_issue_key", false, Isolation::ReadCommitted)?,
            )
            .await
            .map_err(|e| storage(&e))?;
        let created: Rep<()> = async {
            let kind = nm("user_type")?;
            let user = tx
                .load(&nm(doctype::USER)?, owner, RowLock::None)
                .await
                .map_err(|e| storage(&e))?;
            let is_service = user.is_some_and(|u| {
                u.data.fields.get(&kind).and_then(text).as_deref() == Some("service")
            });
            if !is_service {
                return Err(ScimReply::error(
                    403,
                    None,
                    "a SCIM key is owned by a service user",
                ));
            }
            let mut f = blank(&defs, doctype::API_KEY)?;
            put(
                &mut f,
                vec![
                    ("key_id", key.key_id().into()),
                    ("owner", owner.into()),
                    ("label", label.into()),
                    ("state", "revoked".into()),
                    ("expires_at", expires_at.map_or(Value::Null, dt)),
                ],
            )?;
            let mut scope = blank(&defs, doctype::API_KEY_SCOPE)?;
            put(&mut scope, vec![("scope", SCOPE.into())])?;
            let mut data = doc(doctype::API_KEY, id, f)?;
            data.children = BTreeMap::from([(
                nm("scopes")?,
                vec![ChildRow {
                    id: RecordId::new_v7(),
                    fields: scope,
                }],
            )]);
            tx.insert(&data).await.map_err(|e| storage(&e))?;
            Ok(())
        }
        .await;
        match created {
            Ok(()) => tx.commit().await.map_err(|e| storage(&e))?,
            Err(e) => {
                let _ = tx.rollback().await;
                return Err(e);
            }
        }
        let inv = self.system_invocation().ok_or_else(unavailable)?;
        let access = engine.secret_access(SecretCaller::LoginService, &inv);
        let record = engine
            .peek(doctype::API_KEY, id)
            .await
            .map_err(|_| unavailable())?
            .ok_or_else(unavailable)?;
        let version = record.version().ok_or_else(unavailable)?;
        engine
            .set_secret(
                &access,
                doctype::API_KEY,
                id,
                "digest",
                SecretString::new(hex(key.digest().as_bytes())),
                version,
            )
            .await
            .map_err(|_| unavailable())?;
        let mut tx = backend
            .begin(
                Arc::clone(&self.catalog),
                self.tx_options("scim_activate_key", false, Isolation::ReadCommitted)?,
            )
            .await
            .map_err(|e| storage(&e))?;
        let entity = nm(doctype::API_KEY)?;
        let Some(loaded) = tx
            .load(
                &entity,
                id,
                RowLock::Update(spot_storage_contract::Wait::Block),
            )
            .await
            .map_err(|e| storage(&e))?
        else {
            return Err(unavailable());
        };
        let mut data = loaded.data;
        data.fields.insert(nm("state")?, "active".into());
        tx.update(&data, loaded.version)
            .await
            .map_err(|e| storage(&e))?;
        tx.commit().await.map_err(|e| storage(&e))?;
        Ok(key.text())
    }
}

// ---------------------------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------------------------

/// Joins the site when `[services.scim] enabled = true`: provides [`ScimKeyIssuer`] and the
/// `spot-http` port, mounts `/scim/v2`, and adds an optional service that degrades the site when
/// there is no secrets vault. Nothing is mounted otherwise.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: ScimSettings = ctx.settings_or_default("scim")?;
    if !settings.enabled {
        return Ok(());
    }
    let (site, live, catalog) = {
        let env = ctx.env()?;
        (
            env.site.site.clone(),
            Arc::clone(env.live),
            env.permission.catalog.clone(),
        )
    };
    let secrets = ctx.port::<SecretsHost>();
    let has_vault = secrets.is_some();
    let limits = settings.limits();
    let max_body = limits.max_body_bytes;
    // The SCIM door of the sign-in throttle (it registers before this module, ORDER 1006 against
    // 1040). Without it no SCIM credential could be checked safely, so the site does not start.
    let throttle = ctx
        .port::<crate::services::signin_throttle::SigninThrottle>()
        .ok_or_else(|| {
            crate::Error("[services.scim] needs the sign-in throttle ([services.signin_throttle]), so SCIM credentials are never checked unthrottled".into())
        })?;
    let service = Arc::new(
        ScimService::new(
            site,
            live,
            Arc::clone(ctx.clock()),
            catalog,
            secrets,
            limits,
        )
        .with_throttle(throttle),
    );
    if ctx.assembling() {
        ctx.add(Idle::new("scim", Level::Optional, move || {
            Box::pin(async move {
                if has_vault {
                    Ok(())
                } else {
                    Err("no secrets vault ([services.scim] needs [services.secrets]), so SCIM credentials cannot be verified and every SCIM call fails closed".to_owned())
                }
            })
        }));
    }
    let issuer: Arc<dyn ScimKeyIssuer> = service.clone();
    ctx.provide(issuer)?;
    let port: Arc<dyn ScimPort> = Arc::new(EngineScim::new(service, max_body));
    ctx.provide(Arc::clone(&port))?;
    // Mounted here, not by the route registry: the host hands no port of this kind to the router's
    // `Ports`. The port authenticates every request itself, as a SCIM key.
    let mut ports = Ports::new();
    ports.provide::<dyn ScimPort>(port);
    let mounted = routes::mount(&[routes::scim::registration()], &ports, &[]);
    let router = mounted.authenticated;
    if let Some((module, reason)) = mounted.report.skipped.first() {
        tracing::error!(
            module = *module,
            ?reason,
            "the SCIM routes were not mounted"
        );
    }
    ctx.router_layer(move |base| base.merge(router));
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::services::scim_store::desired;
    use crate::services::scim_unit::Unit;
    use spot_iam_roles::RoleName;
    use spot_iam_scim::{GroupMapping, MappingStore, ScimStore, StoredGroup};

    fn unit() -> Unit {
        Unit {
            tenant: "acme".into(),
            users: vec![],
            groups: vec![],
            mappings: vec![],
            assignments: vec![],
            roles: vec!["Sales".into()],
            events: vec![],
            audits: vec![],
            live_keys: BTreeMap::new(),
            keys_to_disable: vec![],
        }
    }

    #[test]
    fn hex_round_trips_and_refuses_odd_or_non_hex_text() {
        assert_eq!(unhex(&hex(&[0, 15, 255])), Some(vec![0, 15, 255]));
        assert_eq!(unhex("abc"), None);
        assert_eq!(unhex("zz"), None);
    }

    #[test]
    fn another_tenant_sees_and_changes_nothing() {
        let mut u = unit();
        let other = "other".to_owned();
        assert!(u.list_users(&other).is_empty() && u.assignments(&other).is_empty());
        let row = GroupMapping {
            idp_group: "g".into(),
            role: RoleName::from("Sales"),
            scope: vec![],
        };
        assert!(u.upsert_mapping(&other, row.clone()).is_err());
        assert!(u.upsert_mapping(&"acme".to_owned(), row).is_ok());
        assert!(!u.remove_mapping(&other, "g"));
    }

    #[test]
    fn a_mapping_to_a_role_that_does_not_exist_is_refused() {
        let mut u = unit();
        let row = GroupMapping {
            idp_group: "g".into(),
            role: RoleName::from("Nope"),
            scope: vec![],
        };
        assert!(u.upsert_mapping(&"acme".to_owned(), row).is_err());
    }

    #[test]
    fn deactivation_records_the_user_whose_keys_end_and_counts_the_live_ones() {
        let mut u = unit();
        let user = RecordId::new_v7();
        u.live_keys.insert(user, 2);
        assert_eq!(u.disable_api_keys(&"acme".to_owned(), &user), 2);
        assert_eq!(u.keys_to_disable, vec![user]);
    }

    #[test]
    fn mapping_rows_keep_their_document_and_a_group_takes_its_mapping_inline() {
        let role = RoleName::from("Sales");
        let map = GroupMapping {
            idp_group: "Sales".into(),
            role,
            scope: vec![],
        };
        let existing_id = RecordId::new_v7();
        let existing = BTreeMap::from([("sales".to_owned(), existing_id)]);
        let mut fresh = BTreeMap::new();
        let only = desired(&[], std::slice::from_ref(&map), &existing, &mut fresh);
        assert_eq!(only.keys().copied().collect::<Vec<_>>(), vec![existing_id]);
        let group = StoredGroup {
            tenant: "acme".into(),
            id: RecordId::new_v7(),
            external_id: None,
            display_name: "sales".into(),
            members: vec![],
            version: 1,
            created_at: OffsetDateTime::UNIX_EPOCH,
            updated_at: OffsetDateTime::UNIX_EPOCH,
        };
        let merged = desired(std::slice::from_ref(&group), &[map], &existing, &mut fresh);
        assert_eq!(merged.len(), 1);
        assert!(merged[&group.id].mapping.is_some());
    }

    #[test]
    fn the_settings_refuse_unknown_keys_and_default_to_off() {
        assert!(!ScimSettings::default().enabled);
        let bad: std::result::Result<ScimSettings, _> = toml::from_str("enabled = true\nbogus = 1");
        assert!(bad.is_err());
        let ok: ScimSettings = toml::from_str("enabled = true\nrequests_per_window = 5").unwrap();
        assert_eq!(ok.limits().requests_per_window, 5);
    }

    #[test]
    fn a_set_of_scopes_other_than_exactly_scim_is_not_a_scim_scope() {
        let is = |s: &[&str]| s.iter().map(|x| (*x).to_owned()).collect::<Vec<_>>() == [SCOPE];
        assert!(is(&["scim"]));
        assert!(!is(&["scim", "read"]) && !is(&[]) && !is(&["read"]));
    }
}
