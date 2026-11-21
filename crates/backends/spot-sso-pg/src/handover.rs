//! The one-time hand-over and the single-logout index rows (WP-IAM-05.T18): [`PgHandoverTables`] is
//! the SQL of `spot_sso_handover` and `spot_sso_session_index`, moved here from the host so the G5
//! lint passes. The host owns what goes in them (the random cookie value and its digest, the payload
//! JSON); this crate sees digests, payload text and ids only.
//!
//! # The hand-over
//!
//! After a provider sign-in the browser is sent home with a random value and exchanges it once for
//! the session. The table holds, per tenant, the **digest** of that value (never the value), the
//! payload text the redemption needs and the expiry. Redeeming is one `DELETE ... RETURNING`: the
//! row is read and gone in the same statement, so of two parallel redemptions exactly one gets the
//! payload, and an expired row is not returned. Expired rows of the tenant are pruned when a new
//! hand-over is issued.
//!
//! # The session index
//!
//! One row per session the host started through a provider: the provider, the user and the IdP `sid`
//! when the provider gave one. Single logout finds the sessions that began through the provider that
//! asked, by `sid` or by user.

use deadpool_postgres::Pool;
use spot_app_api::AppResult;

use crate::sql::run_ddl;

const PREPARE_LOCK: i64 = 0x5353_4f50_0103;

/// The idempotent DDL of both tables.
pub const HANDOVER_DDL: &str = r"
CREATE TABLE IF NOT EXISTS spot_sso_handover (
    tenant     text   NOT NULL,
    digest     text   NOT NULL,
    payload    text   NOT NULL,
    expires_at bigint NOT NULL,
    PRIMARY KEY (tenant, digest),
    CONSTRAINT spot_sso_handover_keys CHECK (
        octet_length(tenant) BETWEEN 1 AND 128
        AND octet_length(digest) = 64
        AND octet_length(payload) BETWEEN 2 AND 4096)
);
CREATE TABLE IF NOT EXISTS spot_sso_session_index (
    tenant     text   NOT NULL,
    session_id text   NOT NULL,
    user_id    text   NOT NULL,
    provider   text   NOT NULL,
    sid        text,
    created_at bigint NOT NULL,
    PRIMARY KEY (tenant, session_id),
    CONSTRAINT spot_sso_session_index_keys CHECK (
        octet_length(tenant) BETWEEN 1 AND 128
        AND octet_length(session_id) BETWEEN 1 AND 128
        AND octet_length(user_id) BETWEEN 1 AND 128
        AND octet_length(provider) BETWEEN 1 AND 128
        AND (sid IS NULL OR octet_length(sid) BETWEEN 1 AND 512))
);
CREATE INDEX IF NOT EXISTS spot_sso_session_index_sid
    ON spot_sso_session_index (tenant, provider, sid);
CREATE INDEX IF NOT EXISTS spot_sso_session_index_user
    ON spot_sso_session_index (tenant, provider, user_id);
";

const PRUNE: &str = "DELETE FROM spot_sso_handover WHERE tenant = $1 AND expires_at <= $2";
const ISSUE: &str = "INSERT INTO spot_sso_handover (tenant, digest, payload, expires_at) \
    VALUES ($1, $2, $3, $4)";
const REDEEM: &str = "DELETE FROM spot_sso_handover \
    WHERE tenant = $1 AND digest = $2 AND expires_at > $3 RETURNING payload";
const INDEX: &str = "INSERT INTO spot_sso_session_index \
    (tenant, session_id, user_id, provider, sid, created_at) VALUES ($1, $2, $3, $4, $5, $6) \
    ON CONFLICT (tenant, session_id) DO NOTHING";
const BY_SID: &str = "SELECT session_id, user_id FROM spot_sso_session_index \
    WHERE tenant = $1 AND provider = $2 AND sid = $3 ORDER BY created_at DESC LIMIT $4";
const BY_USER: &str = "SELECT session_id, user_id FROM spot_sso_session_index \
    WHERE tenant = $1 AND provider = $2 AND user_id = $3 ORDER BY created_at DESC LIMIT $4";
const FORGET: &str = "DELETE FROM spot_sso_session_index WHERE tenant = $1 AND session_id = $2";

/// The store could not answer; nothing was changed (a redeemable hand-over stays redeemable). No
/// driver text is carried.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HandoverUnavailable;

/// The hand-over and index tables of one tenant.
#[derive(Clone)]
pub struct PgHandoverTables {
    pool: Pool,
    tenant: String,
}

impl std::fmt::Debug for PgHandoverTables {
    /// Names the tenant, never a pool or connection detail.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgHandoverTables")
            .field("tenant", &self.tenant)
            .finish_non_exhaustive()
    }
}

impl PgHandoverTables {
    /// Tables on `pool` for `tenant`; `None` when the tenant is empty or longer than 128 bytes.
    #[must_use]
    pub fn new(pool: Pool, tenant: &str) -> Option<Self> {
        (!tenant.is_empty() && tenant.len() <= 128).then(|| Self {
            pool,
            tenant: tenant.to_owned(),
        })
    }

    /// The pool, for the other stores of the tenant.
    #[must_use]
    pub fn pool(&self) -> Pool {
        self.pool.clone()
    }

    /// The tenant.
    #[must_use]
    pub fn tenant(&self) -> &str {
        &self.tenant
    }

    /// Creates the tables if missing. Idempotent; safe from several nodes at once.
    ///
    /// # Errors
    /// `sso_pg.migration_failed`.
    pub async fn prepare(&self) -> AppResult<()> {
        run_ddl(&self.pool, PREPARE_LOCK, HANDOVER_DDL).await
    }

    /// Stores a hand-over: `digest` (64 hex characters), the payload text and the expiry (unix
    /// seconds). Expired hand-overs of the tenant are dropped first; a failure of that housekeeping
    /// does not fail the call.
    ///
    /// # Errors
    /// [`HandoverUnavailable`]; nothing was stored.
    pub async fn issue(
        &self,
        digest: &str,
        payload: &str,
        now: i64,
        expires_at: i64,
    ) -> Result<(), HandoverUnavailable> {
        let conn = self.pool.get().await.map_err(|_| HandoverUnavailable)?;
        let _ = conn.execute(PRUNE, &[&self.tenant, &now]).await;
        conn.execute(ISSUE, &[&self.tenant, &digest, &payload, &expires_at])
            .await
            .map(|_| ())
            .map_err(|_| HandoverUnavailable)
    }

    /// Redeems the hand-over of `digest` at `now`: its payload text, once. `None` for an unknown,
    /// used or expired one (one answer).
    ///
    /// # Errors
    /// [`HandoverUnavailable`]; the hand-over is then still redeemable.
    pub async fn redeem(
        &self,
        digest: &str,
        now: i64,
    ) -> Result<Option<String>, HandoverUnavailable> {
        let conn = self.pool.get().await.map_err(|_| HandoverUnavailable)?;
        let row = conn
            .query_opt(REDEEM, &[&self.tenant, &digest, &now])
            .await
            .map_err(|_| HandoverUnavailable)?;
        row.map(|r| r.try_get::<_, String>(0).map_err(|_| HandoverUnavailable))
            .transpose()
    }

    /// Records a session the host started through `provider`.
    ///
    /// # Errors
    /// [`HandoverUnavailable`].
    pub async fn index_session(
        &self,
        session: &str,
        user: &str,
        provider: &str,
        sid: Option<&str>,
        now: i64,
    ) -> Result<(), HandoverUnavailable> {
        let conn = self.pool.get().await.map_err(|_| HandoverUnavailable)?;
        conn.execute(
            INDEX,
            &[&self.tenant, &session, &user, &provider, &sid, &now],
        )
        .await
        .map(|_| ())
        .map_err(|_| HandoverUnavailable)
    }

    /// The sessions that began through `provider` with IdP session id `sid` (newest first, at most
    /// `limit`): `(session id, user id)`.
    ///
    /// # Errors
    /// [`HandoverUnavailable`].
    pub async fn by_sid(
        &self,
        provider: &str,
        sid: &str,
        limit: i64,
    ) -> Result<Vec<(String, String)>, HandoverUnavailable> {
        let conn = self.pool.get().await.map_err(|_| HandoverUnavailable)?;
        let rows = conn
            .query(BY_SID, &[&self.tenant, &provider, &sid, &limit])
            .await
            .map_err(|_| HandoverUnavailable)?;
        pairs(&rows)
    }

    /// The sessions of `user` that began through `provider` (newest first, at most `limit`).
    ///
    /// # Errors
    /// [`HandoverUnavailable`].
    pub async fn by_user(
        &self,
        provider: &str,
        user: &str,
        limit: i64,
    ) -> Result<Vec<(String, String)>, HandoverUnavailable> {
        let conn = self.pool.get().await.map_err(|_| HandoverUnavailable)?;
        let rows = conn
            .query(BY_USER, &[&self.tenant, &provider, &user, &limit])
            .await
            .map_err(|_| HandoverUnavailable)?;
        pairs(&rows)
    }

    /// Drops an index row (the session ended).
    ///
    /// # Errors
    /// [`HandoverUnavailable`].
    pub async fn forget(&self, session: &str) -> Result<(), HandoverUnavailable> {
        let conn = self.pool.get().await.map_err(|_| HandoverUnavailable)?;
        conn.execute(FORGET, &[&self.tenant, &session])
            .await
            .map(|_| ())
            .map_err(|_| HandoverUnavailable)
    }
}

fn pairs(rows: &[tokio_postgres::Row]) -> Result<Vec<(String, String)>, HandoverUnavailable> {
    rows.iter()
        .map(|r| {
            Ok((
                r.try_get(0).map_err(|_| HandoverUnavailable)?,
                r.try_get(1).map_err(|_| HandoverUnavailable)?,
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_statement_names_the_tenant() {
        for sql in [PRUNE, ISSUE, REDEEM, INDEX, BY_SID, BY_USER, FORGET] {
            assert!(
                sql.contains("tenant = $1") || sql.contains("(tenant,"),
                "{sql}"
            );
        }
    }
}
