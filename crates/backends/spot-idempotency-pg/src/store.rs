//! [`PgIdempotencyStore`]: the PostgreSQL [`IdempotencyStore`], over `spot_idempotency_key`.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use deadpool_postgres::{Object, Pool};
use spot_app_api::AppResult;
use spot_core::Clock;
use spot_http::api::{
    IdempotencyError, IdempotencyStore, KeyScope, Reservation, StoreFuture, StoredResponse,
};

use crate::config::{ConfigError, PgIdempotencyConfig};
use crate::error::unavailable;
use crate::sql;

/// The response headers the store keeps, whatever a caller hands it: `set-cookie`,
/// `authorization` and `idempotency-replayed` are never written.
const KEPT_HEADERS: [&str; 4] = ["etag", "location", "content-type", "retry-after"];

/// How many times `reserve` re-reads a row that a prune deleted between its claim and its read.
const READ_ATTEMPTS: usize = 3;

/// The first and the longest pause between two polls of a waiting duplicate.
const POLL_FIRST: Duration = Duration::from_millis(100);
const POLL_LONGEST: Duration = Duration::from_millis(500);

/// The idempotency store in PostgreSQL.
///
/// See the crate documentation for the rules it keeps. Call [`PgIdempotencyStore::prepare`] once
/// at start-up; clones share the pool, and a restart is a new store over the same database, so it
/// sees every reservation and response the previous process made.
#[derive(Clone)]
pub struct PgIdempotencyStore {
    pool: Pool,
    clock: Arc<dyn Clock>,
    config: PgIdempotencyConfig,
}

impl std::fmt::Debug for PgIdempotencyStore {
    /// Names the settings, never a pool, connection or key.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgIdempotencyStore")
            .field("lease_secs", &self.config.lease.as_secs())
            .field("wait_budget_secs", &self.config.wait_budget.as_secs())
            .field("max_body_bytes", &self.config.max_body_bytes)
            .finish_non_exhaustive()
    }
}

impl PgIdempotencyStore {
    /// A store on `pool`, the site's own pool, timed by `clock`. It does not touch the database
    /// until used.
    ///
    /// # Errors
    /// [`ConfigError`] for a lease below 30 s, a zero wait budget or operation timeout, or a body
    /// cap outside `1..=1 MiB`. (A retention policy is validated when it is built.)
    pub fn new(
        pool: Pool,
        clock: Arc<dyn Clock>,
        config: PgIdempotencyConfig,
    ) -> Result<Self, ConfigError> {
        config.validate()?;
        Ok(Self {
            pool,
            clock,
            config,
        })
    }

    /// The configuration this store runs with.
    #[must_use]
    pub fn config(&self) -> &PgIdempotencyConfig {
        &self.config
    }

    /// Creates the table and its indexes if missing. Idempotent; safe from several nodes at once.
    ///
    /// # Errors
    /// `idempotency_pg.migration_failed` when the database cannot apply the DDL.
    pub async fn prepare(&self) -> AppResult<()> {
        crate::schema::prepare(&self.pool).await
    }

    /// The caller's Unix time. Every statement takes its time from here, never from the system
    /// clock or from `now()` in SQL.
    fn now(&self) -> i64 {
        self.clock.now_utc().unix_timestamp()
    }

    fn lease_until(&self, now: i64) -> i64 {
        now.saturating_add(i64::try_from(self.config.lease.as_secs()).unwrap_or(i64::MAX))
    }

    /// Runs `work` with the operation timeout: a database that stops answering is
    /// "unavailable", not a request held open.
    async fn bounded<T>(
        &self,
        work: impl Future<Output = Result<T, IdempotencyError>>,
    ) -> Result<T, IdempotencyError> {
        match tokio::time::timeout(self.config.operation_timeout, work).await {
            Ok(result) => result,
            Err(_) => Err(unavailable()),
        }
    }

    async fn conn(&self) -> Result<Object, IdempotencyError> {
        self.pool.get().await.map_err(|_| unavailable())
    }

    async fn reserve_inner(
        &self,
        scope: &KeyScope,
        fingerprint: &str,
    ) -> Result<Reservation, IdempotencyError> {
        check_scope(scope)?;
        check_fingerprint(fingerprint)?;
        let conn = self.conn().await?;
        for _ in 0..READ_ATTEMPTS {
            let now = self.now();
            let lease_until = self.lease_until(now);
            let claimed = conn
                .query_opt(
                    sql::RESERVE,
                    &[
                        &scope.site,
                        &scope.credential,
                        &scope.key,
                        &fingerprint,
                        &lease_until,
                        &now,
                    ],
                )
                .await
                .map_err(|_| unavailable())?;
            if claimed.is_some() {
                return Ok(Reservation::Fresh);
            }
            let row = conn
                .query_opt(sql::READ, &[&scope.site, &scope.credential, &scope.key])
                .await
                .map_err(|_| unavailable())?;
            let Some(row) = row else {
                // A prune removed the row between the claim and the read: claim again.
                continue;
            };
            let held: String = row.try_get(0).map_err(|_| unavailable())?;
            let state: String = row.try_get(1).map_err(|_| unavailable())?;
            let expires_at: Option<i64> = row.try_get(5).map_err(|_| unavailable())?;
            let lease_expires_at: Option<i64> = row.try_get(6).map_err(|_| unavailable())?;
            let lapsed = match state.as_str() {
                "done" => expires_at.is_none_or(|end| end <= now),
                "pending" => lease_expires_at.is_none_or(|end| end <= now),
                _ => return Err(unavailable()),
            };
            if lapsed {
                // The row ended between the claim and the read (another node's clock ran
                // ahead): claim again.
                continue;
            }
            if held != fingerprint {
                return Ok(Reservation::Conflict);
            }
            return if state == "pending" {
                Ok(Reservation::Pending)
            } else {
                let status: Option<i32> = row.try_get(2).map_err(|_| unavailable())?;
                let headers: Option<String> = row.try_get(3).map_err(|_| unavailable())?;
                let body: Option<Vec<u8>> = row.try_get(4).map_err(|_| unavailable())?;
                stored_response(status, headers, body).map(Reservation::Completed)
            };
        }
        Err(unavailable())
    }

    async fn complete_inner(
        &self,
        scope: &KeyScope,
        fingerprint: &str,
        response: StoredResponse,
        class: &str,
    ) -> Result<(), IdempotencyError> {
        check_scope(scope)?;
        check_fingerprint(fingerprint)?;
        let conn = self.conn().await?;
        if response.body.len() > self.config.max_body_bytes {
            // Not written: the reservation is released, so a retry runs again.
            conn.execute(
                sql::FAIL,
                &[&scope.site, &scope.credential, &scope.key, &fingerprint],
            )
            .await
            .map_err(|_| unavailable())?;
            return Ok(());
        }
        let status = i32::from(response.status);
        let headers = headers_json(&response.headers);
        let expires_at = self.config.retention.retain_until(class, self.now());
        conn.execute(
            sql::COMPLETE,
            &[
                &scope.site,
                &scope.credential,
                &scope.key,
                &fingerprint,
                &status,
                &headers,
                &response.body,
                &expires_at,
            ],
        )
        .await
        .map_err(|_| unavailable())?;
        Ok(())
    }

    async fn poll_once(&self, scope: &KeyScope) -> Result<bool, IdempotencyError> {
        let conn = self.conn().await?;
        let row = conn
            .query_opt(sql::POLL, &[&scope.site, &scope.credential, &scope.key])
            .await
            .map_err(|_| unavailable())?;
        let Some(row) = row else {
            return Ok(false);
        };
        let state: String = row.try_get(0).map_err(|_| unavailable())?;
        let lease_expires_at: Option<i64> = row.try_get(1).map_err(|_| unavailable())?;
        // A pending row whose lease ended is free to claim: nothing is running, nothing to wait
        // for.
        Ok(state == "pending" && lease_expires_at.is_some_and(|end| end > self.now()))
    }

    /// Deletes at most `limit` rows that are finished (their `expires_at` is at or before now) or
    /// abandoned (a pending lease that ended more than the policy's maximum retention ago), and
    /// returns how many were deleted.
    ///
    /// Bounded on purpose: a host job calls it in a loop, so no single statement locks an
    /// unbounded number of rows. The store runs no timer of its own (ADR-082 open question 4).
    /// Pruning only removes rows the contract already treats as free, so it cannot lose a
    /// reservation; the claim and the delete both go through row locks and the delete repeats
    /// its predicate (see `sql::PRUNE`).
    ///
    /// # Errors
    /// [`IdempotencyError::Unavailable`] when the database cannot answer. A host that is only
    /// tidying up logs it and tries again later; it must not read it as "pruned".
    pub async fn prune_expired(&self, limit: u32) -> Result<u64, IdempotencyError> {
        self.bounded(async {
            let conn = self.conn().await?;
            let now = self.now();
            let max =
                i64::try_from(self.config.retention.max_retention().as_secs()).unwrap_or(i64::MAX);
            let abandoned_before = now.saturating_sub(max);
            let limit = i64::from(limit);
            conn.execute(sql::PRUNE, &[&now, &abandoned_before, &limit])
                .await
                .map_err(|_| unavailable())
        })
        .await
    }
}

/// A scope the table's checks would refuse is refused here, before any SQL: fail closed.
fn check_scope(scope: &KeyScope) -> Result<(), IdempotencyError> {
    let key_ok = !scope.key.is_empty()
        && scope.key.len() <= 128
        && scope.key.bytes().all(|b| (0x21..=0x7e).contains(&b));
    let site_ok = !scope.site.is_empty() && scope.site.len() <= 256;
    let credential_ok = !scope.credential.is_empty() && scope.credential.len() <= 128;
    if key_ok && site_ok && credential_ok {
        Ok(())
    } else {
        Err(unavailable())
    }
}

/// A fingerprint is the router's SHA-256 hex; anything else is refused before any SQL.
fn check_fingerprint(fingerprint: &str) -> Result<(), IdempotencyError> {
    let ok = fingerprint.len() == 64
        && fingerprint
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if ok { Ok(()) } else { Err(unavailable()) }
}

/// The headers worth keeping, as a JSON array of `[name, value]` pairs.
fn headers_json(headers: &[(String, String)]) -> String {
    let kept: Vec<[&str; 2]> = headers
        .iter()
        .filter(|(name, _)| {
            KEPT_HEADERS
                .iter()
                .any(|keep| name.eq_ignore_ascii_case(keep))
        })
        .map(|(name, value)| [name.as_str(), value.as_str()])
        .collect();
    serde_json::to_string(&kept).unwrap_or_else(|_| "[]".to_owned())
}

/// A stored row as the response it replays; a row that does not hold one is unavailable (it is
/// never invented).
fn stored_response(
    status: Option<i32>,
    headers: Option<String>,
    body: Option<Vec<u8>>,
) -> Result<StoredResponse, IdempotencyError> {
    let status = status
        .and_then(|status| u16::try_from(status).ok())
        .ok_or_else(unavailable)?;
    let pairs: Vec<(String, String)> =
        serde_json::from_str::<Vec<(String, String)>>(&headers.ok_or_else(unavailable)?)
            .map_err(|_| unavailable())?;
    Ok(StoredResponse {
        status,
        headers: pairs,
        body: body.ok_or_else(unavailable)?,
    })
}

impl IdempotencyStore for PgIdempotencyStore {
    /// One statement decides: a returned row is [`Reservation::Fresh`]; otherwise the live row is
    /// read and answered by its fingerprint and state.
    fn reserve<'a>(
        &'a self,
        scope: &'a KeyScope,
        fingerprint: &'a str,
    ) -> StoreFuture<'a, Result<Reservation, IdempotencyError>> {
        Box::pin(self.bounded(self.reserve_inner(scope, fingerprint)))
    }

    fn complete<'a>(
        &'a self,
        scope: &'a KeyScope,
        fingerprint: &'a str,
        response: StoredResponse,
        class: &'a str,
    ) -> StoreFuture<'a, Result<(), IdempotencyError>> {
        Box::pin(self.bounded(self.complete_inner(scope, fingerprint, response, class)))
    }

    fn fail<'a>(&'a self, scope: &'a KeyScope, fingerprint: &'a str) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            if check_scope(scope).is_err() || check_fingerprint(fingerprint).is_err() {
                return;
            }
            // Best effort: a release that does not arrive is covered by the lease.
            let _ = self
                .bounded(async {
                    let conn = self.conn().await?;
                    conn.execute(
                        sql::FAIL,
                        &[&scope.site, &scope.credential, &scope.key, &fingerprint],
                    )
                    .await
                    .map_err(|_| unavailable())
                })
                .await;
        })
    }

    /// Re-reads the row by primary key every 100 ms, growing to 500 ms, until it is no longer
    /// pending or the wait budget is spent. There is no `LISTEN/NOTIFY` and no in-process wakeup:
    /// a wait is a re-read (ADR-082 section 3). The budget counts the pauses, so it needs no clock.
    fn wait_pending<'a>(
        &'a self,
        scope: &'a KeyScope,
    ) -> StoreFuture<'a, Result<(), IdempotencyError>> {
        Box::pin(async move {
            check_scope(scope)?;
            let mut pause = POLL_FIRST;
            let mut waited = Duration::ZERO;
            loop {
                if !self.bounded(self.poll_once(scope)).await? {
                    return Ok(());
                }
                if waited >= self.config.wait_budget {
                    return Err(IdempotencyError::unavailable());
                }
                let step = pause.min(self.config.wait_budget - waited);
                tokio::time::sleep(step).await;
                waited += step;
                pause = (pause + pause / 2).min(POLL_LONGEST);
            }
        })
    }

    fn prune<'a>(&'a self, limit: u32) -> StoreFuture<'a, Result<u64, IdempotencyError>> {
        Box::pin(self.prune_expired(limit))
    }

    fn max_body_bytes(&self) -> usize {
        self.config.max_body_bytes
    }
}
