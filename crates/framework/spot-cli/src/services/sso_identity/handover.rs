//! The one-time hand-over and the single-logout index, in PostgreSQL (WP-IAM-05.T16).
//!
//! # The hand-over
//!
//! After a provider sign-in the browser is sent home with a random value (the `Handover` cookie of
//! the host) and exchanges it once at `sso_complete` for the session. The table
//! `spot_sso_handover` holds, per tenant, the **digest** of that value (never the value), the
//! payload the redemption needs (who, how, through which provider) and the expiry. Redeeming is
//! one `DELETE ... RETURNING`: the row is read and gone in the same statement, so of two parallel
//! redemptions exactly one gets the payload, and an expired row is not returned. Expired rows are
//! pruned when a new hand-over is issued. The payload holds ids and words, no secret.
//!
//! # The session index
//!
//! `spot_sso_session_index` records, for every session this module starts, the provider it began
//! through, the user and the IdP `sid` when the provider gave one. Single logout uses it to end
//! only the sessions that began through the provider that asked, by `sid` or by user
//! (`sso_logout_ports.rs`).
//!
//! The SQL lives in `spot-sso-pg` ([`PgHandoverTables`], WP-IAM-05.T18). This file keeps what is the
//! host's: the random cookie value and its digest, the payload and its JSON, and the types the
//! other host modules use.

use serde::{Deserialize, Serialize};
use spot_auth::OpaqueToken;
use spot_sso_pg::{PgHandoverTables, Pool};
use spot_storage_contract::RecordId;

use super::{Unavail, st};
use crate::services::scim_store::hex;
use crate::{Error, Result};

/// The idempotent DDL of both tables (kept by `spot-sso-pg`).
pub use spot_sso_pg::HANDOVER_DDL;

/// What a redemption needs: who signs in, how, and through which provider.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandoverPayload {
    /// The user to sign in (a record id).
    pub user: String,
    /// The provider's name.
    pub provider: String,
    /// `staff` or `partner`: the audience of the session.
    pub audience: String,
    /// `oidc` or `saml`: the session's `method`.
    pub method: String,
    /// The IdP session id, when the provider gave one.
    #[serde(default)]
    pub sid: Option<String>,
    /// The assertion's cap on the session, in unix seconds, when it set one.
    #[serde(default)]
    pub not_on_or_after: Option<i64>,
}

/// The hand-over and index tables of one tenant.
#[derive(Clone)]
pub struct Tables {
    rows: PgHandoverTables,
}

impl std::fmt::Debug for Tables {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Tables")
            .field("tenant", &self.rows.tenant())
            .finish_non_exhaustive()
    }
}

impl Tables {
    /// Tables on `pool` for `tenant`; `None` when the tenant is empty or longer than 128 bytes.
    #[must_use]
    pub fn new(pool: Pool, tenant: &str) -> Option<Self> {
        PgHandoverTables::new(pool, tenant).map(|rows| Self { rows })
    }

    /// The pool, for the other stores of the tenant.
    #[must_use]
    pub fn pool(&self) -> Pool {
        self.rows.pool()
    }

    /// The tenant.
    #[must_use]
    pub fn tenant(&self) -> &str {
        self.rows.tenant()
    }

    /// Creates the tables if missing. Idempotent; safe from several nodes at once.
    ///
    /// # Errors
    ///
    /// `sso_providers_host.migration_failed`.
    pub async fn prepare(&self) -> Result<()> {
        self.rows.prepare().await.map_err(|_| {
            Error(
                "sso_providers_host.migration_failed: the sign-in tables could not be created"
                    .into(),
            )
        })
    }

    /// Stores `payload` for `ttl_secs` from `now` and returns the value to put in the cookie (a
    /// secret: shown once, never stored).
    ///
    /// # Errors
    ///
    /// [`Unavail`] when the payload cannot be written; nothing was stored.
    pub async fn issue(
        &self,
        payload: &HandoverPayload,
        now: i64,
        ttl_secs: i64,
    ) -> std::result::Result<String, Unavail> {
        let text = st(serde_json::to_string(payload))?;
        let token = OpaqueToken::generate();
        let digest = hex(token.digest().as_bytes());
        st(self.rows.issue(&digest, &text, now, now + ttl_secs).await)?;
        Ok(token.as_str().to_owned())
    }

    /// Redeems `cookie` at `now`: the payload, once. `None` for an unknown, used, expired or
    /// malformed value (one answer).
    ///
    /// # Errors
    ///
    /// [`Unavail`] when the store cannot answer; the hand-over is then still redeemable.
    pub async fn redeem(
        &self,
        cookie: &str,
        now: i64,
    ) -> std::result::Result<Option<HandoverPayload>, Unavail> {
        let Ok(token) = OpaqueToken::parse(cookie) else {
            return Ok(None);
        };
        let digest = hex(token.digest().as_bytes());
        let text = st(self.rows.redeem(&digest, now).await)?;
        Ok(text.and_then(|text| serde_json::from_str(&text).ok()))
    }

    /// Records a session this module started.
    ///
    /// # Errors
    ///
    /// [`Unavail`].
    pub async fn index_session(
        &self,
        session: &str,
        user: RecordId,
        provider: &str,
        sid: Option<&str>,
        now: i64,
    ) -> std::result::Result<(), Unavail> {
        st(self
            .rows
            .index_session(session, &user.to_string(), provider, sid, now)
            .await)
    }

    /// The sessions that began through `provider` with IdP session id `sid` (newest first, at most
    /// `limit`): `(session id, user id)`.
    ///
    /// # Errors
    ///
    /// [`Unavail`].
    pub async fn by_sid(
        &self,
        provider: &str,
        sid: &str,
        limit: i64,
    ) -> std::result::Result<Vec<(String, String)>, Unavail> {
        st(self.rows.by_sid(provider, sid, limit).await)
    }

    /// The sessions of `user` that began through `provider` (newest first, at most `limit`).
    ///
    /// # Errors
    ///
    /// [`Unavail`].
    pub async fn by_user(
        &self,
        provider: &str,
        user: RecordId,
        limit: i64,
    ) -> std::result::Result<Vec<(String, String)>, Unavail> {
        st(self.rows.by_user(provider, &user.to_string(), limit).await)
    }

    /// Drops an index row (the session ended).
    ///
    /// # Errors
    ///
    /// [`Unavail`].
    pub async fn forget(&self, session: &str) -> std::result::Result<(), Unavail> {
        st(self.rows.forget(session).await)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_payload_is_ids_and_words_only() {
        let payload = HandoverPayload {
            user: RecordId::new_v7().to_string(),
            provider: "okta".to_owned(),
            audience: "staff".to_owned(),
            method: "oidc".to_owned(),
            sid: Some("sid-1".to_owned()),
            not_on_or_after: Some(1_790_000_000),
        };
        let text = serde_json::to_string(&payload).unwrap();
        assert_eq!(
            serde_json::from_str::<HandoverPayload>(&text).unwrap(),
            payload
        );
        // An unknown key (a field someone added to smuggle a value) is refused.
        let extra = text.replace('}', ",\"token\":\"x\"}");
        assert!(serde_json::from_str::<HandoverPayload>(&extra).is_err());
    }
}
