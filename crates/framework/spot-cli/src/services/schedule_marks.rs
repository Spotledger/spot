//! Where a schedule's high-water mark lives so a failover resumes where the old leader stopped
//! (WP-PLT-07.T3).
//!
//! The planner (`spot-scheduler`) keeps each schedule's mark in memory, so a node that becomes
//! leader after a failover would start at `now` and lose every run between the old leader's last
//! tick and its own first one. The mark is therefore also kept in the coordinator, as the
//! counter `schedule_mark:<site>:<app>:<schedule>` holding whole seconds since the epoch (`0`
//! means no mark). The counter never goes down (`set_at_least`), so a node whose clock runs
//! behind can never lower it, and two sites sharing one coordinator never share a mark because
//! the site is in the name. The mark is the last firing the jobs service **enqueued**, not the
//! time of the last tick: a firing is only ever behind the mark once its job exists.
//!
//! Firings fall on whole seconds (cron's smallest field is the second), so flooring the mark to
//! a whole second loses nothing.
//!
//! The single-node profile's coordinator is the in-process one and keeps its counters in
//! memory, so by itself a process restart forgets the marks there. A **durable table**
//! ([`MarkTable`], WP-PLT-07.T4) closes that: when one is installed for the site
//! ([`install_table`]), [`CoordinatorMarks::store`] writes the table first and the coordinator
//! second, and [`CoordinatorMarks::load`] answers the later of the two, so a restart resumes where
//! the last enqueued firing was. The table never goes down either (`GREATEST`).
//!
//! Atomicity with the enqueue. The job store inserts through its own transaction, so the mark
//! cannot share it; the order does the work instead: the job is inserted (idempotently, by the
//! slot's key) and only then is the mark raised. A crash between the two leaves the mark behind
//! the job, and the next start plans the slot again under the same key, which the store refuses
//! as a duplicate: no double fire, no skipped slot. The missed-run policy of each schedule then
//! applies once, from the stored mark, when the leader plans (the planner reads the mark at the
//! start of every leadership).

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};

use jiff::Timestamp;
use spot_coordinator::Coordinator;

type TableFuture<'a, T> = Pin<Box<dyn Future<Output = crate::Result<T>> + Send + 'a>>;

/// Where marks live beyond the coordinator: one row per `(site, app, schedule)` holding whole
/// seconds. An implementation never lowers a stored value.
pub trait MarkTable: Send + Sync {
    /// The stored mark of `app`'s `schedule`, or `None`.
    fn read<'a>(&'a self, app: &'a str, schedule: &'a str) -> TableFuture<'a, Option<Timestamp>>;
    /// Raises the stored mark to `mark` (floored to seconds); lower or equal changes nothing.
    fn raise<'a>(&'a self, app: &'a str, schedule: &'a str, mark: Timestamp)
    -> TableFuture<'a, ()>;
}

/// An in-memory [`MarkTable`]: what a test shares between a "before" and an "after" restart.
#[derive(Default)]
pub struct MemoryMarkTable {
    rows: Mutex<HashMap<(String, String), i64>>,
}

impl MemoryMarkTable {
    /// An empty table.
    #[must_use]
    pub fn new() -> MemoryMarkTable {
        MemoryMarkTable::default()
    }
}

impl MarkTable for MemoryMarkTable {
    fn read<'a>(&'a self, app: &'a str, schedule: &'a str) -> TableFuture<'a, Option<Timestamp>> {
        let seconds = self
            .rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&(app.to_owned(), schedule.to_owned()))
            .copied();
        Box::pin(async move {
            seconds
                .map(Timestamp::from_second)
                .transpose()
                .map_err(|e| crate::Error(format!("a stored schedule mark is out of range: {e}")))
        })
    }

    fn raise<'a>(
        &'a self,
        app: &'a str,
        schedule: &'a str,
        mark: Timestamp,
    ) -> TableFuture<'a, ()> {
        let mut rows = self
            .rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = rows
            .entry((app.to_owned(), schedule.to_owned()))
            .or_insert(i64::MIN);
        *entry = (*entry).max(mark.as_second());
        Box::pin(async { Ok(()) })
    }
}

/// The marks table in the site's database (`spot_schedule_mark`, created if missing, the way the
/// trigger marks of the process service are).
pub struct PgMarkTable {
    pool: deadpool_postgres::Pool,
    site: String,
}

impl PgMarkTable {
    /// A table over `pool` for `site`. Call [`PgMarkTable::prepare`] once before use.
    #[must_use]
    pub fn new(pool: deadpool_postgres::Pool, site: &str) -> PgMarkTable {
        PgMarkTable {
            pool,
            site: site.to_owned(),
        }
    }

    /// Creates the table when it is missing.
    ///
    /// # Errors
    ///
    /// The database refused.
    pub async fn prepare(&self) -> crate::Result<()> {
        let client = self.client().await?;
        client
            .batch_execute(
                "CREATE TABLE IF NOT EXISTS spot_schedule_mark (\
                 site text NOT NULL, app text NOT NULL, schedule text NOT NULL, \
                 mark bigint NOT NULL, PRIMARY KEY (site, app, schedule))",
            )
            .await
            .map_err(|e| {
                crate::Error(format!(
                    "the schedule mark table could not be prepared: {e}"
                ))
            })
    }

    async fn client(&self) -> crate::Result<deadpool_postgres::Client> {
        self.pool.get().await.map_err(|e| {
            crate::Error(format!(
                "the schedule marks could not reach the database: {e}"
            ))
        })
    }
}

impl MarkTable for PgMarkTable {
    fn read<'a>(&'a self, app: &'a str, schedule: &'a str) -> TableFuture<'a, Option<Timestamp>> {
        Box::pin(async move {
            let client = self.client().await?;
            let row = client
                .query_opt(
                    "SELECT mark FROM spot_schedule_mark WHERE site = $1 AND app = $2 AND schedule = $3",
                    &[&self.site, &app, &schedule],
                )
                .await
                .map_err(|e| crate::Error(format!("reading a schedule mark failed: {e}")))?;
            match row {
                Some(row) => Timestamp::from_second(row.get::<_, i64>(0))
                    .map(Some)
                    .map_err(|e| {
                        crate::Error(format!("a stored schedule mark is out of range: {e}"))
                    }),
                None => Ok(None),
            }
        })
    }

    fn raise<'a>(
        &'a self,
        app: &'a str,
        schedule: &'a str,
        mark: Timestamp,
    ) -> TableFuture<'a, ()> {
        Box::pin(async move {
            let client = self.client().await?;
            let seconds = mark.as_second();
            client
                .execute(
                    "INSERT INTO spot_schedule_mark (site, app, schedule, mark) VALUES ($1, $2, $3, $4) \
                     ON CONFLICT (site, app, schedule) DO UPDATE \
                     SET mark = GREATEST(spot_schedule_mark.mark, EXCLUDED.mark)",
                    &[&self.site, &app, &schedule, &seconds],
                )
                .await
                .map(|_| ())
                .map_err(|e| crate::Error(format!("saving a schedule mark failed: {e}")))
        })
    }
}

fn tables() -> std::sync::MutexGuard<'static, HashMap<String, Arc<dyn MarkTable>>> {
    static TABLES: OnceLock<Mutex<HashMap<String, Arc<dyn MarkTable>>>> = OnceLock::new();
    TABLES
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Makes `table` the durable table of `site`: every [`CoordinatorMarks`] built for the site
/// afterwards writes through it. The host calls this when it assembles the jobs service on a
/// site with a database.
pub fn install_table(site: &str, table: Arc<dyn MarkTable>) {
    tables().insert(site.to_owned(), table);
}

/// Forgets the durable table of `site` (a test's cleanup).
pub fn remove_table(site: &str) {
    tables().remove(site);
}

/// Reads and writes schedule marks in a coordinator, for one site.
#[derive(Clone)]
pub struct CoordinatorMarks {
    coordinator: Arc<dyn Coordinator>,
    site: String,
    table: Option<Arc<dyn MarkTable>>,
}

impl CoordinatorMarks {
    /// Marks for `site` in `coordinator`.
    #[must_use]
    pub fn new(coordinator: Arc<dyn Coordinator>, site: &str) -> Self {
        Self {
            coordinator,
            site: site.to_owned(),
            table: tables().get(site).cloned(),
        }
    }

    /// The counter that holds the mark of `app`'s `schedule`.
    #[must_use]
    pub fn counter_name(&self, app: &str, schedule: &str) -> String {
        format!("schedule_mark:{}:{app}:{schedule}", self.site)
    }

    /// The stored mark, or `None` when the schedule has none yet.
    ///
    /// # Errors
    ///
    /// The coordinator cannot answer. The caller must not lead without the mark (starting at
    /// `now` would lose the runs of the gap), so this is an error and not `None`.
    pub async fn load(&self, app: &str, schedule: &str) -> crate::Result<Option<Timestamp>> {
        let name = self.counter_name(app, schedule);
        let seconds =
            self.coordinator.peek(&name).await.map_err(|e| {
                crate::Error(format!("schedule mark `{name}` could not be read: {e}"))
            })?;
        let from_coordinator = if seconds == 0 {
            None
        } else {
            let seconds = i64::try_from(seconds)
                .map_err(|_| crate::Error(format!("schedule mark `{name}` is out of range")))?;
            Some(Timestamp::from_second(seconds).map_err(|e| {
                crate::Error(format!("schedule mark `{name}` is out of range: {e}"))
            })?)
        };
        // The durable table, when there is one, is read too: the later of the two wins, so a
        // restart that lost the in-process counters still resumes from the table.
        let from_table = match &self.table {
            Some(table) => table.read(app, schedule).await?,
            None => None,
        };
        Ok(match (from_coordinator, from_table) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        })
    }

    /// Raises the stored mark to `mark` (floored to whole seconds); a lower or equal value, and a
    /// mark at or before the epoch, change nothing.
    ///
    /// # Errors
    ///
    /// The coordinator refused the write.
    pub async fn store(&self, app: &str, schedule: &str, mark: Timestamp) -> crate::Result<()> {
        let Ok(seconds) = u64::try_from(mark.as_second()) else {
            return Ok(());
        };
        if seconds == 0 {
            return Ok(());
        }
        // The table first: it is the one that survives a restart.
        if let Some(table) = &self.table {
            table.raise(app, schedule, mark).await?;
        }
        let name = self.counter_name(app, schedule);
        self.coordinator
            .set_at_least(&name, seconds)
            .await
            .map(|_| ())
            .map_err(|e| crate::Error(format!("schedule mark `{name}` could not be stored: {e}")))
    }
}
