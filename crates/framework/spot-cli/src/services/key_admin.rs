//! API keys, the SCIM credential and other users' sessions over HTTP (WP-IAM-12.T3): the
//! `KeyAdmin` and `SessionAdmin` areas of `IdentityAdminPort`, provided as ports; the group itself
//! is mounted by `services/identity_admin.rs`, which composes them in its finaliser.
//!
//! - **Keys**: created in two steps like the SCIM issuer (the record is `revoked` until its digest is
//!   sealed, then activated), so no usable key exists without a digest. The secret is returned once,
//!   by `api_key_create`, and never listed. A key is owned by the caller or by a `service` user, never
//!   by another human. Scopes are `doctype:action` texts. **Scope subset (fail closed)**: until the
//!   permission snapshot is reachable from here, only an owner holding `Tenant Administrator` may hold
//!   any scope; for every other owner each scope is an excess and the refusal
//!   (`identity.scope_exceeds_owner`) lists them. A key's standing is the owner's `state` on every use
//!   (the sign-in service re-reads it), and a disable revokes the owner's keys (`identity_admin`).
//! - **SCIM credential**: created through the `ScimKeyIssuer` port for a service user named
//!   `scim-provisioner` (made on first use); without the SCIM service the call is `503`. Rotation is
//!   create-then-delete by the administrator, so there is no silent overlap.
//! - **Sessions**: list (no tokens), end one, end all (`state = ended`); each is audited with the acting
//!   administrator on `spot::audit::identity`.
//! - **Impersonation**: default deny. There is no support grant store on this host, so
//!   `identity.impersonation_refused` answers every start, and an end is `404`.
//!
//! The caller and its `identity.keys.manage` right are decided by the group's permission port.
//! Settings `[services.key_admin]`: `enabled` (true), `max_key_days` (365, 1 to 3650).

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value as Json;
use spot_auth::ApiKey;
use spot_core::Clock;
use spot_document::engine::Engine;
use spot_document::secrets::SecretCaller;
use spot_http::api::ports::identity::{
    AdminSessionOut, ApiKeyCreateIn, ApiKeyOut, IdResult, ImpersonationIn, KeyAdmin, NewApiKey,
    NewScimCredential, Page, PageIn, ScimOut, SessionAdmin, SessionListIn,
};
use spot_http::api::registry::Code;
use spot_http::api::service::{ApiError, BoxFuture, RequestCtx};
use spot_platform_app::doctype;
use spot_postgres::PgBackend;
use spot_secrets::SecretString;
use spot_storage_contract::{
    Catalog, ChildRow, DocumentStore, ErrorKind, Isolation, RecordId, RowLock, StorageBackend,
    StorageError, Transaction, TxOptions, Value, Wait,
};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::live::SiteLive;
use crate::audit::PathClass;
use crate::services::SiteCtx;
use crate::services::scim::ScimKeyIssuer;
use crate::services::scim_store::{at, blank, doc, dt, find_id, hex, idv, nm, put, rows, text};
use crate::services::secrets::SecretsHost;
use crate::{Error, Result};

/// After user administration (1050): it composes this module's ports in its finaliser.
pub const ORDER: u32 = 1060;

const AUDIT: &str = "spot::audit::identity";
const SCIM_USER: &str = "scim-provisioner";

type Tx = <PgBackend as StorageBackend>::Tx;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct Settings {
    enabled: bool,
    max_key_days: i64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            max_key_days: 365,
        }
    }
}

fn unavailable() -> ApiError {
    ApiError::from_code(Code::ServerUnavailable)
}

fn not_found() -> ApiError {
    ApiError::from_code(Code::IdentityNotFound)
}

fn invalid(field: &str) -> ApiError {
    ApiError::from_code(Code::IdentityInvalid)
        .detail("fields", Json::Array(vec![Json::from(field)]))
}

fn infra<T, E>(r: std::result::Result<T, E>) -> IdResult<T> {
    r.map_err(|_| unavailable())
}

fn store(e: &StorageError) -> ApiError {
    match e.kind {
        ErrorKind::UniqueViolation => ApiError::from_code(Code::IdentityUserExists),
        _ => unavailable(),
    }
}

fn rfc(t: OffsetDateTime) -> String {
    t.format(&Rfc3339).unwrap_or_default()
}

fn record(text: &str) -> IdResult<RecordId> {
    spot_http::api::codec::parse_record_id(text).ok_or_else(not_found)
}

/// `doctype:action` with a lower-case storage name (or `*`) and a known action.
fn scope_ok(scope: &str) -> bool {
    let Some((doctype, action)) = scope.split_once(':') else {
        return false;
    };
    let name = doctype == "*"
        || (!doctype.is_empty()
            && doctype.len() <= 63
            && doctype
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'));
    name && matches!(
        action,
        "read" | "create" | "update" | "delete" | "submit" | "cancel"
    )
}

/// The key and session administration of one site.
pub struct KeyService {
    site: String,
    live: Arc<SiteLive>,
    clock: Arc<dyn Clock>,
    catalog: Arc<Catalog>,
    secrets: Option<Arc<SecretsHost>>,
    scim: Option<Arc<dyn ScimKeyIssuer>>,
    settings: Settings,
}

impl KeyService {
    fn engine(&self) -> IdResult<Arc<Engine<PgBackend>>> {
        self.live.engine().ok_or_else(unavailable)
    }

    async fn begin(&self, label: &str, read_only: bool) -> IdResult<Tx> {
        let options = TxOptions::new(infra(nm(label))?, Isolation::ReadCommitted);
        let options = if read_only {
            options.read_only()
        } else {
            options
        };
        self.engine()?
            .backend()
            .begin(Arc::clone(&self.catalog), options)
            .await
            .map_err(|e| store(&e))
    }

    async fn finish<R>(&self, tx: Tx, result: IdResult<R>) -> IdResult<R> {
        match result {
            Ok(value) => {
                tx.commit().await.map_err(|e| store(&e))?;
                Ok(value)
            }
            Err(e) => {
                let _ = tx.rollback().await;
                Err(e)
            }
        }
    }

    async fn caller(&self, ctx: &RequestCtx) -> IdResult<RecordId> {
        if ctx.site.as_str() != self.site {
            return Err(ApiError::from_code(Code::PermissionDenied));
        }
        let auth = self.live.auth().ok_or_else(unavailable)?;
        let who = auth.authenticate_on(ctx, PathClass::Api).await?;
        if auth.is_restricted(&who) {
            return Err(ApiError::from_code(Code::PermissionDenied));
        }
        Ok(who.user_id)
    }

    fn audit(&self, by: RecordId, op: &str, target: Option<RecordId>, outcome: &str) {
        tracing::info!(
            target: AUDIT,
            site = %self.site,
            actor = %by,
            op,
            target = target.map(|t| t.to_string()).as_deref().unwrap_or(""),
            outcome,
            "identity administration"
        );
    }

    /// Whether `user` is a `service` user, and whether they hold `Tenant Administrator`.
    async fn owner_facts(&self, tx: &mut Tx, user: RecordId) -> IdResult<(bool, bool, bool)> {
        let loaded = tx
            .load(&infra(nm(doctype::USER))?, user, RowLock::None)
            .await
            .map_err(|e| store(&e))?
            .ok_or_else(not_found)?;
        let field = |n: &str| nm(n).ok().and_then(|n| loaded.data.fields.get(&n).cloned());
        let service = field("user_type").as_ref().and_then(text).as_deref() == Some("service");
        let active = field("state").as_ref().and_then(text).as_deref() == Some("active");
        let roles = infra(rows(tx, doctype::ROLE, &["id", "role_name"]).await)?;
        let admin_role = roles
            .iter()
            .find(|r| text(&r[1]).as_deref() == Some(spot_platform_app::role::TENANT_ADMINISTRATOR))
            .and_then(|r| idv(&r[0]));
        let assigned = infra(rows(tx, doctype::ROLE_ASSIGNMENT, &["user", "role", "state"]).await)?;
        let admin = admin_role.is_some_and(|role| {
            assigned.iter().any(|r| {
                idv(&r[0]) == Some(user)
                    && idv(&r[1]) == Some(role)
                    && text(&r[2]).as_deref() == Some("active")
            })
        });
        Ok((service, active, admin))
    }

    async fn scopes_of(&self, tx: &mut Tx, key: RecordId) -> IdResult<Vec<String>> {
        let loaded = tx
            .load(&infra(nm(doctype::API_KEY))?, key, RowLock::None)
            .await
            .map_err(|e| store(&e))?;
        let children = loaded.map(|d| d.data.children).unwrap_or_default();
        Ok(nm("scopes")
            .ok()
            .and_then(|n| children.get(&n).cloned())
            .unwrap_or_default()
            .iter()
            .filter_map(|c| {
                nm("scope")
                    .ok()
                    .and_then(|n| c.fields.get(&n).and_then(text))
            })
            .collect())
    }

    async fn keys(&self, tx: &mut Tx, scim: bool) -> IdResult<Vec<(RecordId, ApiKeyOut)>> {
        let all = infra(
            rows(
                tx,
                doctype::API_KEY,
                &[
                    "id",
                    "key_id",
                    "owner",
                    "label",
                    "state",
                    "expires_at",
                    "last_used_at",
                ],
            )
            .await,
        )?;
        let mut out = Vec::new();
        for r in all
            .iter()
            .filter(|r| text(&r[4]).as_deref() == Some("active"))
        {
            let (Some(id), Some(owner)) = (idv(&r[0]), idv(&r[2])) else {
                continue;
            };
            let scopes = self.scopes_of(tx, id).await?;
            if scopes.iter().any(|s| s == "scim") != scim {
                continue;
            }
            out.push((
                id,
                ApiKeyOut {
                    id: id.to_string(),
                    name: text(&r[3]).unwrap_or_default(),
                    owner_id: owner.to_string(),
                    scopes,
                    created_at: rfc(OffsetDateTime::UNIX_EPOCH),
                    last_used_at: at(&r[6]).map(rfc),
                    expires_at: at(&r[5]).map(rfc),
                    prefix: None,
                },
            ));
        }
        out.sort_by_key(|(id, _)| *id);
        Ok(out)
    }

    fn page<T>(
        mut items: Vec<(RecordId, T)>,
        page: &PageIn,
        id_of: impl Fn(&T) -> String,
    ) -> Page<T> {
        let after = page
            .cursor
            .as_deref()
            .and_then(spot_http::api::codec::parse_record_id);
        items.retain(|(id, _)| after.is_none_or(|a| *id > a));
        let mut out: Vec<T> = items.into_iter().map(|(_, t)| t).collect();
        let next = (out.len() > page.limit).then(|| {
            out.truncate(page.limit);
            out.last().map(&id_of).unwrap_or_default()
        });
        Page {
            items: out,
            next_cursor: next,
        }
    }

    /// Creates a key `revoked`, seals its digest, then activates it.
    async fn issue(
        &self,
        owner: RecordId,
        name: &str,
        scopes: &[String],
        expires: OffsetDateTime,
    ) -> IdResult<(RecordId, String)> {
        if self.secrets.is_none() {
            return Err(unavailable());
        }
        let engine = self.engine()?;
        let defs = infra(spot_platform_app::definitions())?;
        let (key, _) = ApiKey::generate();
        let id = RecordId::new_v7();
        let mut tx = self.begin("identity_key_create", false).await?;
        let created: IdResult<()> = async {
            let mut f = infra(blank(&defs, doctype::API_KEY))?;
            infra(put(
                &mut f,
                vec![
                    ("key_id", key.key_id().into()),
                    ("owner", owner.into()),
                    ("label", name.into()),
                    ("state", "revoked".into()),
                    ("expires_at", dt(expires)),
                ],
            ))?;
            let mut data = infra(doc(doctype::API_KEY, id, f))?;
            let mut rows_out = Vec::new();
            for scope in scopes {
                let mut s = infra(blank(&defs, doctype::API_KEY_SCOPE))?;
                infra(put(&mut s, vec![("scope", scope.as_str().into())]))?;
                rows_out.push(ChildRow {
                    id: RecordId::new_v7(),
                    fields: s,
                });
            }
            data.children = BTreeMap::from([(infra(nm("scopes"))?, rows_out)]);
            tx.insert(&data).await.map_err(|e| store(&e))?;
            Ok(())
        }
        .await;
        self.finish(tx, created).await?;
        let invocation = spot_app_api::context::Invocation {
            site: infra(spot_core::SiteId::new(&self.site))?,
            actor: spot_app_api::context::Actor::System {
                component: infra(spot_app_api::prelude::Name::new("key_admin"))?,
            },
            command_id: None,
            language: None,
            now: self.clock.now_utc(),
        };
        let access = engine.secret_access(SecretCaller::LoginService, &invocation);
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
        let mut tx = self.begin("identity_key_activate", false).await?;
        let activated: IdResult<()> = async {
            let loaded = tx
                .load(
                    &infra(nm(doctype::API_KEY))?,
                    id,
                    RowLock::Update(Wait::Block),
                )
                .await
                .map_err(|e| store(&e))?
                .ok_or_else(unavailable)?;
            let mut data = loaded.data;
            data.fields.insert(infra(nm("state"))?, "active".into());
            tx.update(&data, loaded.version)
                .await
                .map_err(|e| store(&e))?;
            Ok(())
        }
        .await;
        self.finish(tx, activated).await?;
        Ok((id, key.text()))
    }

    async fn revoke(&self, tx: &mut Tx, entity: &str, id: RecordId, to: &str) -> IdResult<bool> {
        let Some(loaded) = tx
            .load(&infra(nm(entity))?, id, RowLock::Update(Wait::Block))
            .await
            .map_err(|e| store(&e))?
        else {
            return Ok(false);
        };
        let mut data = loaded.data;
        data.fields.insert(infra(nm("state"))?, to.into());
        tx.update(&data, loaded.version)
            .await
            .map_err(|e| store(&e))?;
        Ok(true)
    }
}

impl KeyAdmin for KeyService {
    fn api_key_list<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        page: PageIn,
    ) -> BoxFuture<'a, IdResult<Page<ApiKeyOut>>> {
        Box::pin(async move {
            self.caller(ctx).await?;
            let mut tx = self.begin("identity_key_list", true).await?;
            let result = self.keys(&mut tx, false).await;
            let items = self.finish(tx, result).await?;
            Ok(Self::page(items, &page, |k| k.id.clone()))
        })
    }

    fn api_key_create<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        input: ApiKeyCreateIn,
    ) -> BoxFuture<'a, IdResult<NewApiKey>> {
        Box::pin(async move {
            let by = self.caller(ctx).await?;
            let owner = record(&input.owner_id).map_err(|_| invalid("owner_id"))?;
            if input.name.trim().is_empty() {
                return Err(invalid("name"));
            }
            if input.scopes.is_empty() || input.scopes.iter().any(|s| !scope_ok(s) || s == "scim") {
                return Err(invalid("scopes"));
            }
            let now = self.clock.now_utc();
            let expires = match input.expires_at.as_deref() {
                Some(text) => {
                    OffsetDateTime::parse(text, &Rfc3339).map_err(|_| invalid("expires_at"))?
                }
                None => now + time::Duration::days(self.settings.max_key_days),
            };
            if expires <= now || expires > now + time::Duration::days(self.settings.max_key_days) {
                return Err(invalid("expires_at"));
            }
            let mut tx = self.begin("identity_key_owner", true).await?;
            let facts = self.owner_facts(&mut tx, owner).await;
            let _ = tx.rollback().await;
            let (service, active, admin) = facts?;
            // A user makes keys for themselves, an administrator for a service user, never another human.
            if !active || (owner != by && !service) {
                return Err(ApiError::from_code(Code::PermissionDenied));
            }
            if !admin {
                return Err(ApiError::from_code(Code::IdentityScopeExceedsOwner).detail(
                    "excess",
                    Json::Array(
                        input
                            .scopes
                            .iter()
                            .map(|s| Json::from(s.as_str()))
                            .collect(),
                    ),
                ));
            }
            let (id, secret) = self
                .issue(owner, input.name.trim(), &input.scopes, expires)
                .await?;
            self.audit(by, "api_key_create", Some(id), "ok");
            Ok(NewApiKey {
                key: ApiKeyOut {
                    id: id.to_string(),
                    name: input.name.trim().to_owned(),
                    owner_id: owner.to_string(),
                    scopes: input.scopes,
                    created_at: rfc(now),
                    last_used_at: None,
                    expires_at: Some(rfc(expires)),
                    prefix: None,
                },
                secret,
            })
        })
    }

    fn api_key_delete<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
    ) -> BoxFuture<'a, IdResult<()>> {
        Box::pin(async move {
            let by = self.caller(ctx).await?;
            let id = record(id)?;
            let mut tx = self.begin("identity_key_revoke", false).await?;
            let result = async {
                if self
                    .revoke(&mut tx, doctype::API_KEY, id, "revoked")
                    .await?
                {
                    Ok(())
                } else {
                    Err(not_found())
                }
            }
            .await;
            let out = self.finish(tx, result).await;
            self.audit(
                by,
                "api_key_delete",
                Some(id),
                if out.is_ok() { "ok" } else { "refused" },
            );
            out
        })
    }

    fn scim_list<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        page: PageIn,
    ) -> BoxFuture<'a, IdResult<Page<ScimOut>>> {
        Box::pin(async move {
            self.caller(ctx).await?;
            let mut tx = self.begin("identity_scim_list", true).await?;
            let result = self.keys(&mut tx, true).await;
            let keys = self.finish(tx, result).await?;
            let items = keys
                .into_iter()
                .map(|(id, k)| {
                    (
                        id,
                        ScimOut {
                            id: k.id,
                            created_at: k.created_at,
                            last_used_at: k.last_used_at,
                            prefix: None,
                        },
                    )
                })
                .collect();
            Ok(Self::page(items, &page, |s: &ScimOut| s.id.clone()))
        })
    }

    fn scim_create<'a>(
        &'a self,
        ctx: &'a RequestCtx,
    ) -> BoxFuture<'a, IdResult<NewScimCredential>> {
        Box::pin(async move {
            let by = self.caller(ctx).await?;
            let issuer = self.scim.clone().ok_or_else(unavailable)?;
            let now = self.clock.now_utc();
            let owner = self.scim_owner().await?;
            let secret = issuer
                .issue(owner, "SCIM credential", None)
                .await
                .map_err(|_| unavailable())?;
            self.audit(by, "scim_create", None, "ok");
            Ok(NewScimCredential {
                credential: ScimOut {
                    id: owner.to_string(),
                    created_at: rfc(now),
                    last_used_at: None,
                    prefix: None,
                },
                secret,
            })
        })
    }

    fn scim_delete<'a>(&'a self, ctx: &'a RequestCtx, id: &'a str) -> BoxFuture<'a, IdResult<()>> {
        self.api_key_delete(ctx, id)
    }
}

impl KeyService {
    /// The `service` user a SCIM credential belongs to, made on first use.
    async fn scim_owner(&self) -> IdResult<RecordId> {
        let mut tx = self.begin("identity_scim_owner", false).await?;
        let result = async {
            if let Some(id) = infra(find_id(&mut tx, doctype::USER, "user_name", SCIM_USER).await)?
            {
                return Ok(id);
            }
            let defs = infra(spot_platform_app::definitions())?;
            let mut f = infra(blank(&defs, doctype::USER))?;
            infra(put(
                &mut f,
                vec![
                    ("user_name", SCIM_USER.into()),
                    ("full_name", "SCIM provisioner".into()),
                    ("user_type", "service".into()),
                    ("state", "active".into()),
                    ("language", "en".into()),
                    ("time_zone", "UTC".into()),
                    ("failed_attempts", Value::Int(0)),
                ],
            ))?;
            let id = RecordId::new_v7();
            tx.insert(&infra(doc(doctype::USER, id, f))?)
                .await
                .map_err(|e| store(&e))?;
            Ok(id)
        }
        .await;
        self.finish(tx, result).await
    }
}

impl SessionAdmin for KeyService {
    fn session_list<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        q: SessionListIn,
    ) -> BoxFuture<'a, IdResult<Page<AdminSessionOut>>> {
        Box::pin(async move {
            self.caller(ctx).await?;
            let user = record(&q.user)?;
            let mut tx = self.begin("identity_session_list", true).await?;
            let result = async {
                let all = infra(
                    rows(
                        &mut tx,
                        doctype::SESSION,
                        &[
                            "id",
                            "user",
                            "state",
                            "method",
                            "created_at",
                            "last_seen",
                            "client_info",
                        ],
                    )
                    .await,
                )?;
                Ok(all
                    .iter()
                    .filter(|r| {
                        idv(&r[1]) == Some(user) && text(&r[2]).as_deref() == Some("active")
                    })
                    .filter_map(|r| {
                        let id = idv(&r[0])?;
                        Some((
                            id,
                            AdminSessionOut {
                                id: id.to_string(),
                                user_id: user.to_string(),
                                created_at: rfc(at(&r[4])?),
                                last_seen_at: at(&r[5]).map(rfc),
                                client: text(&r[6]),
                                method: text(&r[3]),
                                impersonated_by: None,
                            },
                        ))
                    })
                    .collect::<Vec<_>>())
            }
            .await;
            let mut items = self.finish(tx, result).await?;
            items.sort_by_key(|(id, _)| *id);
            Ok(Self::page(items, &q.page, |s: &AdminSessionOut| {
                s.id.clone()
            }))
        })
    }

    fn session_end<'a>(&'a self, ctx: &'a RequestCtx, id: &'a str) -> BoxFuture<'a, IdResult<()>> {
        Box::pin(async move {
            let by = self.caller(ctx).await?;
            let id = record(id)?;
            let mut tx = self.begin("identity_session_end", false).await?;
            let result = async {
                if self.revoke(&mut tx, doctype::SESSION, id, "ended").await? {
                    Ok(())
                } else {
                    Err(not_found())
                }
            }
            .await;
            let out = self.finish(tx, result).await;
            self.audit(
                by,
                "session_end",
                Some(id),
                if out.is_ok() { "ok" } else { "refused" },
            );
            out
        })
    }

    fn sessions_end_all<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        user: &'a str,
    ) -> BoxFuture<'a, IdResult<u64>> {
        Box::pin(async move {
            let by = self.caller(ctx).await?;
            let user = record(user)?;
            let mut tx = self.begin("identity_sessions_end_all", false).await?;
            let result = async {
                let all = infra(rows(&mut tx, doctype::SESSION, &["id", "user", "state"]).await)?;
                let mut n = 0u64;
                for r in all.iter().filter(|r| {
                    idv(&r[1]) == Some(user) && text(&r[2]).as_deref() == Some("active")
                }) {
                    if let Some(id) = idv(&r[0])
                        && self.revoke(&mut tx, doctype::SESSION, id, "ended").await?
                    {
                        n += 1;
                    }
                }
                Ok(n)
            }
            .await;
            let out = self.finish(tx, result).await;
            self.audit(
                by,
                "sessions_end_all",
                Some(user),
                if out.is_ok() { "ok" } else { "refused" },
            );
            out
        })
    }

    fn impersonation_start<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        _input: ImpersonationIn,
    ) -> BoxFuture<'a, IdResult<String>> {
        Box::pin(async move {
            let by = self.caller(ctx).await?;
            // Default deny: no support grant exists on this host.
            self.audit(by, "impersonation_start", None, "refused");
            Err(ApiError::from_code(Code::IdentityImpersonationRefused))
        })
    }

    fn impersonation_end<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, IdResult<()>> {
        Box::pin(async move {
            self.caller(ctx).await?;
            Err(not_found())
        })
    }
}

/// Joins the site unless `[services.key_admin] enabled = false`: provides `dyn KeyAdmin` and
/// `dyn SessionAdmin` for the identity group.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("key_admin")?;
    if !settings.enabled {
        return Ok(());
    }
    if !(1..=3650).contains(&settings.max_key_days) {
        return Err(Error(
            "services.key_admin: max_key_days is 1 to 3650".to_owned(),
        ));
    }
    let (site, live, catalog) = {
        let env = ctx.env()?;
        (
            env.site.site.clone(),
            Arc::clone(env.live),
            env.permission.catalog.clone(),
        )
    };
    let service = Arc::new(KeyService {
        site,
        live,
        clock: Arc::clone(ctx.clock()),
        catalog,
        secrets: ctx.port::<SecretsHost>(),
        scim: ctx.port::<dyn ScimKeyIssuer>(),
        settings,
    });
    let keys: Arc<dyn KeyAdmin> = service.clone();
    ctx.provide(keys)?;
    let sessions: Arc<dyn SessionAdmin> = service;
    ctx.provide(sessions)?;
    Ok(())
}
