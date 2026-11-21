//! The signing secret of a webhook: a write-only path and a read path for the delivery handler
//! (WP-INT-01.T5). Operator guide: `docs/running-spot/webhook-secrets.md`.
//!
//! The `secret` field of the `webhook` DocType is sealed by the engine's secret path under
//! [`SecretCaller::Webhooks`]. This module is the only place that holds that capability for the
//! `webhook` DocType, and it offers exactly two things:
//!
//! - **set and describe** ([`WebhookSecrets`]): `set` (create or rotate) seals a value in the same
//!   transaction as the webhook record's update; `fingerprint` answers the key version and a short
//!   non-reversible fingerprint, never the value. The `PUT|GET /api/v1/webhooks/{name}/signing-secret`
//!   routes of `spot-http` reach it through [`WebhookSecretOps`](spot_api_services::resources::webhook_secret::WebhookSecretOps);
//! - **signing key** ([`WebhookSigningKeys`]): the one read. It opens the value for the delivery
//!   handler, as the platform itself, and returns a [`SecretString`] (redacted `Debug`, zeroed on
//!   drop). It is a port of its own so the delivery wiring (WP-INT-01.T2) takes it and nothing
//!   wider; `spot-cli` has no dependency on `spot-webhooks`, so the adapter onto that crate's
//!   `SecretResolver` is the delivery task's.
//!
//! # Who may set a secret
//!
//! The caller authenticates through the site's sign-in service (a session that has not chosen its
//! password yet may do nothing here) and must hold, on the permission snapshot served now, both
//! the `update` right on the `webhook` DocType **and** the `update` right on its `secret`
//! protection group. The check runs **before** the webhook is looked up, so a caller without the
//! right is refused the same way whether or not the webhook exists. The engine then checks the
//! write again on the document itself.
//!
//! # Level
//!
//! Optional. With no secrets vault (`[services.secrets]` absent or failed) the service is
//! degraded, `set`, `fingerprint` and `signing_key` answer not-configured (`501
//! service.not_configured` on the routes), and a webhook cannot be signed, so no delivery is
//! attempted: it fails closed. There is no plaintext fallback.
//!
//! # Audit
//!
//! Every call that reaches the engine is journalled by the engine itself as a `sys_secret_audit`
//! event naming the caller `webhooks`, the field and the outcome, in the transaction of the write
//! (or on its own for a refusal). A call refused here, before the engine, writes one line to the
//! target `spot::audit::webhook_secret`: who, the operation and the reason code. Never a value, a
//! sealed blob, a fingerprint input or a key.

use std::sync::Arc;

use spot_api_services::AccessControl;
use spot_api_services::engine_resources::ActorResolver;
use spot_api_services::resources::webhook_secret::{
    EngineWebhookSecrets, SecretEntropy, StoredSigningSecret, WebhookSecretOps,
};
use spot_app_api::AppError;
use spot_app_api::ErrorClass;
use spot_app_api::context::{Actor, Invocation};
use spot_app_api::prelude::{Name, RecordId};
use spot_core::Clock;
use spot_document::engine::Engine;
use spot_document::secrets::SecretCaller;
use spot_events_contract::lifecycle::Operation;
use spot_http::api::Ports;
use spot_http::api::ports::webhook_secret::{
    SigningSecretText, WebhookSecretError, WebhookSecretPort, secret_fingerprint,
};
use spot_http::api::registry::Code;
use spot_http::api::router::routes;
use spot_http::api::service::{ApiError, BoxFuture, RequestCtx};
use spot_permissions::Action;
use spot_postgres::PgBackend;
use spot_secrets::SecretString;
use spot_storage_contract::{
    Expr, Isolation, Query, QueryExecutor, QueryPath, Selection, StorageBackend, TextMatch,
    TextMode, Transaction, TxOptions, Value,
};

use crate::Result;
use crate::audit::PathClass;
use crate::live::SiteLive;
use crate::services::secrets::SecretsHost;
use crate::services::{Idle, Level, SiteCtx};

/// Start order: after the framework's own services (they use 100 to 900) and the secrets module.
pub const ORDER: u32 = 1030;

/// The DocType, its secret field and its protection group.
const DOCTYPE: &str = "webhook";
const FIELD: &str = "secret";

/// The component name the delivery handler's invocation carries.
const COMPONENT: &str = "webhooks";

/// How many times a set retries after losing a version race with another edit of the record.
const SET_ATTEMPTS: u32 = 3;

// ---------------------------------------------------------------------------------------------
// The ports
// ---------------------------------------------------------------------------------------------

/// Setting and describing a webhook's signing secret, as the caller `inv` names.
pub trait WebhookSecrets: Send + Sync {
    /// Seals `secret` as the signing secret of the webhook called `webhook_name` (create or
    /// rotate), in the same transaction as the record. Returns the key version and fingerprint.
    fn set<'a>(
        &'a self,
        inv: &'a Invocation,
        webhook_name: &'a str,
        secret: &'a SecretString,
    ) -> BoxFuture<'a, std::result::Result<StoredSigningSecret, WebhookSecretError>>;

    /// The key version and fingerprint of the webhook's signing secret, `None` when it has none.
    /// Never the value.
    fn fingerprint<'a>(
        &'a self,
        inv: &'a Invocation,
        webhook_name: &'a str,
    ) -> BoxFuture<'a, std::result::Result<Option<StoredSigningSecret>, WebhookSecretError>>;
}

/// Why a signing key could not be produced. No variant carries text from the vault.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SigningKeyError {
    /// The site has no vault.
    NotConfigured,
    /// The webhook does not exist.
    NotFound,
    /// The webhook has no signing secret: an unsigned delivery is never sent.
    NotSet,
    /// The request names another site than this service's.
    WrongSite,
    /// Storage or the engine was unavailable.
    Unavailable,
    /// The stored value could not be opened (tampered, moved, key retired).
    Failed,
}

/// The signing key of a webhook, for the delivery handler: the one read of the secret.
pub trait WebhookSigningKeys: Send + Sync {
    /// Opens the signing secret of the webhook called `webhook_name` on `site`.
    ///
    /// An error, never an empty key, when the vault is absent, the webhook is unknown or the field
    /// is unset. The value is audited as an `open` by the caller `webhooks`.
    fn signing_key<'a>(
        &'a self,
        site: &'a str,
        webhook_name: &'a str,
    ) -> BoxFuture<'a, std::result::Result<SecretString, SigningKeyError>>;
}

// ---------------------------------------------------------------------------------------------
// The service
// ---------------------------------------------------------------------------------------------

/// The signing-secret service of one site.
pub struct WebhookSecretService {
    site: String,
    live: Arc<SiteLive>,
    clock: Arc<dyn Clock>,
    catalog: Arc<spot_storage_contract::Catalog>,
    secrets: Option<Arc<SecretsHost>>,
}

impl WebhookSecretService {
    /// The service of `site`. `secrets` is the vault's host, `None` when the site has none.
    pub fn new(
        site: impl Into<String>,
        live: Arc<SiteLive>,
        clock: Arc<dyn Clock>,
        catalog: Arc<spot_storage_contract::Catalog>,
        secrets: Option<Arc<SecretsHost>>,
    ) -> Self {
        Self {
            site: site.into(),
            live,
            clock,
            catalog,
            secrets,
        }
    }

    fn engine(&self) -> std::result::Result<Arc<Engine<PgBackend>>, WebhookSecretError> {
        self.live.engine().ok_or(WebhookSecretError::Unavailable)
    }

    fn vault_version(&self) -> std::result::Result<u32, WebhookSecretError> {
        self.secrets
            .as_ref()
            .map(|host| host.current_version())
            .ok_or(WebhookSecretError::NotConfigured)
    }

    fn system_invocation(&self, site: &str) -> Option<Invocation> {
        let component = Name::new(COMPONENT).ok()?;
        Some(Invocation {
            site: spot_core::SiteId::new(site).ok()?,
            actor: Actor::System { component },
            command_id: None,
            language: None,
            now: self.clock.now_utc(),
        })
    }

    /// The invocation of the caller a request names, or why there is none.
    async fn caller(
        &self,
        ctx: &RequestCtx,
    ) -> std::result::Result<Invocation, WebhookSecretError> {
        if ctx.site.as_str() != self.site {
            return Err(WebhookSecretError::Refused);
        }
        if ctx.credential.is_none() {
            return Err(WebhookSecretError::Api(ApiError::from_code(
                Code::AuthRequired,
            )));
        }
        let auth = self.live.auth().ok_or(WebhookSecretError::Unavailable)?;
        let who = auth
            .authenticate_on(ctx, PathClass::Api)
            .await
            .map_err(WebhookSecretError::Api)?;
        // A session that must still choose its password may do nothing here.
        if auth.is_restricted(&who) {
            return Err(WebhookSecretError::Refused);
        }
        Ok(Invocation {
            site: ctx.site.clone(),
            actor: Actor::User {
                id: who.user_id,
                via: None,
            },
            command_id: None,
            language: ctx.language.clone(),
            now: self.clock.now_utc(),
        })
    }

    /// Whether the caller holds `update` on the `webhook` DocType and on its `secret` group, on
    /// the snapshot served now. It runs before the webhook is looked up.
    async fn may_write(&self, inv: &Invocation) -> std::result::Result<bool, WebhookSecretError> {
        let served = self
            .live
            .current()
            .map_err(|_| WebhookSecretError::Unavailable)?;
        let principals = self
            .live
            .principals()
            .ok_or(WebhookSecretError::Unavailable)?;
        // The invocation is already built; the resolver is never asked.
        let actor: Arc<ActorResolver> = Arc::new(|_ctx: &RequestCtx| {
            Err::<Actor, ApiError>(ApiError::from_code(Code::AuthRequired))
        });
        let access = AccessControl::new(
            Arc::clone(&served.meta),
            Arc::clone(&served.decision),
            principals,
            0,
            Arc::clone(&self.clock),
            actor,
        );
        let principal = access.principal(inv).await;
        let Ok(def) = access.doctype(DOCTYPE) else {
            return Ok(false);
        };
        let action = Action::Op(Operation::Update);
        let facts = access.optimistic_facts(&def, &principal, action);
        if !access.can(&principal, action, &facts) {
            return Ok(false);
        }
        Ok(access
            .field_decision(&principal, DOCTYPE, FIELD, &facts)
            .can_update)
    }

    /// The id of the webhook called `name`, `None` when there is none.
    async fn find(
        &self,
        engine: &Engine<PgBackend>,
        name: &str,
    ) -> std::result::Result<Option<RecordId>, WebhookSecretError> {
        let failed = |_| WebhookSecretError::Failed;
        let entity = Name::new(DOCTYPE).map_err(failed)?;
        let alias = Name::new("w").map_err(failed)?;
        let mut query = Query::from(&entity, &alias, QueryPath::Report);
        query.select = vec![Selection {
            expr: Expr::field(&alias, &Name::new("id").map_err(failed)?),
            alias: Name::new("v").map_err(failed)?,
        }];
        query.filter = Some(Expr::Text(
            Box::new(Expr::field(&alias, &Name::new("name").map_err(failed)?)),
            TextMatch {
                mode: TextMode::Equals,
                pattern: name.to_owned(),
                case_insensitive: false,
            },
        ));
        query.limit = Some(1);
        let options = TxOptions::new(
            Name::new("webhook_secret_find").map_err(failed)?,
            Isolation::ReadCommitted,
        )
        .read_only();
        let mut tx = engine
            .backend()
            .begin(Arc::clone(&self.catalog), options)
            .await
            .map_err(|_| WebhookSecretError::Unavailable)?;
        let rows = tx.query(&query).await;
        let _ = tx.rollback().await;
        let rows = rows.map_err(|_| WebhookSecretError::Unavailable)?;
        Ok(rows
            .rows
            .into_iter()
            .flatten()
            .find_map(|value| match value {
                Value::Id(id) => Some(id),
                _ => None,
            }))
    }

    fn audit_refused(&self, inv: &Invocation, op: &str, reason: &str) {
        tracing::info!(
            target: "spot::audit::webhook_secret",
            site = %self.site,
            actor = %inv.actor.principal_text(),
            op,
            outcome = "refused",
            reason,
            "webhook signing secret call refused"
        );
    }
}

/// An engine error as the port's kind. The text of the error is dropped on purpose: only the
/// class and the stable code decide, and neither can hold a value.
fn app_failure(error: &AppError) -> WebhookSecretError {
    if error.code == spot_document::secrets::codes::NOT_CONFIGURED {
        return WebhookSecretError::NotConfigured;
    }
    match error.class {
        ErrorClass::PermissionDenied => WebhookSecretError::Refused,
        ErrorClass::NotFound => WebhookSecretError::NotFound,
        ErrorClass::Unavailable => WebhookSecretError::Unavailable,
        _ => {
            tracing::warn!(
                target: "spot::audit::webhook_secret",
                code = %error.code,
                "a webhook signing secret call failed"
            );
            WebhookSecretError::Failed
        }
    }
}

impl WebhookSecrets for WebhookSecretService {
    fn set<'a>(
        &'a self,
        inv: &'a Invocation,
        webhook_name: &'a str,
        secret: &'a SecretString,
    ) -> BoxFuture<'a, std::result::Result<StoredSigningSecret, WebhookSecretError>> {
        Box::pin(async move {
            if inv.site.as_str() != self.site {
                self.audit_refused(inv, "set", "wrong_site");
                return Err(WebhookSecretError::Refused);
            }
            let key_version = self.vault_version().inspect_err(|_| {
                self.audit_refused(inv, "set", "not_configured");
            })?;
            // The right first, the webhook second: no answer differs by whether it exists.
            if !self.may_write(inv).await? {
                self.audit_refused(inv, "set", "refused");
                return Err(WebhookSecretError::Refused);
            }
            let engine = self.engine()?;
            let Some(id) = self.find(&engine, webhook_name).await? else {
                self.audit_refused(inv, "set", "not_found");
                return Err(WebhookSecretError::NotFound);
            };
            let access = engine.secret_access(SecretCaller::Webhooks, inv);
            let mut attempt = 0;
            loop {
                attempt += 1;
                let Some(doc) = engine
                    .peek(DOCTYPE, id)
                    .await
                    .map_err(|e| app_failure(&e))?
                else {
                    return Err(WebhookSecretError::NotFound);
                };
                let Some(version) = doc.version() else {
                    return Err(WebhookSecretError::Failed);
                };
                // The value is handed to the engine by value (it zeroes it); a retry needs it
                // again, so each attempt seals its own copy and the copy is the engine's.
                let value = SecretString::new(secret.expose());
                match engine
                    .set_secret(&access, DOCTYPE, id, FIELD, value, version)
                    .await
                {
                    Ok(_) => break,
                    Err(e) if e.code == "document.stale_version" && attempt < SET_ATTEMPTS => {
                        continue;
                    }
                    Err(e) => return Err(app_failure(&e)),
                }
            }
            Ok(StoredSigningSecret {
                key_version,
                fingerprint: secret_fingerprint(&self.site, webhook_name, secret.expose()),
            })
        })
    }

    fn fingerprint<'a>(
        &'a self,
        inv: &'a Invocation,
        webhook_name: &'a str,
    ) -> BoxFuture<'a, std::result::Result<Option<StoredSigningSecret>, WebhookSecretError>> {
        Box::pin(async move {
            if inv.site.as_str() != self.site {
                self.audit_refused(inv, "fingerprint", "wrong_site");
                return Err(WebhookSecretError::Refused);
            }
            let key_version = self.vault_version().inspect_err(|_| {
                self.audit_refused(inv, "fingerprint", "not_configured");
            })?;
            if !self.may_write(inv).await? {
                self.audit_refused(inv, "fingerprint", "refused");
                return Err(WebhookSecretError::Refused);
            }
            let engine = self.engine()?;
            let Some(id) = self.find(&engine, webhook_name).await? else {
                self.audit_refused(inv, "fingerprint", "not_found");
                return Err(WebhookSecretError::NotFound);
            };
            let access = engine.secret_access(SecretCaller::Webhooks, inv);
            let opened = engine
                .open_secret(&access, DOCTYPE, id, FIELD)
                .await
                .map_err(|e| app_failure(&e))?;
            Ok(opened.map(|value| StoredSigningSecret {
                key_version,
                fingerprint: secret_fingerprint(&self.site, webhook_name, value.expose()),
            }))
        })
    }
}

impl WebhookSigningKeys for WebhookSecretService {
    fn signing_key<'a>(
        &'a self,
        site: &'a str,
        webhook_name: &'a str,
    ) -> BoxFuture<'a, std::result::Result<SecretString, SigningKeyError>> {
        Box::pin(async move {
            // One service serves one site: another site's webhook is not this service's to open.
            if site != self.site {
                return Err(SigningKeyError::WrongSite);
            }
            if self.secrets.is_none() {
                return Err(SigningKeyError::NotConfigured);
            }
            let engine = self.live.engine().ok_or(SigningKeyError::Unavailable)?;
            let inv = self
                .system_invocation(site)
                .ok_or(SigningKeyError::Failed)?;
            let id = match self.find(&engine, webhook_name).await {
                Ok(Some(id)) => id,
                Ok(None) => return Err(SigningKeyError::NotFound),
                Err(_) => return Err(SigningKeyError::Unavailable),
            };
            let access = engine.secret_access(SecretCaller::Webhooks, &inv);
            match engine.open_secret(&access, DOCTYPE, id, FIELD).await {
                Ok(Some(key)) => Ok(key),
                Ok(None) => Err(SigningKeyError::NotSet),
                Err(e) => Err(match app_failure(&e) {
                    WebhookSecretError::NotConfigured => SigningKeyError::NotConfigured,
                    WebhookSecretError::NotFound => SigningKeyError::NotFound,
                    WebhookSecretError::Unavailable => SigningKeyError::Unavailable,
                    _ => SigningKeyError::Failed,
                }),
            }
        })
    }
}

impl WebhookSecretOps for WebhookSecretService {
    fn set<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        webhook: &'a str,
        secret: &'a SigningSecretText,
    ) -> BoxFuture<'a, std::result::Result<StoredSigningSecret, WebhookSecretError>> {
        Box::pin(async move {
            let inv = self.caller(ctx).await?;
            let value = SecretString::new(secret.expose());
            WebhookSecrets::set(self, &inv, webhook, &value).await
        })
    }

    fn describe<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        webhook: &'a str,
    ) -> BoxFuture<'a, std::result::Result<Option<StoredSigningSecret>, WebhookSecretError>> {
        Box::pin(async move {
            let inv = self.caller(ctx).await?;
            WebhookSecrets::fingerprint(self, &inv, webhook).await
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Entropy
// ---------------------------------------------------------------------------------------------

/// The operating system's random source, through the sign-in crate's token generator (32 random
/// bytes as 43 base64url characters, zeroed on drop). It fills at most 32 bytes.
struct OsEntropy;

/// The value of one base64url character, `None` for any other.
fn sextet(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'-' => Some(62),
        b'_' => Some(63),
        _ => None,
    }
}

impl SecretEntropy for OsEntropy {
    fn fill(&self, buf: &mut [u8]) -> bool {
        if buf.len() > 32 {
            return false;
        }
        let token = spot_auth::OpaqueToken::generate();
        let mut bits: u32 = 0;
        let mut have: u32 = 0;
        let mut filled = 0usize;
        for byte in token.as_str().bytes() {
            let Some(value) = sextet(byte) else {
                buf.fill(0);
                return false;
            };
            bits = (bits << 6) | u32::from(value);
            have += 6;
            if have >= 8 {
                have -= 8;
                if filled < buf.len() {
                    // `have` is below 8 here, so the shifted value fits one byte.
                    buf[filled] = u8::try_from((bits >> have) & 0xff).unwrap_or(0);
                    filled += 1;
                }
                bits &= (1 << have) - 1;
            }
        }
        filled == buf.len()
    }
}

// ---------------------------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------------------------

/// Joins the site: provides [`WebhookSecrets`], [`WebhookSigningKeys`] and the `spot-http` port,
/// mounts the two routes, and adds an optional service that degrades the site when there is no
/// vault.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
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
    let service = Arc::new(WebhookSecretService::new(
        site,
        live,
        Arc::clone(ctx.clock()),
        catalog,
        secrets,
    ));
    if ctx.assembling() {
        ctx.add(Idle::new("webhook_secret", Level::Optional, move || {
            Box::pin(async move {
                if has_vault {
                    Ok(())
                } else {
                    Err(
                        "no secrets vault ([services.secrets] is absent), so webhook signing \
                         secrets cannot be set or read and webhooks cannot be signed"
                            .to_owned(),
                    )
                }
            })
        }));
    }
    let set_and_describe: Arc<dyn WebhookSecrets> = service.clone();
    ctx.provide(set_and_describe)?;
    let keys: Arc<dyn WebhookSigningKeys> = service.clone();
    ctx.provide(keys)?;
    let port: Arc<dyn WebhookSecretPort> =
        Arc::new(EngineWebhookSecrets::new(service, Arc::new(OsEntropy)));
    ctx.provide(Arc::clone(&port))?;
    // The group is mounted here, not by the route registry: the `spot-cli` host hands no port of
    // this kind to the router's `Ports`. The port authenticates every request itself.
    let mut ports = Ports::new();
    ports.provide::<dyn WebhookSecretPort>(port);
    let mounted = routes::mount(&[routes::webhook_secret::registration()], &ports, &[]);
    let router = mounted.authenticated;
    if let Some((module, reason)) = mounted.report.skipped.first() {
        tracing::error!(
            module = *module,
            ?reason,
            "the webhook signing-secret routes were not mounted"
        );
    }
    ctx.router_layer(move |base| base.merge(router));
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn the_entropy_source_fills_32_distinct_looking_bytes_and_nothing_wider() {
        let mut a = [0u8; 32];
        let mut b = [0u8; 32];
        assert!(OsEntropy.fill(&mut a));
        assert!(OsEntropy.fill(&mut b));
        assert_ne!(a, b);
        assert_ne!(a, [0u8; 32]);
        let mut wide = [0u8; 33];
        assert!(!OsEntropy.fill(&mut wide));
        let mut short = [0u8; 16];
        assert!(OsEntropy.fill(&mut short));
    }

    #[test]
    fn a_base64url_character_decodes_to_its_six_bits() {
        assert_eq!(sextet(b'A'), Some(0));
        assert_eq!(sextet(b'Z'), Some(25));
        assert_eq!(sextet(b'a'), Some(26));
        assert_eq!(sextet(b'9'), Some(61));
        assert_eq!(sextet(b'-'), Some(62));
        assert_eq!(sextet(b'_'), Some(63));
        assert_eq!(sextet(b'='), None);
    }
}
