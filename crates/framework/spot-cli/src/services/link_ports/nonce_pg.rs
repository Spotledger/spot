//! The PostgreSQL [`NonceStore`] of the external link: replay refusal that survives a restart and
//! holds across the nodes of a clustered site (one row per `(peer, nonce)`, the primary key
//! decides who claimed first).
//!
//! `claim` is synchronous (the trait is), and the inbound route calls it from the runtime, so it
//! blocks through `block_in_place` on a multi-thread runtime, through the stored handle on a plain
//! thread, and **refuses** (fail closed) on a current-thread runtime where blocking would stall
//! everything. A database that cannot answer is `NonceUnavailable`: the request is refused.

use deadpool_postgres::Pool;
use tokio::runtime::{Handle, RuntimeFlavor};

use crate::services::link_external::{NonceStore, NonceUnavailable};

/// How many expired rows one claim removes besides its own key.
const PURGE_BATCH: i64 = 50;

/// A nonce store in the tenant's PostgreSQL database.
pub struct PgNonceStore {
    pool: Pool,
    handle: Handle,
    table: String,
}

impl PgNonceStore {
    /// A store over `pool` whose table is `<prefix>_nonce`; `handle` is the runtime to block on
    /// from a plain thread.
    ///
    /// # Errors
    /// A message when `prefix` is not `[a-z][a-z0-9_]{0,40}`.
    pub fn new(pool: Pool, handle: Handle, prefix: &str) -> Result<Self, String> {
        let ok = !prefix.is_empty()
            && prefix.len() <= 41
            && prefix.starts_with(|c: char| c.is_ascii_lowercase())
            && prefix
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
        if !ok {
            return Err("the table prefix is not a plain identifier".to_owned());
        }
        Ok(Self {
            pool,
            handle,
            table: format!("{prefix}_nonce"),
        })
    }

    /// Creates the table (idempotent).
    ///
    /// # Errors
    /// A message when the database cannot be reached.
    pub async fn prepare(&self) -> Result<(), String> {
        let client = self
            .pool
            .get()
            .await
            .map_err(|_| "the database is not reachable".to_owned())?;
        client
            .batch_execute(&format!(
                "CREATE TABLE IF NOT EXISTS {t} (peer text NOT NULL, nonce text NOT NULL, \
                 expires_ms bigint NOT NULL, PRIMARY KEY (peer, nonce)); \
                 CREATE INDEX IF NOT EXISTS {t}_expiry ON {t} (expires_ms)",
                t = self.table
            ))
            .await
            .map_err(|_| "the nonce table could not be created".to_owned())
    }

    async fn claim_async(
        &self,
        peer: &str,
        nonce: &str,
        now_ms: i64,
        expires_ms: i64,
    ) -> Result<bool, NonceUnavailable> {
        let client = self.pool.get().await.map_err(|_| NonceUnavailable)?;
        // An expired claim of the same key is gone before the new one is tried; a few other
        // expired rows go with it so the table stays bounded by the window.
        client
            .execute(
                &format!(
                    "DELETE FROM {t} WHERE (peer = $1 AND nonce = $2 AND expires_ms <= $3) \
                     OR ctid IN (SELECT ctid FROM {t} WHERE expires_ms <= $3 LIMIT $4)",
                    t = self.table
                ),
                &[&peer, &nonce, &now_ms, &PURGE_BATCH],
            )
            .await
            .map_err(|_| NonceUnavailable)?;
        let inserted = client
            .execute(
                &format!(
                    "INSERT INTO {t} (peer, nonce, expires_ms) VALUES ($1, $2, $3) \
                     ON CONFLICT DO NOTHING",
                    t = self.table
                ),
                &[&peer, &nonce, &expires_ms],
            )
            .await
            .map_err(|_| NonceUnavailable)?;
        Ok(inserted == 1)
    }
}

impl NonceStore for PgNonceStore {
    fn claim(
        &self,
        peer: &str,
        nonce: &str,
        now_ms: u64,
        expires_ms: u64,
    ) -> Result<bool, NonceUnavailable> {
        let (Ok(now), Ok(expires)) = (i64::try_from(now_ms), i64::try_from(expires_ms)) else {
            return Err(NonceUnavailable);
        };
        let work = self.claim_async(peer, nonce, now, expires);
        match Handle::try_current() {
            Ok(current) if current.runtime_flavor() == RuntimeFlavor::MultiThread => {
                tokio::task::block_in_place(|| current.block_on(work))
            }
            Ok(_) => Err(NonceUnavailable),
            Err(_) => self.handle.block_on(work),
        }
    }
}
