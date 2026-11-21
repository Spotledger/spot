//! [`PgMeterStore`]: the PostgreSQL [`MeterStore`].

use std::collections::{BTreeMap, BTreeSet, HashSet};

use deadpool_postgres::{Object, Pool};
use spot_app_api::AppResult;
use spot_metering::{
    BoxFuture, CompactionReport, MAX_BATCH_EVENTS, MemberDigest, Meter, MeterEvent, MeterKind,
    MeterStore, MeteringError, PeriodKey, PeriodResolver, PeriodTotals, RetentionPolicy,
    TenantPeriodUsage,
};
use tokio_postgres::IsolationLevel;
use tokio_postgres::Transaction;

use crate::error;
use crate::keys::{count_u64, floor_from_row, meter_from_key, meter_key, parse_u64};
use crate::{schema, sql};

/// Events inserted per statement; a bigger batch is several statements in one transaction.
const EVENT_CHUNK: usize = 5_000;
/// Rows one compaction or tombstone-purge statement touches (ADR-045: bounded statements).
const COMPACT_BATCH: i64 = 10_000;
/// The same bound as a row count, for comparing what a statement changed.
const COMPACT_BATCH_ROWS: u64 = COMPACT_BATCH as u64;
/// How often a transaction that lost a deadlock or serialisation race is run again. The batch
/// is idempotent, so running it again is safe.
const ATTEMPTS: usize = 4;

/// The usage ledger in PostgreSQL, for one tenant.
///
/// See the crate documentation for the rules it keeps. Call [`PgMeterStore::prepare`] once at
/// start-up; clones share the pool and the tenant.
#[derive(Clone)]
pub struct PgMeterStore {
    pool: Pool,
    tenant: String,
}

impl std::fmt::Debug for PgMeterStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgMeterStore")
            .field("tenant", &self.tenant)
            .finish_non_exhaustive()
    }
}

/// One event that passed validation, with the window it falls in.
struct Prepared {
    event: MeterEvent,
    period: PeriodKey,
}

/// What the newly recorded events add to one `(tenant, period)`.
#[derive(Default)]
struct Delta {
    counters: BTreeMap<String, u128>,
    gauges: BTreeMap<String, u64>,
    members: BTreeMap<String, BTreeSet<MemberDigest>>,
}

impl PgMeterStore {
    /// A store on `pool` for `tenant`, and for no other tenant.
    ///
    /// It does not touch the database until used. Every write lands in `tenant`; an event
    /// naming another tenant is refused with `MeteringError::TenantMismatch` before any write.
    #[must_use]
    pub fn new(pool: Pool, tenant: impl Into<String>) -> Self {
        Self {
            pool,
            tenant: tenant.into(),
        }
    }

    /// Creates the tables and indexes if missing. Idempotent; safe from several nodes at once.
    ///
    /// # Errors
    /// `metering_pg.migration_failed` when the database cannot apply the DDL.
    pub async fn prepare(&self) -> AppResult<()> {
        schema::prepare(&self.pool).await
    }

    async fn conn(&self) -> Result<Object, MeteringError> {
        self.pool.get().await.map_err(error::pool)
    }

    async fn add(
        &self,
        events: Vec<MeterEvent>,
        resolver: &dyn PeriodResolver,
    ) -> Result<usize, MeteringError> {
        if events.len() > MAX_BATCH_EVENTS {
            return Err(MeteringError::BatchTooLarge {
                size: events.len(),
                max: MAX_BATCH_EVENTS,
            });
        }
        // Tenant binding first: an event naming another tenant must not be resolved, inserted
        // or counted, and one such event refuses the whole batch.
        if events.iter().any(|event| event.tenant != self.tenant) {
            return Err(MeteringError::TenantMismatch);
        }
        // Validate and resolve everything before touching the database, so a bad event
        // refuses the whole batch exactly as the in-memory store does.
        let mut seen = HashSet::new();
        let mut prepared = Vec::new();
        for event in events {
            if !event.is_well_formed() {
                return Err(MeteringError::MalformedEvent {
                    id: event.id,
                    meter: event.meter.to_string(),
                });
            }
            let period = resolver.resolve(&event.tenant, event.at)?;
            // The first occurrence of an id in a batch wins.
            if seen.insert(event.id.clone()) {
                prepared.push(Prepared { event, period });
            }
        }
        if prepared.is_empty() {
            return Ok(0);
        }
        // Key order: two overlapping batches then take their row locks in the same order.
        prepared.sort_by(|a, b| a.event.id.cmp(&b.event.id));
        let mut attempt = 0;
        loop {
            attempt += 1;
            match self.add_once(&prepared).await {
                Err(Retry::Again) if attempt < ATTEMPTS => {}
                Err(Retry::Again) => {
                    return Err(error::conflicting());
                }
                Err(Retry::Fail(error)) => return Err(error),
                Ok(added) => return Ok(added),
            }
        }
    }

    async fn add_once(&self, prepared: &[Prepared]) -> Result<usize, Retry> {
        let mut conn = self.conn().await?;
        let tx = conn.transaction().await.map_err(retry)?;
        // The floor row is created (sentinel) and read under a shared lock, so a concurrent
        // compaction cannot advance the floor and purge a tombstone between this read and the
        // inserts below: the refuse-or-count decision is made against one floor, and that is
        // true for the tenant's very first compaction too.
        let floor = floor_for_add(&tx, &self.tenant).await?;
        if let Some(floor) = &floor {
            for item in prepared {
                if item.period.end_exclusive <= floor.start {
                    return Err(Retry::Fail(MeteringError::LateBeyondHorizon {
                        id: item.event.id.clone(),
                        period: item.period.label.clone(),
                    }));
                }
            }
        }
        let mut fresh: Vec<&Prepared> = Vec::new();
        for chunk in prepared.chunks(EVENT_CHUNK) {
            fresh.extend(insert_events(&tx, chunk).await?);
        }
        if fresh.is_empty() {
            tx.commit().await.map_err(retry)?;
            return Ok(0);
        }
        apply_deltas(&tx, &self.tenant, &fresh).await?;
        tx.commit().await.map_err(retry)?;
        Ok(fresh.len())
    }

    async fn usage(
        &self,
        tenant: &str,
        period: &PeriodKey,
    ) -> Result<TenantPeriodUsage, MeteringError> {
        let mut conn = self.conn().await?;
        // One snapshot for the totals and the members, so a count and its population agree.
        let tx = conn
            .build_transaction()
            .isolation_level(IsolationLevel::RepeatableRead)
            .read_only(true)
            .start()
            .await
            .map_err(|e| error::pg(&e))?;
        let mut usage = TenantPeriodUsage::default();
        let Some(period_id) = find_period(&tx, tenant, period).await? else {
            return Ok(usage);
        };
        for row in tx
            .query(sql::SELECT_TOTALS, &[&period_id])
            .await
            .map_err(|e| error::pg(&e))?
        {
            let meter = meter_from_key(row.get(0))?;
            let totals = PeriodTotals {
                sum: parse_u64(row.get(1))?,
                max: parse_u64(row.get(2))?,
                distinct: count_u64(row.get(3))?,
            };
            usage.by_meter.insert(meter, totals);
        }
        for row in tx
            .query(sql::SELECT_MEMBERS, &[&period_id])
            .await
            .map_err(|e| error::pg(&e))?
        {
            let meter = meter_from_key(row.get(0))?;
            let digest: String = row.get(1);
            usage
                .member_sets
                .entry(meter)
                .or_default()
                .push(MemberDigest::new(digest));
        }
        Ok(usage)
    }

    async fn members(
        &self,
        tenant: &str,
        period: &PeriodKey,
        meter: &Meter,
    ) -> Result<Vec<MemberDigest>, MeteringError> {
        let mut conn = self.conn().await?;
        let tx = conn.transaction().await.map_err(|e| error::pg(&e))?;
        let Some(period_id) = find_period(&tx, tenant, period).await? else {
            return Ok(Vec::new());
        };
        let rows = tx
            .query(sql::SELECT_METER_MEMBERS, &[&period_id, &meter_key(meter)])
            .await
            .map_err(|e| error::pg(&e))?;
        Ok(rows
            .iter()
            .map(|row| MemberDigest::new(row.get::<_, String>(0)))
            .collect())
    }

    /// How many tombstones this tenant holds.
    async fn tombstones(&self) -> Result<u64, MeteringError> {
        let conn = self.conn().await?;
        let row = conn
            .query_one(sql::COUNT_TOMBSTONES, &[&self.tenant])
            .await
            .map_err(|e| error::pg(&e))?;
        count_u64(row.get(0))
    }

    /// Drops raw metadata up to `before`, then the tombstones past the horizon.
    ///
    /// The floor is recorded **before** any tombstone is deleted, so a crash in between leaves
    /// tombstones that the next run removes; it can never leave a purged id that the write path
    /// would still accept.
    async fn compact_before(
        &self,
        before: &PeriodKey,
        policy: &RetentionPolicy,
        resolver: &dyn PeriodResolver,
    ) -> Result<CompactionReport, MeteringError> {
        let policy = policy.validated()?;
        let cut = policy.tombstone_cut(before, &self.tenant, resolver)?;
        let conn = self.conn().await?;
        let mut events_dropped = 0u64;
        loop {
            // Each batch is its own statement, so compaction never holds a long lock and a
            // crash between batches loses nothing (the next run continues).
            let changed = conn
                .execute(
                    sql::COMPACT_BATCH,
                    &[&self.tenant, &before.start, &COMPACT_BATCH],
                )
                .await
                .map_err(|e| error::pg(&e))?;
            events_dropped += changed;
            if changed < COMPACT_BATCH_ROWS {
                break;
            }
        }
        let mut tombstones_dropped = 0u64;
        let mut floor = None;
        if let Some(cut) = cut {
            let basis = cut.basis.label();
            conn.execute(
                sql::ADVANCE_FLOOR,
                &[
                    &self.tenant,
                    &cut.start,
                    &cut.end_exclusive,
                    &cut.label,
                    &cut.zone,
                    &basis,
                ],
            )
            .await
            .map_err(|e| error::pg(&e))?;
            floor = read_floor(&conn, &self.tenant).await?;
            if let Some(floor) = &floor {
                loop {
                    let changed = conn
                        .execute(
                            sql::DELETE_TOMBSTONES,
                            &[&self.tenant, &floor.start, &COMPACT_BATCH],
                        )
                        .await
                        .map_err(|e| error::pg(&e))?;
                    tombstones_dropped += changed;
                    if changed < COMPACT_BATCH_ROWS {
                        break;
                    }
                }
            }
        }
        Ok(CompactionReport {
            events_dropped,
            tombstones_dropped,
            tombstone_floor: floor,
        })
    }
}

/// How a transaction attempt ended badly.
#[derive(Debug)]
enum Retry {
    /// A deadlock or serialisation failure: run the whole transaction again.
    Again,
    /// Anything else: report it.
    Fail(MeteringError),
}

impl From<MeteringError> for Retry {
    fn from(error: MeteringError) -> Self {
        Self::Fail(error)
    }
}

/// Classifies a driver error for the retry loop.
fn retry(error: tokio_postgres::Error) -> Retry {
    if error::is_retryable(&error) {
        Retry::Again
    } else {
        Retry::Fail(error::pg(&error))
    }
}

/// The tenant's floor, read under a shared lock inside the batch's transaction.
///
/// The no-floor sentinel row is created first ([`sql::ENSURE_FLOOR_ROW`]), because `FOR SHARE` on
/// a row that does not exist yet locks nothing: a first compaction could otherwise advance the
/// floor and purge a tombstone between this read and the batch's insert, and a replayed id would
/// be counted twice. With the row present, the writer's shared lock and the compaction's
/// `ADVANCE_FLOOR` update serialise, so the batch either finishes against the old floor (its
/// tombstones still there) or reads the new one and refuses the period.
async fn floor_for_add(
    tx: &Transaction<'_>,
    tenant: &str,
) -> Result<Option<PeriodKey>, MeteringError> {
    tx.execute(
        sql::ENSURE_FLOOR_ROW,
        &[&tenant, &sql::NO_FLOOR_START, &sql::NO_FLOOR_END_EXCLUSIVE],
    )
    .await
    .map_err(|e| error::pg(&e))?;
    let row = tx
        .query_opt(sql::SELECT_FLOOR_FOR_ADD, &[&tenant])
        .await
        .map_err(|e| error::pg(&e))?;
    match row {
        Some(row) => floor_from_row(&row),
        None => Ok(None),
    }
}

/// The tenant's floor, outside a write transaction.
async fn read_floor(conn: &Object, tenant: &str) -> Result<Option<PeriodKey>, MeteringError> {
    let row = conn
        .query_opt(sql::SELECT_FLOOR, &[&tenant])
        .await
        .map_err(|e| error::pg(&e))?;
    match row {
        Some(row) => floor_from_row(&row),
        None => Ok(None),
    }
}

/// Inserts one chunk, first write wins, and returns the events that were really new.
async fn insert_events<'a>(
    tx: &Transaction<'_>,
    chunk: &'a [Prepared],
) -> Result<Vec<&'a Prepared>, Retry> {
    let n = chunk.len();
    let mut tenants = Vec::with_capacity(n);
    let mut ids = Vec::with_capacity(n);
    let mut meters = Vec::with_capacity(n);
    let mut amounts = Vec::with_capacity(n);
    let mut members: Vec<Option<String>> = Vec::with_capacity(n);
    let mut instants = Vec::with_capacity(n);
    let mut ends = Vec::with_capacity(n);
    for item in chunk {
        tenants.push(item.event.tenant.clone());
        ids.push(item.event.id.clone());
        meters.push(meter_key(&item.event.meter));
        amounts.push(item.event.amount.to_string());
        members.push(item.event.member.as_ref().map(|m| m.as_str().to_owned()));
        instants.push(item.event.at);
        ends.push(item.period.end_exclusive);
    }
    let rows = tx
        .query(
            sql::INSERT_EVENTS,
            &[
                &tenants, &ids, &meters, &amounts, &members, &instants, &ends,
            ],
        )
        .await
        .map_err(retry)?;
    let inserted: HashSet<String> = rows.iter().map(|row| row.get::<_, String>(1)).collect();
    Ok(chunk
        .iter()
        .filter(|item| inserted.contains(&item.event.id))
        .collect())
}

/// Folds the newly recorded events into counter sums, gauge maxima and member sets, per
/// `(tenant, period)`.
fn fold(fresh: &[&Prepared]) -> BTreeMap<PeriodKey, Delta> {
    let mut deltas: BTreeMap<PeriodKey, Delta> = BTreeMap::new();
    for item in fresh {
        let delta = deltas.entry(item.period.clone()).or_default();
        let key = meter_key(&item.event.meter);
        match item.event.meter.kind() {
            MeterKind::Counter => {
                *delta.counters.entry(key).or_default() += u128::from(item.event.amount);
            }
            MeterKind::Gauge => {
                let max = delta.gauges.entry(key).or_default();
                *max = (*max).max(item.event.amount);
            }
            MeterKind::DistinctSet => {
                if let Some(member) = &item.event.member {
                    delta.members.entry(key).or_default().insert(member.clone());
                }
            }
        }
    }
    deltas
}

/// Updates totals and members for the new events only, in one key order (counters, gauges,
/// then members, each by period and meter) so concurrent batches cannot deadlock each other.
async fn apply_deltas(
    tx: &Transaction<'_>,
    tenant: &str,
    fresh: &[&Prepared],
) -> Result<(), Retry> {
    let deltas = fold(fresh);
    let mut counters = (Vec::new(), Vec::new(), Vec::new());
    let mut gauges = (Vec::new(), Vec::new(), Vec::new());
    let mut members = (Vec::new(), Vec::new(), Vec::new());
    for (period, delta) in &deltas {
        let period_id = ensure_period(tx, tenant, period).await?;
        for (meter, sum) in &delta.counters {
            counters.0.push(period_id);
            counters.1.push(meter.clone());
            counters.2.push(sum.to_string());
        }
        for (meter, max) in &delta.gauges {
            gauges.0.push(period_id);
            gauges.1.push(meter.clone());
            gauges.2.push(max.to_string());
        }
        for (meter, set) in &delta.members {
            for digest in set {
                members.0.push(period_id);
                members.1.push(meter.clone());
                members.2.push(digest.as_str().to_owned());
            }
        }
    }
    if !counters.0.is_empty() {
        tx.execute(sql::ADD_COUNTERS, &[&counters.0, &counters.1, &counters.2])
            .await
            .map_err(retry)?;
    }
    if !gauges.0.is_empty() {
        tx.execute(sql::RAISE_GAUGES, &[&gauges.0, &gauges.1, &gauges.2])
            .await
            .map_err(retry)?;
    }
    if !members.0.is_empty() {
        let rows = tx
            .query(sql::INSERT_MEMBERS, &[&members.0, &members.1, &members.2])
            .await
            .map_err(retry)?;
        // Count what was really new per (period, meter); a member already known adds nothing.
        let mut added: BTreeMap<(i64, String), i64> = BTreeMap::new();
        for (period_id, meter) in members.0.iter().zip(&members.1) {
            added.entry((*period_id, meter.clone())).or_default();
        }
        for row in &rows {
            *added
                .entry((row.get::<_, i64>(0), row.get::<_, String>(1)))
                .or_default() += 1;
        }
        let ids: Vec<i64> = added.keys().map(|(id, _)| *id).collect();
        let names: Vec<String> = added.keys().map(|(_, meter)| meter.clone()).collect();
        let counts: Vec<i64> = added.values().copied().collect();
        tx.execute(sql::ADD_DISTINCT, &[&ids, &names, &counts])
            .await
            .map_err(retry)?;
    }
    Ok(())
}

/// The id of a period window, creating it when new.
async fn ensure_period(
    tx: &Transaction<'_>,
    tenant: &str,
    period: &PeriodKey,
) -> Result<i64, Retry> {
    let basis = period.basis.label();
    let params: [&(dyn tokio_postgres::types::ToSql + Sync); 6] = [
        &tenant,
        &period.start,
        &period.end_exclusive,
        &period.label,
        &period.zone,
        &basis,
    ];
    tx.execute(sql::INSERT_PERIOD, &params)
        .await
        .map_err(retry)?;
    let row = tx
        .query_one(sql::SELECT_PERIOD, &params)
        .await
        .map_err(retry)?;
    Ok(row.get(0))
}

/// The id of a period window, or `None` when nothing was ever recorded in it.
async fn find_period(
    tx: &Transaction<'_>,
    tenant: &str,
    period: &PeriodKey,
) -> Result<Option<i64>, MeteringError> {
    let basis = period.basis.label();
    let row = tx
        .query_opt(
            sql::SELECT_PERIOD,
            &[
                &tenant,
                &period.start,
                &period.end_exclusive,
                &period.label,
                &period.zone,
                &basis,
            ],
        )
        .await
        .map_err(|e| error::pg(&e))?;
    Ok(row.map(|row| row.get(0)))
}

impl MeterStore for PgMeterStore {
    fn tenant(&self) -> &str {
        &self.tenant
    }

    fn add_events<'a>(
        &'a self,
        events: Vec<MeterEvent>,
        resolver: &'a dyn PeriodResolver,
    ) -> BoxFuture<'a, Result<usize, MeteringError>> {
        Box::pin(self.add(events, resolver))
    }

    fn period_totals<'a>(
        &'a self,
        tenant: &'a str,
        period: &'a PeriodKey,
        _resolver: &'a dyn PeriodResolver,
    ) -> BoxFuture<'a, Result<TenantPeriodUsage, MeteringError>> {
        Box::pin(async move {
            if tenant != self.tenant {
                return Err(MeteringError::TenantMismatch);
            }
            self.usage(tenant, period).await
        })
    }

    fn distinct_members<'a>(
        &'a self,
        tenant: &'a str,
        period: &'a PeriodKey,
        meter: &'a Meter,
        _resolver: &'a dyn PeriodResolver,
    ) -> BoxFuture<'a, Result<Vec<MemberDigest>, MeteringError>> {
        Box::pin(async move {
            if tenant != self.tenant {
                return Err(MeteringError::TenantMismatch);
            }
            self.members(tenant, period, meter).await
        })
    }

    fn tombstone_count<'a>(&'a self) -> BoxFuture<'a, Result<u64, MeteringError>> {
        Box::pin(self.tombstones())
    }

    fn compact<'a>(
        &'a self,
        before: &'a PeriodKey,
        policy: &'a RetentionPolicy,
        resolver: &'a dyn PeriodResolver,
    ) -> BoxFuture<'a, Result<CompactionReport, MeteringError>> {
        Box::pin(self.compact_before(before, policy, resolver))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::time::Duration;

    use deadpool_postgres::{Pool, PoolConfig, Runtime};
    use spot_metering::{Meter, MeterEvent, PeriodSpec, TenantPeriods, period_of};
    use spot_postgres::testing::TestDb;
    use tokio_postgres::NoTls;

    use super::*;

    /// Seconds from the epoch to 1970-02-01: the first instant of the second calendar month.
    const FEBRUARY: i64 = 2_678_400;

    fn pool(db: &TestDb, size: usize) -> Pool {
        let c = db.config();
        let mut cfg = deadpool_postgres::Config::new();
        cfg.host = Some(c.host.clone());
        cfg.port = Some(c.port);
        cfg.dbname = Some(c.dbname.clone());
        cfg.user = Some(c.user.clone());
        cfg.password = Some(c.password.clone());
        cfg.application_name = Some("metering_floor_race".to_owned());
        cfg.pool = Some(PoolConfig::new(size));
        cfg.create_pool(Some(Runtime::Tokio1), NoTls).unwrap()
    }

    /// Recorded security finding review blocker: the tenant's **first** compaction must not purge a tombstone
    /// between a replay's floor read and its insert.
    ///
    /// `FOR SHARE` on a row that does not exist locks nothing, so before the sentinel row the
    /// interleaving was: (a) the writer reads "no floor"; (b) the first compaction advances the
    /// floor and deletes the replayed id's tombstone; (c) the writer inserts the id as new and
    /// adds it to the totals again. The sentinel row is created in the writer's transaction, so
    /// here the compaction's `ADVANCE_FLOOR` waits for the writer, the tombstone is still there
    /// (the insert conflicts and returns nothing), and the total stays at one event's worth.
    #[tokio::test]
    async fn the_first_compaction_cannot_purge_between_a_replays_floor_read_and_its_insert() {
        let Some(db) = TestDb::create().await else {
            return;
        };
        let pool = pool(&db, 4);
        let store = PgMeterStore::new(pool.clone(), "acme");
        store.prepare().await.unwrap();
        let res = TenantPeriods::new(PeriodSpec::calendar_month("UTC").unwrap());
        let january = period_of(0, res.spec_for("acme")).unwrap();
        let february = period_of(FEBRUARY, res.spec_for("acme")).unwrap();
        let march = res.resolve("acme", february.end_exclusive).unwrap();
        // April with a two-period horizon cuts at February: January's tombstones go, February's
        // stay. Both periods are final as far as this test is concerned.
        let april = res.resolve("acme", march.end_exclusive).unwrap();
        let events = vec![
            MeterEvent::amount("x", "acme", Meter::Transactions, 3, 0),
            MeterEvent::amount("y", "acme", Meter::Transactions, 4, FEBRUARY),
        ];
        assert_eq!(store.add_events(events, &res).await.unwrap(), 2);

        // The writer's transaction reads the floor: the point the old code was vulnerable at.
        let mut writer = pool.get().await.unwrap();
        let tx = writer.transaction().await.unwrap();
        assert_eq!(floor_for_add(&tx, "acme").await.unwrap(), None);

        // The first compaction on another connection must wait for the writer's floor row.
        let compactor = PgMeterStore::new(pool.clone(), "acme");
        let resolver = res.clone();
        let mut compacting = tokio::spawn(async move {
            compactor
                .compact(&april, &RetentionPolicy::new(2), &resolver)
                .await
        });
        assert!(
            tokio::time::timeout(Duration::from_secs(1), &mut compacting)
                .await
                .is_err(),
            "the first compaction must wait for the writer that holds the floor row"
        );

        // The replay the old interleaving counted twice: the tombstone is still there.
        let replay = Prepared {
            event: MeterEvent::amount("x", "acme", Meter::Transactions, 3, 0),
            period: january.clone(),
        };
        let fresh = insert_events(&tx, std::slice::from_ref(&replay))
            .await
            .unwrap();
        assert!(fresh.is_empty(), "the replay is not a new event");
        tx.commit().await.unwrap();

        compacting.await.unwrap().unwrap();

        // Counted once, and now refused rather than counted again.
        assert_eq!(
            store
                .period_totals("acme", &january, &res)
                .await
                .unwrap()
                .total(&Meter::Transactions),
            3
        );
        let replay = MeterEvent::amount("x", "acme", Meter::Transactions, 3, 0);
        assert_eq!(
            store
                .add_events(vec![replay], &res)
                .await
                .unwrap_err()
                .code(),
            "metering.late_beyond_horizon"
        );
    }
}
