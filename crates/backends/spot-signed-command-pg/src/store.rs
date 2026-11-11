//! [`PgNonceStore`]: the durable, fail-closed nonce window.

use std::fmt;

use deadpool_postgres::Pool;
use spot_signed_command::{DEFAULT_MAX_SKEW, KeyId, NonceFuture, NonceStore, StoreError};

use crate::error::SetupError;
use crate::schema;

/// The atomic claim: a returned row means the pair is fresh. `$4` is `now - skew`; a pair whose
/// `expires_at` is below it has run out its window and may be claimed again. Fixed text, bound
/// parameters only.
const CLAIM: &str = "INSERT INTO spot_signed_command_nonce (key_id, nonce, expires_at) \
    VALUES ($1, $2, $3) \
    ON CONFLICT (key_id, nonce) DO UPDATE SET expires_at = EXCLUDED.expires_at \
    WHERE spot_signed_command_nonce.expires_at < $4 \
    RETURNING 1";

/// Deletes up to `$2` expired rows (`expires_at < $1`).
const PRUNE: &str = "DELETE FROM spot_signed_command_nonce WHERE ctid IN \
    (SELECT ctid FROM spot_signed_command_nonce WHERE expires_at < $1 LIMIT $2)";

/// How many rows one `prune` statement deletes at most.
pub const PRUNE_BATCH: i64 = 1_000;

/// The PostgreSQL nonce store. Cheap to clone the pool into; share one store per process.
pub struct PgNonceStore {
    pool: Pool,
    skew: i64,
}

impl PgNonceStore {
    /// A store over `pool` with the default skew ([`DEFAULT_MAX_SKEW`]).
    #[must_use]
    pub fn new(pool: Pool) -> Self {
        Self::with_skew(pool, DEFAULT_MAX_SKEW)
    }

    /// A store with an explicit skew. It must match the verifier policy's skew, as for the memory
    /// store: a live pair is kept until `expires_at + skew`.
    #[must_use]
    pub fn with_skew(pool: Pool, skew: i64) -> Self {
        Self { pool, skew }
    }

    /// Creates the table (idempotent, serialised across nodes).
    ///
    /// # Errors
    /// `signed_command_pg.migration_failed` when the database cannot apply the DDL.
    pub async fn prepare(&self) -> Result<(), SetupError> {
        schema::prepare(&self.pool).await
    }

    /// Deletes rows whose window has run out (`expires_at + skew < now`), in statements of at
    /// most [`PRUNE_BATCH`] rows, and returns how many were deleted. A live row is never
    /// touched. Time is an argument; the host decides when to call this.
    ///
    /// # Errors
    /// [`StoreError::Unavailable`] when a statement fails.
    pub async fn prune(&self, now: i64) -> Result<u64, StoreError> {
        let horizon = now.saturating_sub(self.skew);
        let client = self.pool.get().await.map_err(|_| StoreError::Unavailable)?;
        let mut total = 0u64;
        loop {
            let deleted = client
                .execute(PRUNE, &[&horizon, &PRUNE_BATCH])
                .await
                .map_err(|_| StoreError::Unavailable)?;
            total = total.saturating_add(deleted);
            if deleted < PRUNE_BATCH.unsigned_abs() {
                return Ok(total);
            }
        }
    }

    async fn claim(
        &self,
        key: &KeyId,
        nonce: [u8; 16],
        expires_at: i64,
        now: i64,
    ) -> Result<bool, StoreError> {
        let horizon = now.saturating_sub(self.skew);
        let client = self.pool.get().await.map_err(|_| StoreError::Unavailable)?;
        let nonce_bytes: &[u8] = &nonce;
        let key_bytes: &[u8] = key.as_str().as_bytes();
        let row = client
            .query_opt(CLAIM, &[&key_bytes, &nonce_bytes, &expires_at, &horizon])
            .await
            .map_err(|_| StoreError::Unavailable)?;
        Ok(row.is_some())
    }
}

impl NonceStore for PgNonceStore {
    fn check_and_insert<'a>(
        &'a self,
        key: &'a KeyId,
        nonce: [u8; 16],
        expires_at: i64,
        now: i64,
    ) -> NonceFuture<'a, Result<bool, StoreError>> {
        Box::pin(self.claim(key, nonce, expires_at, now))
    }
}

impl fmt::Debug for PgNonceStore {
    /// Shows the skew only: no connection detail, key or nonce.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PgNonceStore")
            .field("skew", &self.skew)
            .finish()
    }
}
