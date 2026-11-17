//! [`SsoIdentityService`]: the production [`SsoIdentityPort`] (WP-IAM-05.T16).
//!
//! The four calls the host makes after a provider verified an identity:
//!
//! 1. [`policy`](SsoIdentityPort::policy): the tenant's stored policy, read fresh. A store that
//!    cannot answer is a refusal, never `local`.
//! 2. [`snapshot`](SsoIdentityPort::snapshot): the directory facts the pure resolution reads
//!    ([`snapshot_in`], one read-only transaction on the site's own database) plus the configured
//!    providers and whether each still validates.
//! 3. [`sign_in`](SsoIdentityPort::sign_in): the resolution is applied in one transaction
//!    ([`apply_in`]: link by invitation, just-in-time user without roles, last sign-in) and, only
//!    when it committed, the one-time hand-over is issued (a digest in `spot_sso_handover`).
//! 4. [`redeem`](SsoIdentityPort::redeem): the hand-over is deleted-and-read in one statement, the
//!    session is started by the site's session manager over the shared session store (so the
//!    ordinary authenticator accepts its token), and the session record is then marked with how
//!    it began: `method = oidc | saml`, the cap a SAML assertion set, the audience the manager was
//!    given. The marking and the `spot_sso_session_index` row are written before the token is
//!    returned; if either fails the session is ended and the sign-in refused.
//!
//! A request for another site is refused before anything is read.

use std::collections::BTreeMap;
use std::sync::Arc;

use spot_core::{Audience, Clock};
use spot_http::api::ports::sso::{Handover, SignedIn};
use spot_http::api::service::{BoxFuture, RequestCtx, Session, UserInfo};
use spot_meta::Definition;
use spot_postgres::PgBackend;
use spot_sessions::{ClientInfo, SaltKey, SessionManager, SessionStore, UserKey};
use spot_sso::{ProviderId, ProviderStore, SignInPolicy, SsoIdentity, codes};
use spot_storage_contract::{
    Catalog, DocumentStore as _, Isolation, RecordId, RowLock, StorageBackend, Transaction, TxOptions, Value, Wait,
};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::apply::{ApplyEnv, apply_in, event};
use super::directory::{SnapshotEnv, snapshot_in};
use super::handover::{HandoverPayload, Tables};
use super::{Unavail, field};
use crate::live::SiteLive;
use crate::services::scim_store::{at, dt, nm, text};
use crate::services::sso::{DirectorySnapshot, IdentityFailure, SignInRequest, SsoIdentityPort};
use crate::services::sso_policy::PgPolicyStore;
use crate::services::sso_saml_store::PgSamlStore;

type Tx = <PgBackend as StorageBackend>::Tx;

/// What the service is built from.
pub struct Parts {
    /// The site (the tenant).
    pub site: String,
    /// The running site, for its engine.
    pub live: Arc<SiteLive>,
    /// The platform catalog.
    pub catalog: Arc<Catalog>,
    /// The injected clock.
    pub clock: Arc<dyn Clock>,
    /// The hand-over and session-index tables.
    pub tables: Tables,
    /// The tenant's policy.
    pub policy: Arc<PgPolicyStore>,
    /// The OpenID Connect provider store.
    pub oidc: Arc<dyn ProviderStore>,
    /// The SAML provider store.
    pub saml: Arc<PgSamlStore>,
    /// The sessions the authenticator reads.
    pub sessions: Arc<dyn SessionStore>,
    /// How long a hand-over lives, seconds.
    pub handover_ttl_secs: i64,
}

/// The production identity port of one site.
pub struct SsoIdentityService {
    parts: Parts,
    defs: Vec<Definition>,
    manager: SessionManager<Arc<dyn SessionStore>>,
}

fn unavailable(_: Unavail) -> IdentityFailure {
    IdentityFailure::Unavailable
}

fn audience_word(word: &str) -> Option<Audience> {
    match word {
        "staff" => Some(Audience::Staff),
        "partner" => Some(Audience::Partner),
        _ => None,
    }
}

impl SsoIdentityService {
    /// The service over `parts`.
    #[must_use]
    pub fn new(parts: Parts) -> Self {
        let defs = spot_platform_app::definitions().unwrap_or_default();
        let manager = SessionManager::new(
            Arc::clone(&parts.sessions),
            crate::login::default_login_config().session_policy,
            Arc::new(SaltKey::generate()),
        );
        Self {
            parts,
            defs,
            manager,
        }
    }

    fn check_site(&self, ctx: &RequestCtx) -> Result<(), IdentityFailure> {
        if ctx.site.as_str() == self.parts.site {
            Ok(())
        } else {
            // Another tenant's request finds no policy, no directory and no hand-over here.
            Err(IdentityFailure::Refused(codes::PROVIDER_NOT_CONFIGURED))
        }
    }

    async fn begin(&self, label: &str, read_only: bool) -> Result<Tx, IdentityFailure> {
        let engine = self
            .parts
            .live
            .engine()
            .ok_or(IdentityFailure::Unavailable)?;
        let options = TxOptions::new(
            nm(label).map_err(|_| IdentityFailure::Unavailable)?,
            Isolation::ReadCommitted,
        );
        let options = if read_only {
            options.read_only()
        } else {
            options
        };
        engine
            .backend()
            .begin(Arc::clone(&self.parts.catalog), options)
            .await
            .map_err(|_| IdentityFailure::Unavailable)
    }

    async fn providers(&self) -> Result<BTreeMap<String, (bool, Audience)>, IdentityFailure> {
        let mut map = BTreeMap::new();
        let listing = self
            .parts
            .oidc
            .list()
            .await
            .map_err(|_| IdentityFailure::Unavailable)?;
        for config in &listing.providers {
            map.insert(config.id.as_str().to_owned(), (true, config.audience));
        }
        for skipped in &listing.skipped {
            map.insert(skipped.as_str().to_owned(), (false, Audience::Staff));
        }
        for saml in self
            .parts
            .saml
            .all()
            .await
            .map_err(|_| IdentityFailure::Unavailable)?
        {
            map.insert(saml.id.as_str().to_owned(), (true, saml.provider.audience));
        }
        Ok(map)
    }

    async fn method_of(&self, provider: &ProviderId) -> Result<&'static str, IdentityFailure> {
        match self.parts.saml.find(provider).await {
            Ok(Some(_)) => Ok("saml"),
            Ok(None) => Ok("oidc"),
            Err(_) => Err(IdentityFailure::Unavailable),
        }
    }

    /// Ends a session this call started but could not finish.
    async fn abandon(&self, session: &str) {
        let _ = self
            .parts
            .sessions
            .revoke(session, "revoked_by_admin".to_owned())
            .await;
    }
}

impl SsoIdentityPort for SsoIdentityService {
    fn policy<'a>(
        &'a self,
        ctx: &'a RequestCtx,
    ) -> BoxFuture<'a, Result<SignInPolicy, IdentityFailure>> {
        Box::pin(async move {
            self.check_site(ctx)?;
            self.parts
                .policy
                .read()
                .await
                .map(|(policy, _)| policy)
                .map_err(|_| IdentityFailure::Unavailable)
        })
    }

    fn snapshot<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        identity: &'a SsoIdentity,
    ) -> BoxFuture<'a, Result<DirectorySnapshot, IdentityFailure>> {
        Box::pin(async move {
            self.check_site(ctx)?;
            let (policy, _) = self
                .parts
                .policy
                .read()
                .await
                .map_err(|_| IdentityFailure::Unavailable)?;
            let providers = self.providers().await?;
            let mut tx = self.begin("sso_snapshot", true).await?;
            let env = SnapshotEnv {
                now: self.parts.clock.now_utc(),
                policy: &policy,
                providers: &providers,
            };
            let snapshot = snapshot_in(&mut tx, &env, identity).await;
            let _ = tx.rollback().await;
            snapshot.map_err(unavailable)
        })
    }

    fn sign_in<'a>(
        &'a self,
        request: SignInRequest<'a>,
    ) -> BoxFuture<'a, Result<Handover, IdentityFailure>> {
        Box::pin(async move {
            self.check_site(request.ctx)?;
            // A consumer is never a user and has no store here yet.
            let Some(audience) = audience_word(request.audience.as_str()) else {
                return Err(IdentityFailure::Refused(codes::SSO_NOT_ALLOWED));
            };
            let (policy, _) = self
                .parts
                .policy
                .read()
                .await
                .map_err(|_| IdentityFailure::Unavailable)?;
            let now = self.parts.clock.now_utc();
            let mut tx = self.begin("sso_sign_in", false).await?;
            let env = ApplyEnv {
                now,
                defs: &self.defs,
                policy: &policy,
            };
            let user = match apply_in(&mut tx, &env, request.identity, request.resolution).await {
                Ok(user) => user,
                Err(failure) => {
                    let _ = tx.rollback().await;
                    return Err(failure);
                }
            };
            tx.commit()
                .await
                .map_err(|_| IdentityFailure::Unavailable)?;
            let method = self.method_of(request.provider).await?;
            let payload = HandoverPayload {
                user: user.to_string(),
                provider: request.provider.as_str().to_owned(),
                audience: audience.as_str().to_owned(),
                method: method.to_owned(),
                sid: None,
                not_on_or_after: request.session_not_on_or_after,
            };
            let cookie = self
                .parts
                .tables
                .issue(&payload, request.now, self.parts.handover_ttl_secs)
                .await
                .map_err(unavailable)?;
            Ok(Handover {
                location: request.return_to.to_owned(),
                cookie,
            })
        })
    }

    fn redeem<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        handover: &'a str,
    ) -> BoxFuture<'a, Result<SignedIn, IdentityFailure>> {
        Box::pin(async move {
            self.check_site(ctx)?;
            let now = self.parts.clock.now_utc();
            let now_secs = now.unix_timestamp();
            // Read and gone in one statement: of two parallel redemptions one gets the payload.
            let payload = self
                .parts
                .tables
                .redeem(handover, now_secs)
                .await
                .map_err(unavailable)?
                .ok_or(IdentityFailure::Refused(codes::STATE_MISMATCH))?;
            let refuse = || IdentityFailure::Refused(codes::STATE_MISMATCH);
            let user = spot_http::api::codec::parse_record_id(&payload.user).ok_or_else(refuse)?;
            let audience = audience_word(&payload.audience).ok_or_else(refuse)?;
            let method = match payload.method.as_str() {
                "oidc" => "oidc",
                "saml" => "saml",
                _ => return Err(refuse()),
            };
            let cap = payload.not_on_or_after;
            if cap.is_some_and(|t| now_secs >= t) {
                return Err(IdentityFailure::Refused(codes::TOKEN_EXPIRED));
            }
            let key = UserKey::new(user.to_string()).map_err(|_| IdentityFailure::Unavailable)?;
            let source = ctx.peer.map(|p| p.to_string()).unwrap_or_default();
            let started = self
                .manager
                .start(key, audience, &source, ClientInfo::default(), now_secs)
                .await
                .map_err(|_| IdentityFailure::Unavailable)?;
            let session_id = started.session.id.clone();
            let mut absolute = started.session.absolute_expires;
            if let Some(cap) = cap {
                absolute = absolute.min(cap);
            }

            // Mark how the session began, then index it: both before the token leaves.
            let marked = self
                .mark(&session_id, method, cap, now, user, absolute)
                .await;
            let who = match marked {
                Ok(who) => who,
                Err(failure) => {
                    self.abandon(&session_id).await;
                    return Err(failure);
                }
            };
            if self
                .parts
                .tables
                .index_session(
                    &session_id,
                    user,
                    &payload.provider,
                    payload.sid.as_deref(),
                    now_secs,
                )
                .await
                .is_err()
            {
                self.abandon(&session_id).await;
                return Err(IdentityFailure::Unavailable);
            }
            let expires_at = OffsetDateTime::from_unix_timestamp(absolute)
                .ok()
                .and_then(|t| t.format(&Rfc3339).ok())
                .ok_or(IdentityFailure::Unavailable)?;
            Ok(SignedIn {
                session: Session {
                    token: started.token.as_str().to_owned(),
                    expires_at,
                    user: UserInfo {
                        id: user,
                        email: who.0,
                        full_name: who.1,
                    },
                },
            })
        })
    }
}

impl SsoIdentityService {
    /// The directory facts a policy change is reviewed against: the enabled administrators, the
    /// break-glass users the new policy names and the configured providers (for the
    /// administration port; read-only).
    ///
    /// # Errors
    ///
    /// [`Unavail`] when a read fails.
    pub(crate) async fn review_view(
        &self,
        new: &SignInPolicy,
    ) -> Result<DirectorySnapshot, Unavail> {
        use super::directory::{directory_user, enabled_admins};
        let providers = self.providers().await.map_err(|_| Unavail)?;
        let mut tx = self
            .begin("sso_policy_review", true)
            .await
            .map_err(|_| Unavail)?;
        let result = async {
            let admins = enabled_admins(&mut tx).await?;
            let mut snapshot = DirectorySnapshot {
                policy: Some(new.clone()),
                providers,
                tenant_admins: admins.len(),
                break_glass_admins: admins
                    .iter()
                    .filter(|a| new.break_glass.contains(a))
                    .count(),
                ..DirectorySnapshot::default()
            };
            for id in &new.break_glass {
                if let Some(user) = directory_user(&mut tx, *id, &admins).await? {
                    snapshot.users.push(user);
                }
            }
            Ok(snapshot)
        }
        .await;
        let _ = tx.rollback().await;
        result
    }

    /// The ids of every configured provider, healthy or not.
    ///
    /// # Errors
    ///
    /// [`Unavail`] when a store cannot answer.
    pub(crate) async fn provider_ids(&self) -> Result<Vec<ProviderId>, Unavail> {
        Ok(self
            .providers()
            .await
            .map_err(|_| Unavail)?
            .keys()
            .map(|k| ProviderId::new(k.clone()))
            .collect())
    }
}

impl SsoIdentityService {
    /// Sets `method` (and the cap) on the session record and writes the two identity events, in
    /// one transaction; returns the user's e-mail and name for the answer.
    async fn mark(
        &self,
        session_id: &str,
        method: &str,
        cap: Option<i64>,
        now: OffsetDateTime,
        user: RecordId,
        absolute: i64,
    ) -> Result<(String, String), IdentityFailure> {
        let id = spot_http::api::codec::parse_record_id(session_id)
            .ok_or(IdentityFailure::Unavailable)?;
        let mut tx = self.begin("sso_session_mark", false).await?;
        let result = async {
            let entity = nm(doctype_session()).map_err(|_| IdentityFailure::Unavailable)?;
            let Some(loaded) = tx
                .load(&entity, id, RowLock::Update(Wait::Block))
                .await
                .map_err(|_| IdentityFailure::Unavailable)?
            else {
                return Err(IdentityFailure::Unavailable);
            };
            let mut data = loaded.data;
            let set = |data: &mut spot_storage_contract::DocumentData, k: &str, v: Value| {
                nm(k).map(|n| {
                    data.fields.insert(n, v);
                })
            };
            set(&mut data, "method", method.into()).map_err(|_| IdentityFailure::Unavailable)?;
            if cap.is_some() {
                let capped = OffsetDateTime::from_unix_timestamp(absolute)
                    .map_err(|_| IdentityFailure::Unavailable)?;
                set(&mut data, "absolute_expires_at", dt(capped))
                    .map_err(|_| IdentityFailure::Unavailable)?;
                let idle = at(&field(&data, "idle_expires_at"));
                if idle.is_none_or(|i| i > capped) {
                    set(&mut data, "idle_expires_at", dt(capped))
                        .map_err(|_| IdentityFailure::Unavailable)?;
                }
            }
            tx.update(&data, loaded.version)
                .await
                .map_err(|_| IdentityFailure::Unavailable)?;
            let expires = OffsetDateTime::from_unix_timestamp(absolute)
                .ok()
                .and_then(|t| t.format(&Rfc3339).ok())
                .ok_or(IdentityFailure::Unavailable)?;
            event(
                &mut tx,
                now,
                "identity_session_started",
                user,
                serde_json::json!({
                    "session": session_id,
                    "user": user.to_string(),
                    "method": method,
                    "expires_at": expires,
                }),
            )
            .await?;
            event(
                &mut tx,
                now,
                "identity_sign_in_succeeded",
                user,
                serde_json::json!({
                    "user": user.to_string(),
                    "method": method,
                    "session": session_id,
                    "step_up": false,
                }),
            )
            .await?;
            let users =
                nm(spot_platform_app::doctype::USER).map_err(|_| IdentityFailure::Unavailable)?;
            let who = tx
                .load(&users, user, RowLock::None)
                .await
                .map_err(|_| IdentityFailure::Unavailable)?
                .map(|u| {
                    (
                        text(&field(&u.data, "email")).unwrap_or_default(),
                        text(&field(&u.data, "full_name")).unwrap_or_default(),
                    )
                })
                .unwrap_or_default();
            Ok(who)
        }
        .await;
        match result {
            Ok(who) => {
                tx.commit()
                    .await
                    .map_err(|_| IdentityFailure::Unavailable)?;
                Ok(who)
            }
            Err(failure) => {
                let _ = tx.rollback().await;
                Err(failure)
            }
        }
    }
}

fn doctype_session() -> &'static str {
    spot_platform_app::doctype::SESSION
}
