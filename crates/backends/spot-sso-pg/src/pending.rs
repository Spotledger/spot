//! [`PgPendingStore`]: the PostgreSQL [`PendingStore`], over `spot_sso_pending` (WP-IAM-05.T9).
//!
//! In a cluster the browser's callback can land on a node other than the one that started the
//! flow, and a restart must not lose the flows in progress. The pending record is the CSRF and
//! replay defence of the whole sign-in, so the store keeps it as strictly as it can:
//!
//! | Column | Holds |
//! | --- | --- |
//! | `tenant` | the tenant the store serves; **in every predicate** of every statement |
//! | `binding` | the HMAC-SHA256 of the tenant and the browser binding, never the binding itself |
//! | `provider` | the provider id (not secret), for operators |
//! | `sealed` | the whole [`PendingAuth`] (state, nonce, PKCE verifier) sealed with AES-256-GCM, bound to the tenant and the row |
//! | `expires_at` | when the entry stops being valid (Unix seconds) |
//!
//! Rules it keeps:
//!
//! - **`take` is one statement**: `DELETE … WHERE tenant = $1 AND binding = $2 RETURNING …`. Of
//!   any number of nodes racing for one binding, the database hands the row to exactly one.
//! - **Expired means gone**: a row whose `expires_at` is at or before the caller's `now` is
//!   removed by the take that finds it and answers `None`.
//! - **Tenant isolation**: another tenant's binding digests to another key *and* is filtered by
//!   tenant, so a cross-tenant binding answers exactly like a missing one.
//! - **Fails closed**: any pool or database failure, and any row that does not open, is
//!   [`PendingError`] with a fixed sentence. No driver text, binding, state, nonce or verifier is
//!   carried or logged.
//! - **Time is an argument**; nothing reads a clock or entropy (the nonce source is injected into
//!   the sealer).
//! - **[`PgPendingStore::prune`] is bounded** and safe to run concurrently.

use std::sync::Arc;

use deadpool_postgres::{Object, Pool};
use ring::hmac;
use spot_app_api::AppResult;
use spot_sso::{PendingAuth, PendingError, PendingStore, StoreFuture};

use crate::error::{migration, migration_pool};
use crate::seal::{KEY_LEN, Sealer, aad, binding_digest};

/// The most bytes a sealed record may take (the schema enforces it too).
const MAX_SEALED: usize = 8192;
/// The lock key that serialises concurrent `prepare` calls (transaction scoped).
const PREPARE_LOCK: i64 = 0x5353_4f50_0002;

/// The idempotent DDL of the pending table. Every statement can run again.
pub const PENDING_DDL: &str = r"
CREATE TABLE IF NOT EXISTS spot_sso_pending (
    tenant     text NOT NULL,
    binding    bytea NOT NULL,
    provider   text NOT NULL,
    sealed     bytea NOT NULL,
    expires_at bigint NOT NULL,
    PRIMARY KEY (tenant, binding),
    CONSTRAINT spot_sso_pending_keys CHECK (
        octet_length(tenant) BETWEEN 1 AND 128
        AND octet_length(binding) = 32
        AND octet_length(provider) BETWEEN 1 AND 128
        AND octet_length(sealed) BETWEEN 1 AND 8192)
);

CREATE INDEX IF NOT EXISTS spot_sso_pending_expires_idx ON spot_sso_pending (expires_at);
";

/// Stores (or replaces) one entry. `$1` tenant, `$2` binding digest, `$3` provider, `$4` sealed
/// record, `$5` expiry.
const PUT: &str = "INSERT INTO spot_sso_pending (tenant, binding, provider, sealed, expires_at) \
    VALUES ($1, $2, $3, $4, $5) \
    ON CONFLICT (tenant, binding) DO UPDATE \
    SET provider = EXCLUDED.provider, sealed = EXCLUDED.sealed, expires_at = EXCLUDED.expires_at";

/// Removes and returns the entry of `$2` under tenant `$1`, atomically.
const TAKE: &str = "DELETE FROM spot_sso_pending \
    WHERE tenant = $1 AND binding = $2 \
    RETURNING sealed, expires_at";

/// Deletes at most `$2` rows expired at or before `$1`. The repeated outer predicate is
/// load-bearing for the same reason as in the replay store's prune: a concurrent `put` that
/// re-records the binding must not lose its row to a prune that selected it earlier.
const PRUNE: &str = "DELETE FROM spot_sso_pending \
    WHERE expires_at <= $1 AND (tenant, binding) IN ( \
        SELECT tenant, binding FROM spot_sso_pending WHERE expires_at <= $1 LIMIT $2)";

/// The pending-authorisation store in PostgreSQL.
///
/// Call [`PgPendingStore::prepare`] once at start-up. Clones share the pool and the keys; a
/// restart is a new store over the same database, tenant and keys, so it sees every entry the
/// previous process made.
#[derive(Clone)]
pub struct PgPendingStore {
    pool: Pool,
    tenant: String,
    digest_key: hmac::Key,
    sealer: Arc<dyn Sealer>,
}

impl std::fmt::Debug for PgPendingStore {
    /// Names the tenant, never a pool, key or connection detail.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgPendingStore")
            .field("tenant", &self.tenant)
            .finish_non_exhaustive()
    }
}

impl PgPendingStore {
    /// A store on `pool` for `tenant`. `digest_key` is the 32-byte HMAC key of the binding
    /// digest and `sealer` seals the record; both come from `spot-secrets`. Returns `None`
    /// when the tenant is empty or longer than 128 bytes, or the key is not 32 bytes.
    #[must_use]
    pub fn new(
        pool: Pool,
        tenant: impl Into<String>,
        digest_key: &[u8],
        sealer: Arc<dyn Sealer>,
    ) -> Option<Self> {
        let tenant = tenant.into();
        if tenant.is_empty() || tenant.len() > 128 || digest_key.len() != KEY_LEN {
            return None;
        }
        Some(Self {
            pool,
            tenant,
            digest_key: hmac::Key::new(hmac::HMAC_SHA256, digest_key),
            sealer,
        })
    }

    /// The tenant this store serves.
    #[must_use]
    pub fn tenant(&self) -> &str {
        &self.tenant
    }

    /// Creates the table and its index if missing. Idempotent; safe from several nodes at once.
    ///
    /// # Errors
    /// `sso_pg.migration_failed` when the database cannot apply the DDL.
    pub async fn prepare(&self) -> AppResult<()> {
        let mut conn = self.pool.get().await.map_err(|e| migration_pool(&e))?;
        let tx = conn.transaction().await.map_err(|e| migration(&e))?;
        tx.execute("SELECT pg_advisory_xact_lock($1)", &[&PREPARE_LOCK])
            .await
            .map_err(|e| migration(&e))?;
        tx.batch_execute(PENDING_DDL)
            .await
            .map_err(|e| migration(&e))?;
        tx.commit().await.map_err(|e| migration(&e))
    }

    /// Deletes at most `limit` rows (of any tenant) expired at or before `now` and returns how
    /// many. Bounded so a cleanup job never holds a long lock; an expired row is unusable by
    /// every tenant, so removing it loses nothing.
    ///
    /// # Errors
    /// [`PendingError`] when the database cannot answer; a tidy-up job may retry later.
    pub async fn prune(&self, now: i64, limit: u32) -> Result<u64, PendingError> {
        let conn = self.conn().await?;
        let limit = i64::from(limit);
        conn.execute(PRUNE, &[&now, &limit])
            .await
            .map_err(|_| PendingError::unavailable())
    }

    async fn conn(&self) -> Result<Object, PendingError> {
        self.pool
            .get()
            .await
            .map_err(|_| PendingError::unavailable())
    }

    async fn put_entry(&self, binding: &str, pending: &PendingAuth) -> Result<(), PendingError> {
        let digest = binding_digest(&self.digest_key, &self.tenant, binding);
        let plain = serde_json::to_vec(pending).map_err(|_| PendingError::unavailable())?;
        let sealed = self
            .sealer
            .seal(&aad(&self.tenant, &digest), &plain)
            .filter(|sealed| sealed.len() <= MAX_SEALED)
            .ok_or_else(PendingError::unavailable)?;
        let provider = pending.provider.as_str();
        let digest_ref: &[u8] = &digest;
        let sealed_ref: &[u8] = &sealed;
        let conn = self.conn().await?;
        conn.execute(
            PUT,
            &[
                &self.tenant,
                &digest_ref,
                &provider,
                &sealed_ref,
                &pending.expires_at,
            ],
        )
        .await
        .map(|_| ())
        .map_err(|_| PendingError::unavailable())
    }

    async fn take_entry(
        &self,
        binding: &str,
        now: i64,
    ) -> Result<Option<PendingAuth>, PendingError> {
        let digest = binding_digest(&self.digest_key, &self.tenant, binding);
        let digest_ref: &[u8] = &digest;
        let conn = self.conn().await?;
        let row = conn
            .query_opt(TAKE, &[&self.tenant, &digest_ref])
            .await
            .map_err(|_| PendingError::unavailable())?;
        let Some(row) = row else {
            return Ok(None);
        };
        let sealed: Vec<u8> = row.try_get(0).map_err(|_| PendingError::unavailable())?;
        let expires_at: i64 = row.try_get(1).map_err(|_| PendingError::unavailable())?;
        if now >= expires_at {
            // Removed by the delete above; an expired entry is simply not there.
            return Ok(None);
        }
        let plain = self
            .sealer
            .open(&aad(&self.tenant, &digest), &sealed)
            .ok_or_else(PendingError::unavailable)?;
        serde_json::from_slice(&plain)
            .map(Some)
            .map_err(|_| PendingError::unavailable())
    }
}

impl PendingStore for PgPendingStore {
    fn put<'a>(
        &'a self,
        binding: &'a str,
        pending: &'a PendingAuth,
    ) -> StoreFuture<'a, Result<(), PendingError>> {
        Box::pin(self.put_entry(binding, pending))
    }

    fn take<'a>(
        &'a self,
        binding: &'a str,
        now: i64,
    ) -> StoreFuture<'a, Result<Option<PendingAuth>, PendingError>> {
        Box::pin(self.take_entry(binding, now))
    }
}
