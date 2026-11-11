//! [`PgJobStore`]: the PostgreSQL implementation of the job store.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use deadpool_postgres::{Object, Pool};
use spot_jobs::model::InsertResult;
use spot_jobs::store::{ClaimedJob, JobFilter};
use spot_jobs::{Claim, Error, Job, JobId, JobState, JobStore, NewJob, Result};
use spot_storage_contract::feed::FeedPosition;
use time::OffsetDateTime;

use crate::BATCH;
use crate::convert::{id_from_db, id_to_db, job_from_row, millis, pg_err, pool_err, state_text};
use crate::schema;
use crate::sql::{self, JOB_COLUMNS};

type Fut<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

/// Key of the transaction-scoped advisory lock that serialises claims while a per-site cap is set.
const CLAIM_LOCK: i64 = 0x5107_c1a1;

/// The default `max_attempts` of a new job (the default queue policy's).
pub const DEFAULT_MAX_ATTEMPTS: u32 = 3;

/// The default delay before a job whose worker was lost may run again.
pub const DEFAULT_REAP_RETRY_DELAY: Duration = Duration::from_secs(5);

/// The intake name used by [`PgJobStore::new`].
pub const DEFAULT_INTAKE: &str = "default";

/// A durable job store on PostgreSQL. Cheap to clone; clones share the pool.
///
/// See the crate documentation for the rules it keeps. Call [`PgJobStore::prepare`] once at
/// start-up.
#[derive(Clone)]
pub struct PgJobStore {
    pub(crate) pool: Pool,
    max_attempts: u32,
    max_running_per_site: Option<u32>,
    reap_retry_delay: Duration,
    intake: String,
}

impl std::fmt::Debug for PgJobStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgJobStore")
            .field("max_attempts", &self.max_attempts)
            .field("max_running_per_site", &self.max_running_per_site)
            .finish_non_exhaustive()
    }
}

impl PgJobStore {
    /// A store on `pool`. It does not touch the database until used.
    pub fn new(pool: Pool) -> Self {
        Self {
            pool,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            max_running_per_site: None,
            reap_retry_delay: DEFAULT_REAP_RETRY_DELAY,
            intake: DEFAULT_INTAKE.to_owned(),
        }
    }

    /// `max_attempts` given to jobs inserted from now on (at least 1).
    pub fn with_max_attempts(mut self, max_attempts: u32) -> Self {
        self.max_attempts = max_attempts.max(1);
        self
    }

    /// Never run more than `cap` jobs of one site at once (see the crate documentation).
    pub fn with_max_running_per_site(mut self, cap: u32) -> Self {
        self.max_running_per_site = Some(cap);
        self
    }

    /// How long after its lease was reaped a job may run again (database clock).
    pub fn with_reap_retry_delay(mut self, delay: Duration) -> Self {
        self.reap_retry_delay = delay;
        self
    }

    /// The name under which this store keeps its outbox intake position. Stores that read
    /// different outboxes use different names.
    pub fn with_intake_name(mut self, name: impl Into<String>) -> Self {
        self.intake = name.into();
        self
    }

    /// Creates the tables and indexes if missing. Idempotent; see [`schema`].
    pub async fn prepare(&self) -> Result<()> {
        schema::prepare(&self.pool).await
    }

    pub(crate) async fn conn(&self) -> Result<Object> {
        self.pool.get().await.map_err(pool_err)
    }

    /// Returns the work of every job whose lease has lapsed according to the **database clock**
    /// alone. Production schedulers call this; see [`JobStore::reap_expired`] for the variant
    /// that accepts a caller's clock.
    pub async fn reap_lapsed(&self) -> Result<Vec<JobId>> {
        self.reap(None).await
    }

    async fn reap(&self, caller_now: Option<OffsetDateTime>) -> Result<Vec<JobId>> {
        let delay = millis(self.reap_retry_delay);
        let mut reaped = Vec::new();
        let conn = self.conn().await?;
        loop {
            // GREATEST ignores NULL, so no caller clock means the database clock alone.
            let rows = conn
                .query(sql::REAP, &[&caller_now, &delay, &i64::from(BATCH)])
                .await
                .map_err(pg_err)?;
            let n = rows.len();
            for row in &rows {
                reaped.push(id_from_db(row.try_get(0).map_err(pg_err)?)?);
            }
            if n < BATCH as usize {
                return Ok(reaped);
            }
        }
    }

    async fn insert(&self, job: NewJob) -> Result<InsertResult> {
        if !Job::is_valid_name(&job.name) {
            return Err(Error::InvalidName);
        }
        if job.queue.is_empty() {
            return Err(Error::UnknownQueue);
        }
        let max_attempts = i32::try_from(self.max_attempts).unwrap_or(i32::MAX);
        let mut conn = self.conn().await?;
        let tx = conn.transaction().await.map_err(pg_err)?;
        if let Some(event) = &job.outbox_event_id {
            let seen = tx
                .query_opt(
                    "SELECT job_id FROM spot_job_outbox_seen WHERE event_id = $1",
                    &[event],
                )
                .await
                .map_err(pg_err)?;
            if let Some(row) = seen {
                return Ok(InsertResult::Duplicate(id_from_db(
                    row.try_get(0).map_err(pg_err)?,
                )?));
            }
        }
        // A live job with this key may finish between our insert and our lookup, so retry; the
        // loop ends because every pass either inserts or finds a live holder.
        let mut result = None;
        for _ in 0..5 {
            let inserted = tx
                .query_opt(
                    "INSERT INTO spot_job
                         (site, name, args, queue, state, max_attempts, not_before,
                          idempotency_key, context)
                     VALUES ($1, $2, $3, $4, 'pending', $5, $6, $7, $8)
                     ON CONFLICT (site, name, idempotency_key)
                         WHERE idempotency_key IS NOT NULL
                           AND state IN ('pending', 'running', 'retrying')
                     DO NOTHING
                     RETURNING id",
                    &[
                        &job.site.as_str(),
                        &job.name,
                        &job.args,
                        &job.queue,
                        &max_attempts,
                        &job.not_before,
                        &job.idempotency_key,
                        &job.context,
                    ],
                )
                .await
                .map_err(pg_err)?;
            if let Some(row) = inserted {
                result = Some(InsertResult::Inserted(id_from_db(
                    row.try_get(0).map_err(pg_err)?,
                )?));
                break;
            }
            let live = tx
                .query_opt(
                    sql::IDEMPOTENCY_LIVE,
                    &[&job.site.as_str(), &job.name, &job.idempotency_key],
                )
                .await
                .map_err(pg_err)?;
            if let Some(row) = live {
                result = Some(InsertResult::Duplicate(id_from_db(
                    row.try_get(0).map_err(pg_err)?,
                )?));
                break;
            }
        }
        let result =
            result.ok_or_else(|| Error::other("job store: idempotency key kept changing"))?;
        let job_id = match &result {
            InsertResult::Inserted(id) | InsertResult::Duplicate(id) => id_to_db(*id)?,
        };
        if let Some(event) = &job.outbox_event_id {
            // Recorded for duplicates too: a replay of this event must not create a second job
            // once the first one has finished and its key is free again.
            let fresh = tx
                .execute(
                    "INSERT INTO spot_job_outbox_seen (event_id, job_id) VALUES ($1, $2)
                     ON CONFLICT (event_id) DO NOTHING",
                    &[event, &job_id],
                )
                .await
                .map_err(pg_err)?;
            if fresh == 0 {
                // Another session recorded this event first (and has committed: the conflict
                // waited for it). Undo our job and answer with the first one.
                tx.rollback().await.map_err(pg_err)?;
                let row = conn
                    .query_one(
                        "SELECT job_id FROM spot_job_outbox_seen WHERE event_id = $1",
                        &[event],
                    )
                    .await
                    .map_err(pg_err)?;
                return Ok(InsertResult::Duplicate(id_from_db(
                    row.try_get(0).map_err(pg_err)?,
                )?));
            }
        }
        tx.commit().await.map_err(pg_err)?;
        Ok(result)
    }

    async fn claim_jobs(
        &self,
        queues: &[String],
        worker_id: &str,
        ttl: Duration,
        limit: u32,
    ) -> Result<Vec<ClaimedJob>> {
        if limit == 0 || queues.is_empty() {
            return Ok(Vec::new());
        }
        let ttl_ms = millis(ttl);
        let mut conn = self.conn().await?;
        let tx = conn.transaction().await.map_err(pg_err)?;
        if self.max_running_per_site.is_some() {
            tx.execute("SELECT pg_advisory_xact_lock($1)", &[&CLAIM_LOCK])
                .await
                .map_err(pg_err)?;
        }
        let mut claimed: Vec<ClaimedJob> = Vec::new();
        for queue in queues {
            let want = i64::from(limit) - i64::try_from(claimed.len()).unwrap_or(i64::MAX);
            if want <= 0 {
                break;
            }
            let rows = match self.max_running_per_site {
                None => {
                    // One statement: lock up to `want` claimable rows that nobody else holds,
                    // and lease them from the database clock.
                    tx.query(sql::CLAIM, &[queue, &worker_id, &ttl_ms, &want])
                        .await
                }
                Some(cap) => {
                    let cap = i64::from(cap);
                    tx.query(
                        sql::CLAIM_CAPPED,
                        &[queue, &worker_id, &ttl_ms, &want, &cap],
                    )
                    .await
                }
            }
            .map_err(pg_err)?;
            let mut batch = Vec::with_capacity(rows.len());
            for row in &rows {
                let job = job_from_row(row)?;
                let claim = job
                    .claim
                    .clone()
                    .ok_or_else(|| Error::other("job store: claimed row without a claim"))?;
                batch.push(ClaimedJob { job, claim });
            }
            batch.sort_by_key(|c| (c.job.not_before, c.job.id));
            claimed.extend(batch);
        }
        tx.commit().await.map_err(pg_err)?;
        Ok(claimed)
    }
}

fn to_db_position(p: FeedPosition) -> Result<(i64, i64)> {
    let g = i64::try_from(p.generation())
        .map_err(|_| Error::other("job store: generation does not fit the database"))?;
    let s = i64::try_from(p.seq())
        .map_err(|_| Error::other("job store: position does not fit the database"))?;
    Ok((g, s))
}

impl JobStore for PgJobStore {
    fn insert_if_new(&self, job: NewJob) -> Fut<'_, InsertResult> {
        Box::pin(self.insert(job))
    }

    /// Claims up to `limit` jobs, queue by queue in the given order. `now` is ignored: the
    /// lease and `not_before` use the database clock.
    fn claim(
        &self,
        queues: &[String],
        worker_id: &str,
        ttl: Duration,
        _now: OffsetDateTime,
        limit: u32,
    ) -> Fut<'_, Vec<ClaimedJob>> {
        let queues = queues.to_vec();
        let worker_id = worker_id.to_owned();
        Box::pin(async move { self.claim_jobs(&queues, &worker_id, ttl, limit).await })
    }

    /// Renews the lease from the database clock; `now` is ignored.
    fn heartbeat(&self, claim: &Claim, ttl: Duration, _now: OffsetDateTime) -> Fut<'_, ()> {
        let token = i64::try_from(claim.token.0);
        let worker = claim.worker.clone();
        let ttl_ms = millis(ttl);
        Box::pin(async move {
            let Ok(token) = token else {
                return Err(Error::ClaimLost);
            };
            let conn = self.conn().await?;
            let n = conn
                .execute(sql::HEARTBEAT, &[&token, &worker, &ttl_ms])
                .await
                .map_err(pg_err)?;
            if n == 1 {
                Ok(())
            } else {
                Err(Error::ClaimLost)
            }
        })
    }

    fn complete(
        &self,
        claim: &Claim,
        new_state: JobState,
        last_error: Option<String>,
    ) -> Fut<'_, ()> {
        let token = i64::try_from(claim.token.0);
        let worker = claim.worker.clone();
        Box::pin(async move {
            let (state, retry_at) = match &new_state {
                JobState::Pending | JobState::Running => {
                    return Err(Error::other(format!(
                        "job store: a job cannot be completed as {}",
                        state_text(&new_state)
                    )));
                }
                JobState::Retrying { at } => ("retrying", Some(*at)),
                other => (state_text(other), None),
            };
            let Ok(token) = token else {
                return Err(Error::ClaimLost);
            };
            let conn = self.conn().await?;
            let n = conn
                .execute(
                    sql::COMPLETE,
                    &[&token, &worker, &state, &retry_at, &last_error],
                )
                .await
                .map_err(pg_err)?;
            if n == 1 {
                Ok(())
            } else {
                Err(Error::ClaimLost)
            }
        })
    }

    /// Returns jobs whose lease lapsed at `max(database now, now)` to the queue (or to `Dead`
    /// when their attempts are used up). Production code uses [`PgJobStore::reap_lapsed`].
    fn reap_expired(&self, now: OffsetDateTime) -> Fut<'_, Vec<JobId>> {
        Box::pin(self.reap(Some(now)))
    }

    fn cancel(&self, id: JobId) -> Fut<'_, ()> {
        Box::pin(async move {
            let id = id_to_db(id)?;
            let conn = self.conn().await?;
            let n = conn
                .execute(
                    "UPDATE spot_job SET
                         state = 'cancelled', finished_at = now(),
                         claim_token = NULL, claim_worker = NULL, lease_expires_at = NULL
                     WHERE id = $1 AND state IN ('pending', 'retrying', 'running')",
                    &[&id],
                )
                .await
                .map_err(pg_err)?;
            if n == 1 {
                return Ok(());
            }
            // Already finished is fine (cancel is idempotent); unknown is an error.
            let exists = conn
                .query_opt("SELECT 1 FROM spot_job WHERE id = $1", &[&id])
                .await
                .map_err(pg_err)?;
            if exists.is_some() {
                Ok(())
            } else {
                Err(Error::other("Job not found"))
            }
        })
    }

    fn merge_context(
        &self,
        id: JobId,
        claim: &Claim,
        patch: serde_json::Map<String, serde_json::Value>,
    ) -> Fut<'_, serde_json::Value> {
        let token = i64::try_from(claim.token.0);
        let worker = claim.worker.clone();
        Box::pin(async move {
            let id = id_to_db(id)?;
            let Ok(token) = token else {
                return Err(Error::ClaimLost);
            };
            let mut conn = self.conn().await?;
            let tx = conn.transaction().await.map_err(pg_err)?;
            // The fence comes first (a stale claim is ClaimLost whatever the patch), then the
            // limits. The row lock is the fence and the serialiser in one: a stale claim finds no row,
            // and two merges queue behind each other, so neither loses a key.
            let row = tx
                .query_opt(sql::CONTEXT_LOCK, &[&id, &token, &worker])
                .await
                .map_err(pg_err)?;
            let Some(row) = row else {
                return Err(Error::ClaimLost);
            };
            let current: serde_json::Value = row.try_get(0).map_err(pg_err)?;
            let merged = spot_jobs::merge_context_value(&current, patch)?;
            tx.execute(sql::CONTEXT_WRITE, &[&id, &merged])
                .await
                .map_err(pg_err)?;
            tx.commit().await.map_err(pg_err)?;
            Ok(merged)
        })
    }

    fn get(&self, id: JobId) -> Fut<'_, Option<Job>> {
        Box::pin(async move {
            let id = id_to_db(id)?;
            let conn = self.conn().await?;
            let row = conn
                .query_opt(
                    &format!("SELECT {JOB_COLUMNS} FROM spot_job WHERE id = $1"),
                    &[&id],
                )
                .await
                .map_err(pg_err)?;
            row.as_ref().map(job_from_row).transpose()
        })
    }

    fn list(&self, filter: JobFilter) -> Fut<'_, Vec<Job>> {
        Box::pin(async move {
            let site = filter.site.as_ref().map(|s| s.as_str().to_owned());
            let state = filter.state.as_ref().map(|s| state_text(s).to_owned());
            let limit = filter.limit.map(i64::from);
            let conn = self.conn().await?;
            let rows = conn
                .query(
                    &format!(
                        "SELECT {JOB_COLUMNS} FROM spot_job
                         WHERE ($1::text IS NULL OR site = $1)
                           AND ($2::text IS NULL OR state = $2)
                           AND ($3::text IS NULL OR queue = $3)
                         ORDER BY id LIMIT $4::bigint"
                    ),
                    &[&site, &state, &filter.queue, &limit],
                )
                .await
                .map_err(pg_err)?;
            rows.iter().map(job_from_row).collect()
        })
    }

    fn intake_position(&self) -> Fut<'_, FeedPosition> {
        Box::pin(async move {
            let conn = self.conn().await?;
            let row = conn
                .query_opt(
                    "SELECT generation, seq FROM spot_job_intake WHERE site = $1",
                    &[&self.intake],
                )
                .await
                .map_err(pg_err)?;
            let Some(row) = row else {
                return Ok(FeedPosition::START);
            };
            let g: i64 = row.try_get(0).map_err(pg_err)?;
            let s: i64 = row.try_get(1).map_err(pg_err)?;
            Ok(FeedPosition::new(
                u64::try_from(g).unwrap_or(0),
                u64::try_from(s).unwrap_or(0),
            ))
        })
    }

    fn set_intake_position(&self, position: FeedPosition) -> Fut<'_, ()> {
        Box::pin(async move {
            let (g, s) = to_db_position(position)?;
            let conn = self.conn().await?;
            conn.execute(
                "INSERT INTO spot_job_intake (site, generation, seq) VALUES ($1, $2, $3)
                 ON CONFLICT (site) DO UPDATE
                     SET generation = EXCLUDED.generation, seq = EXCLUDED.seq, updated_at = now()",
                &[&self.intake, &g, &s],
            )
            .await
            .map_err(pg_err)?;
            Ok(())
        })
    }
}
