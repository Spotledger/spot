//! The two ports single logout needs (WP-IAM-05.T16, for WP-IAM-05.T13): [`LogoutPorts`]
//! provides [`LogoutDirectory`] (which local users an IdP subject is linked to) and
//! [`SessionEnder`] (end their sessions).
//!
//! - **Directory.** The linked identities of `(issuer, subject)` in the site's own records. An
//!   unknown identity is an empty list (the receiver answers like for a known one, so the
//!   provider learns nothing about who has an account here). A request for another site finds
//!   none.
//! - **Ending.** Only sessions that **began through the provider that asked** end: the
//!   `spot_sso_session_index` row written when the session was started carries the provider, the
//!   user and, when the provider gave one, the `sid`. A plan with a `sid` ends the sessions of
//!   that `sid`; a plan with users ends their sessions of that provider (a password session of the
//!   same user, or a session through another provider, stays). Each session is ended through the
//!   shared session store with the stored end reason `sso_logout`, which the realtime sockets
//!   notice within their session check interval and close; API keys are not sessions and are never
//!   touched. At most `plan.limit` sessions per user end in one call. A SAML `SessionIndex` is
//!   not recorded, so a request that names only session indexes ends the identity's sessions of
//!   that provider (the safe direction for a logout).
//!
//! Nothing is ended on a store error: the receiver then answers 503 and the provider retries.

use std::sync::Arc;

use spot_http::api::service::{BoxFuture, RequestCtx};
use spot_iam_directory::LinkedIdentity;
use spot_platform_app::doctype;
use spot_sessions::SessionStore;
use spot_storage_contract::{
    Catalog, Isolation, RecordId, RowLock, StorageBackend, Transaction, TxOptions,
};

use crate::live::SiteLive;
use crate::services::scim_store::{idv, nm};
use crate::services::sso_identity::handover::Tables;
use crate::services::sso_identity::{eq_ids, field};
use crate::services::sso_logout::{EndPlan, LogoutDirectory, LogoutUnavailable, SessionEnder};

/// What the ports are built from.
pub struct Parts {
    /// The site (the tenant).
    pub site: String,
    /// The running site, for its engine.
    pub live: Arc<SiteLive>,
    /// The platform catalog.
    pub catalog: Arc<Catalog>,
    /// The sessions the authenticator reads.
    pub sessions: Arc<dyn SessionStore>,
    /// The session index.
    pub tables: Tables,
}

/// The production logout directory and session ender of one site.
pub struct LogoutPorts {
    parts: Parts,
}

impl LogoutPorts {
    /// The ports over `parts`.
    #[must_use]
    pub fn new(parts: Parts) -> Self {
        Self { parts }
    }
}

impl LogoutDirectory for LogoutPorts {
    fn linked<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        issuer: &'a str,
        subject: &'a str,
    ) -> BoxFuture<'a, Result<Vec<LinkedIdentity>, LogoutUnavailable>> {
        Box::pin(async move {
            if ctx.site.as_str() != self.parts.site {
                return Ok(Vec::new());
            }
            let engine = self.parts.live.engine().ok_or(LogoutUnavailable)?;
            let options = TxOptions::new(
                nm("sso_logout_directory").map_err(|_| LogoutUnavailable)?,
                Isolation::ReadCommitted,
            )
            .read_only();
            let mut tx = engine
                .backend()
                .begin(Arc::clone(&self.parts.catalog), options)
                .await
                .map_err(|_| LogoutUnavailable)?;
            let result = linked_in(&mut tx, issuer, subject).await;
            let _ = tx.rollback().await;
            result
        })
    }
}

/// The identities linked to `(issuer, subject)`, read in `tx`.
///
/// # Errors
///
/// [`LogoutUnavailable`] when a read fails.
pub async fn linked_in<T: Transaction>(
    tx: &mut T,
    issuer: &str,
    subject: &str,
) -> Result<Vec<LinkedIdentity>, LogoutUnavailable> {
    let ids = eq_ids(
        tx,
        doctype::LINKED_IDENTITY,
        &[("provider", issuer), ("subject", subject)],
    )
    .await
    .map_err(|_| LogoutUnavailable)?;
    let entity = nm(doctype::LINKED_IDENTITY).map_err(|_| LogoutUnavailable)?;
    let mut out = Vec::new();
    for id in ids {
        let loaded = tx
            .load(&entity, id, RowLock::None)
            .await
            .map_err(|_| LogoutUnavailable)?;
        if let Some(loaded) = loaded
            && let Some(user) = idv(&field(&loaded.data, "user"))
        {
            out.push(LinkedIdentity {
                user,
                issuer: issuer.to_owned(),
                subject: subject.to_owned(),
            });
        }
    }
    Ok(out)
}

impl SessionEnder for LogoutPorts {
    fn end<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        plan: &'a EndPlan,
    ) -> BoxFuture<'a, Result<u32, LogoutUnavailable>> {
        Box::pin(async move {
            if ctx.site.as_str() != self.parts.site {
                return Ok(0);
            }
            let provider = plan.provider.as_str();
            let per_user = i64::try_from(plan.limit).map_err(|_| LogoutUnavailable)?;
            let mut targets: Vec<(String, String)> = Vec::new();
            if let Some(sid) = plan.session_id.as_deref() {
                targets.extend(
                    self.parts
                        .tables
                        .by_sid(provider, sid, per_user)
                        .await
                        .map_err(|_| LogoutUnavailable)?,
                );
            }
            // With a `sid` the plan names the session itself; without one it names the users.
            if plan.session_id.is_none() {
                for user in &plan.users {
                    targets.extend(
                        self.parts
                            .tables
                            .by_user(provider, *user, per_user)
                            .await
                            .map_err(|_| LogoutUnavailable)?,
                    );
                }
            }
            // A request that names both a subject and a session ends that session only when it is the
            // subject's: a `sid` of somebody else's session ends nothing.
            if !plan.users.is_empty() {
                let named: Vec<String> = plan.users.iter().map(|u| user_text(*u)).collect();
                targets.retain(|(_, user)| named.contains(user));
            }
            targets.sort();
            targets.dedup();
            let mut ended = 0u32;
            for (session, _user) in targets {
                let report = self
                    .parts
                    .sessions
                    .revoke(&session, plan.reason.to_owned())
                    .await
                    .map_err(|_| LogoutUnavailable)?;
                ended = ended.saturating_add(u32::try_from(report.revoked).unwrap_or(u32::MAX));
                // The session is over either way; a leftover index row is harmless but useless.
                let _ = self.parts.tables.forget(&session).await;
            }
            Ok(ended)
        })
    }
}

/// Whether a record id text is one (used by the tests of this module's callers).
#[must_use]
pub fn is_record_id(text: &str) -> bool {
    spot_http::api::codec::parse_record_id(text).is_some()
}

/// A user id as the index stores it.
#[must_use]
pub fn user_text(user: RecordId) -> String {
    user.to_string()
}
