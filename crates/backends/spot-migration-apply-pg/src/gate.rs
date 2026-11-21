//! The admission gate every request path consults (ADR-22).
//!
//! The gate is one row in `spot_migration_control.gate`. Every request path asks
//! [`PgAdmissionGate::status`] (or [`PgAdmissionGate::problem`]) before it serves anything;
//! while the row says `closed`, a session or a write is refused with the
//! `server.maintenance` problem document.
//!
//! # Failing closed
//!
//! `status` is synchronous and answers from the last successful read. A gate that has never
//! been read, or whose last read failed, answers [`GateStatus::Closed`] with the
//! [`UNREADABLE`] reason. A request path that wants to be sure the answer is current calls
//! [`PgAdmissionGate::refresh`] (or [`PgAdmissionGate::refresh_if_stale`]) first; the host
//! runs [`PgAdmissionGate::refresh_if_stale`] on a short interval, and a failed refresh
//! leaves the gate closed until a later read succeeds.
//!
//! # Invalidation
//!
//! The row carries an `epoch` that every `close` and `open` increments. A second node sees a
//! changed epoch on its next refresh and re-reads immediately instead of waiting out the TTL.
//! When a [`Coordinator`] is wired, `close` and `open` also advance the cluster-wide counter
//! `migration-gate:<site>`; `refresh_if_stale` peeks that counter and treats a moved value as
//! a push invalidation. [`PgAdmissionGate::invalidate`] is the explicit seam a `LISTEN`
//! consumer or an administrative push calls.
//!
//! # The drain
//!
//! After closing, the engine calls [`AdmissionGate::drain`]. The gate polls
//! `pg_stat_activity` for other backends on the same database that hold a transaction and are
//! not the migration's own connection, until the count reaches zero or the deadline passes.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use deadpool_postgres::Pool;
use spot_coordinator::Coordinator;
use spot_core::{Clock, SiteId};
use spot_migration_apply::gate::{AdmissionGate, DrainReport, GateStatus};
use spot_migration_apply::{BoxFuture, Result as ApplyResult};

use crate::error::{Error, Result};
use crate::schema::{client, table};

/// The reason a gate reports while its state cannot be read.
pub const UNREADABLE: &str = "the maintenance gate state could not be read; the site is closed \
                              until it can be";

/// The application name of the migration's own connection, which the drain does not wait for.
const MIGRATION_APPLICATION: &str = "spot-migrate";

/// One cached read of the gate row.
#[derive(Clone, Debug)]
struct Cached {
    status: GateStatus,
    /// The row's epoch at the read, or the coordinator counter's value.
    epoch: i64,
    /// The injected monotonic instant of the read, in microseconds.
    read_at_micros: i64,
}

/// The maintenance gate over one site's control schema.
pub struct PgAdmissionGate {
    pool: Pool,
    site: SiteId,
    clock: Arc<dyn Clock>,
    ttl: Duration,
    coordinator: Option<Arc<dyn Coordinator>>,
    cache: Mutex<Option<Cached>>,
}

impl PgAdmissionGate {
    /// A gate for `site` over `pool`, reading at most once per `ttl`.
    ///
    /// It starts with no read, so [`AdmissionGate::status`] answers closed until the first
    /// [`PgAdmissionGate::refresh`].
    #[must_use]
    pub fn new(pool: Pool, site: SiteId, clock: Arc<dyn Clock>, ttl: Duration) -> Self {
        Self {
            pool,
            site,
            clock,
            ttl,
            coordinator: None,
            cache: Mutex::new(None),
        }
    }

    /// The same gate reporting its epoch through a cluster-wide coordinator counter.
    #[must_use]
    pub fn with_coordinator(mut self, coordinator: Arc<dyn Coordinator>) -> Self {
        self.coordinator = Some(coordinator);
        self
    }

    /// The site this gate guards.
    #[must_use]
    pub fn site(&self) -> &SiteId {
        &self.site
    }

    /// The cache's TTL.
    #[must_use]
    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    /// Reads the gate row and replaces the cached answer.
    ///
    /// A failed read closes the gate: the cached answer becomes [`GateStatus::Closed`] with
    /// [`UNREADABLE`] and the error is returned.
    ///
    /// # Errors
    /// [`Error::ControlUnavailable`] when the row cannot be read.
    pub async fn refresh(&self, _now: i64) -> Result<GateStatus> {
        match self.read_row().await {
            Ok((status, epoch)) => {
                self.store(status.clone(), epoch);
                Ok(status)
            }
            Err(error) => {
                self.store_closed(UNREADABLE.to_owned(), 0, i64::MIN);
                Err(error)
            }
        }
    }

    /// Refreshes when the cached answer is older than the TTL or the coordinator's epoch
    /// counter moved; otherwise answers from the cache.
    ///
    /// # Errors
    /// [`Error::ControlUnavailable`] when a needed read fails.
    pub async fn refresh_if_stale(&self, now: i64) -> Result<GateStatus> {
        let cached = self.cached();
        let fresh = cached.as_ref().is_some_and(|cached| {
            let age = self
                .clock
                .monotonic_micros()
                .saturating_sub(cached.read_at_micros);
            age < i64::try_from(self.ttl.as_micros()).unwrap_or(i64::MAX)
        });
        if fresh && !self.epoch_moved(cached.as_ref().map(|c| c.epoch)).await {
            return Ok(cached
                .map(|cached| cached.status)
                .unwrap_or_else(unreadable));
        }
        self.refresh(now).await
    }

    /// Forgets the cached answer. The next [`AdmissionGate::status`] reports the gate closed
    /// until a refresh succeeds; this is the push-invalidation seam.
    pub fn invalidate(&self) {
        *self
            .cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }

    /// Whether the cached answer is younger than the TTL.
    #[must_use]
    pub fn is_fresh(&self) -> bool {
        self.cached().is_some_and(|cached| {
            let age = self
                .clock
                .monotonic_micros()
                .saturating_sub(cached.read_at_micros);
            age < i64::try_from(self.ttl.as_micros()).unwrap_or(i64::MAX)
        })
    }

    /// The cached answer's age in microseconds, when there is one.
    #[must_use]
    pub fn age_micros(&self) -> Option<i64> {
        self.cached().map(|cached| {
            self.clock
                .monotonic_micros()
                .saturating_sub(cached.read_at_micros)
        })
    }

    fn cached(&self) -> Option<Cached> {
        self.cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn store(&self, status: GateStatus, epoch: i64) {
        *self
            .cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Cached {
            status,
            epoch,
            read_at_micros: self.clock.monotonic_micros(),
        });
    }

    fn store_closed(&self, reason: String, since: i64, epoch: i64) {
        self.store(GateStatus::Closed { reason, since }, epoch);
    }

    /// Whether the cluster-wide epoch counter moved away from `observed`.
    async fn epoch_moved(&self, observed: Option<i64>) -> bool {
        let (Some(coordinator), Some(observed)) = (&self.coordinator, observed) else {
            return false;
        };
        match coordinator.peek(&self.epoch_key()).await {
            Ok(current) => i64::try_from(current).unwrap_or(i64::MAX) != observed,
            // A counter that cannot be read is not evidence of a change: the TTL still bounds
            // staleness, and a failed row read is what closes the gate.
            Err(_) => false,
        }
    }

    fn epoch_key(&self) -> String {
        format!("migration-gate:{}", self.site)
    }

    /// Reads the gate row: `(status, epoch)`. A missing row is an open gate.
    async fn read_row(&self) -> Result<(GateStatus, i64)> {
        let client = client(&self.pool, "reading the gate").await?;
        let row = client
            .query_opt(
                &format!(
                    "SELECT state, reason, since, epoch FROM {} WHERE site = $1",
                    table("gate")
                ),
                &[&self.site.as_str()],
            )
            .await
            .map_err(|error| Error::database("reading the gate", &error))?;
        let Some(row) = row else {
            return Ok((GateStatus::Open, 0));
        };
        let state: String = row.get(0);
        let epoch: i64 = row.get(3);
        let status = if state == "closed" {
            GateStatus::Closed {
                reason: row.get(1),
                since: row.get(2),
            }
        } else {
            GateStatus::Open
        };
        Ok((status, epoch))
    }

    /// Writes the gate row and advances its epoch inside one transaction serialised against
    /// every other node by a transaction-scoped advisory lock; returns the new epoch.
    ///
    /// The lock is what makes a transition atomic across pooled connections and nodes: two
    /// `close`/`open` calls on different nodes cannot interleave their row write and epoch
    /// bump, so the epoch a reader sees always matches the row it describes.
    async fn write_row(&self, status: &GateStatus) -> Result<i64> {
        let mut client = client(&self.pool, "writing the gate").await?;
        let tx = client
            .transaction()
            .await
            .map_err(|error| Error::database("writing the gate", &error))?;
        tx.execute(
            "SELECT pg_advisory_xact_lock(hashtext($1))",
            &[&format!("spot-migration-apply-pg:gate:{}", self.site)],
        )
        .await
        .map_err(|error| Error::database("locking the gate", &error))?;
        let (state, reason, since) = match status {
            GateStatus::Open => ("open", String::new(), 0i64),
            GateStatus::Closed { reason, since } => ("closed", reason.clone(), *since),
        };
        let row = tx
            .query_one(
                &format!(
                    "INSERT INTO {table} (site, state, reason, since, epoch) \
                     VALUES ($1, $2, $3, $4, 1) \
                     ON CONFLICT (site) DO UPDATE SET \
                         state = EXCLUDED.state, reason = EXCLUDED.reason, \
                         since = EXCLUDED.since, epoch = {table}.epoch + 1 \
                     RETURNING epoch",
                    table = table("gate")
                ),
                &[&self.site.as_str(), &state, &reason, &since],
            )
            .await
            .map_err(|error| Error::database("writing the gate", &error))?;
        let epoch = row.get(0);
        tx.commit()
            .await
            .map_err(|error| Error::database("committing the gate", &error))?;
        Ok(epoch)
    }

    /// Advances the cluster-wide epoch counter, when a coordinator is wired.
    async fn bump_epoch(&self) {
        if let Some(coordinator) = &self.coordinator {
            let _ = coordinator.next(&self.epoch_key(), 1).await;
        }
    }
}

/// The answer for a gate that has never been read.
fn unreadable() -> GateStatus {
    GateStatus::Closed {
        reason: UNREADABLE.to_owned(),
        since: 0,
    }
}

impl AdmissionGate for PgAdmissionGate {
    fn close<'a>(&'a self, reason: &'a str, now: i64) -> BoxFuture<'a, ApplyResult<()>> {
        Box::pin(async move {
            let status = GateStatus::Closed {
                reason: reason.to_owned(),
                since: now,
            };
            let epoch = self.write_row(&status).await.map_err(|error| {
                spot_migration_apply::Error::GateFailed {
                    message: error.to_string(),
                }
            })?;
            self.bump_epoch().await;
            self.store(status, epoch);
            Ok(())
        })
    }

    fn open<'a>(&'a self, _now: i64) -> BoxFuture<'a, ApplyResult<()>> {
        Box::pin(async move {
            let epoch = self.write_row(&GateStatus::Open).await.map_err(|error| {
                spot_migration_apply::Error::GateFailed {
                    message: error.to_string(),
                }
            })?;
            self.bump_epoch().await;
            self.store(GateStatus::Open, epoch);
            Ok(())
        })
    }

    fn status(&self) -> GateStatus {
        self.cached()
            .map(|cached| cached.status)
            .unwrap_or_else(unreadable)
    }

    fn drain(&self, deadline_micros: i64, now: i64) -> BoxFuture<'_, ApplyResult<DrainReport>> {
        Box::pin(async move {
            let initial = self.in_flight().await.map_err(|error| {
                spot_migration_apply::Error::GateFailed {
                    message: error.to_string(),
                }
            })?;
            let poll = 5_000i64.min(deadline_micros.max(1));
            let mut waited = 0i64;
            loop {
                let current = self.in_flight().await.map_err(|error| {
                    spot_migration_apply::Error::GateFailed {
                        message: error.to_string(),
                    }
                })?;
                if current == 0 {
                    return Ok(DrainReport {
                        drained: initial,
                        timed_out: false,
                        waited_micros: waited,
                    });
                }
                if waited >= deadline_micros {
                    return Ok(DrainReport {
                        drained: initial.saturating_sub(current),
                        timed_out: true,
                        waited_micros: waited,
                    });
                }
                let step = poll.min(deadline_micros - waited).max(1);
                tokio::time::sleep(Duration::from_micros(
                    u64::try_from(step).unwrap_or(u64::MAX),
                ))
                .await;
                waited += step;
                let _ = now;
            }
        })
    }
}

impl PgAdmissionGate {
    /// How many other backends on this database hold a transaction right now.
    ///
    /// # Errors
    /// [`Error::ControlUnavailable`] when the count cannot be read.
    pub async fn in_flight(&self) -> Result<u64> {
        let client = client(&self.pool, "counting in-flight transactions").await?;
        let row = client
            .query_one(
                "SELECT COUNT(*) FROM pg_stat_activity \
                 WHERE datname = current_database() \
                   AND pid <> pg_backend_pid() \
                   AND application_name <> $1 \
                   AND (state = 'active' OR state LIKE 'idle in transaction%')",
                &[&MIGRATION_APPLICATION],
            )
            .await
            .map_err(|error| Error::database("counting in-flight transactions", &error))?;
        Ok(u64::try_from(row.get::<_, i64>(0)).unwrap_or(0))
    }
}

impl std::fmt::Debug for PgAdmissionGate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PgAdmissionGate")
            .field("site", &self.site)
            .field("ttl", &self.ttl)
            .field("cached", &self.status())
            .finish_non_exhaustive()
    }
}

/// The injected monotonic instant, in microseconds.
trait MonotonicMicros {
    fn monotonic_micros(&self) -> i64;
}

impl MonotonicMicros for dyn Clock {
    fn monotonic_micros(&self) -> i64 {
        i64::try_from(self.monotonic().as_micros()).unwrap_or(i64::MAX)
    }
}
