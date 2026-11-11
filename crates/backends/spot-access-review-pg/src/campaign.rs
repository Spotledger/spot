//! [`PgCampaignStore`]: the PostgreSQL [`CampaignStore`].
//!
//! Every mutation is one transaction that first locks the campaign row (`FOR UPDATE`), so two
//! decisions, two ticks or a decision and a tick on one campaign run one after the other, each
//! on the state the previous one committed. The state machine itself runs in this process on
//! the loaded state through the same planning functions the memory fake uses; the database
//! checks the outcome again with the item version in the `UPDATE`'s `WHERE` and with triggers
//! that refuse any rewrite of the snapshot or the history.

use deadpool_postgres::{Object, Pool};
use spot_access_review::store::{
    CampaignStore, CampaignSummary, HistoryEntry, HistoryRecord, StoreError, StoreFuture,
    StoredCampaign, Transition, check_id, check_limit, check_open, entries_of, plan_decide,
    plan_tick, refusal_entry,
};
use spot_access_review::{
    Campaign, CampaignSpec, CampaignStatus, Decision, Outcome, ReviewEvent, ReviewItem,
    ReviewItemState,
};
use spot_app_api::AppResult;
use spot_tenancy::TenantId;
use tokio_postgres::Transaction;

use crate::error::{campaign_pool, campaign_unavailable as unavailable, is_unique};

const LOAD_CAMPAIGN: &str = "SELECT spec, open_events, status FROM spot_access_review_campaigns \
    WHERE tenant = $1 AND campaign_id = $2";
const LOCK_CAMPAIGN: &str = "SELECT spec, open_events, status FROM spot_access_review_campaigns \
    WHERE tenant = $1 AND campaign_id = $2 FOR UPDATE";
const LOAD_ITEMS: &str = "SELECT snapshot, state, version FROM spot_access_review_items \
    WHERE tenant = $1 AND campaign_id = $2 ORDER BY ord";
const INSERT_CAMPAIGN: &str = "INSERT INTO spot_access_review_campaigns \
    (tenant, campaign_id, title, deadline, item_count, spec, open_events, status, opened_at) \
    VALUES ($1, $2, $3, $4, $5, $6, $7, 'open', $8)";
const INSERT_ITEMS: &str = "INSERT INTO spot_access_review_items \
    (tenant, campaign_id, ord, item_id, snapshot, state, version) \
    SELECT $1::text, $2::text, (t.ord - 1)::integer, t.item_id, t.snapshot, t.state, 0 \
    FROM unnest($3::text[], $4::text[], $5::text[]) WITH ORDINALITY AS t(item_id, snapshot, state, ord)";
const UPDATE_ITEMS: &str = "UPDATE spot_access_review_items i \
    SET state = u.state, version = i.version + 1 \
    FROM unnest($3::text[], $4::text[], $5::bigint[]) AS u(item_id, state, version) \
    WHERE i.tenant = $1 AND i.campaign_id = $2 AND i.item_id = u.item_id AND i.version = u.version";
const UPDATE_STATUS: &str = "UPDATE spot_access_review_campaigns SET status = $3 \
    WHERE tenant = $1 AND campaign_id = $2";
// The next number is read inside the statement that writes it, under the campaign row lock the
// caller holds, so numbers are dense per campaign and a tenant never sees another's counter.
const INSERT_HISTORY: &str = "INSERT INTO spot_access_review_history \
    (tenant, campaign_id, seq, kind, entry, at) \
    SELECT $1::text, $2::text, \
        (SELECT COALESCE(MAX(h.seq), 0) FROM spot_access_review_history h \
         WHERE h.tenant = $1 AND h.campaign_id = $2) + t.ord, \
        t.kind, t.entry, $5::bigint \
    FROM unnest($3::text[], $4::text[]) WITH ORDINALITY AS t(kind, entry, ord)";
const LIST: &str = "SELECT campaign_id, title, status, deadline, item_count \
    FROM spot_access_review_campaigns \
    WHERE tenant = $1 AND ($2::text IS NULL OR campaign_id COLLATE \"C\" > $2::text COLLATE \"C\") \
    ORDER BY campaign_id COLLATE \"C\" LIMIT $3";
const EXISTS: &str =
    "SELECT 1 FROM spot_access_review_campaigns WHERE tenant = $1 AND campaign_id = $2";
const HISTORY: &str = "SELECT seq, kind, entry, at FROM spot_access_review_history \
    WHERE tenant = $1 AND campaign_id = $2 AND seq > $3 ORDER BY seq LIMIT $4";

/// The campaign store in PostgreSQL. Clones share the pool; a restart is a new store over the
/// same database and sees everything the old one committed.
#[derive(Clone)]
pub struct PgCampaignStore {
    pool: Pool,
}

impl std::fmt::Debug for PgCampaignStore {
    /// Names the type only, never a pool or connection detail.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgCampaignStore").finish_non_exhaustive()
    }
}

fn json<T: serde::Serialize>(value: &T) -> Result<String, StoreError> {
    serde_json::to_string(value).map_err(|_| StoreError::Corrupt)
}

fn unjson<T: serde::de::DeserializeOwned>(text: &str) -> Result<T, StoreError> {
    serde_json::from_str(text).map_err(|_| StoreError::Corrupt)
}

fn status_text(status: CampaignStatus) -> &'static str {
    match status {
        CampaignStatus::Open => "open",
        CampaignStatus::Completed => "completed",
    }
}

fn parse_status(text: &str) -> Result<CampaignStatus, StoreError> {
    match text {
        "open" => Ok(CampaignStatus::Open),
        "completed" => Ok(CampaignStatus::Completed),
        _ => Err(StoreError::Corrupt),
    }
}

/// Reads the campaign and its items inside `tx`; `lock` takes the campaign row for update.
async fn load_tx(
    tx: &Transaction<'_>,
    tenant: &str,
    id: &str,
    lock: bool,
) -> Result<StoredCampaign, StoreError> {
    let sql = if lock { LOCK_CAMPAIGN } else { LOAD_CAMPAIGN };
    let row = tx
        .query_opt(sql, &[&tenant, &id])
        .await
        .map_err(|e| unavailable(&e))?
        .ok_or(StoreError::NotFound)?;
    let (spec, open_events, status): (String, String, String) =
        (row.get(0), row.get(1), row.get(2));
    let rows = tx
        .query(LOAD_ITEMS, &[&tenant, &id])
        .await
        .map_err(|e| unavailable(&e))?;
    let mut items: Vec<ReviewItem> = Vec::with_capacity(rows.len());
    let mut states: Vec<ReviewItemState> = Vec::with_capacity(rows.len());
    let mut versions = Vec::with_capacity(rows.len());
    for r in &rows {
        items.push(unjson(&r.get::<_, String>(0))?);
        states.push(unjson(&r.get::<_, String>(1))?);
        versions.push(u64::try_from(r.get::<_, i64>(2)).map_err(|_| StoreError::Corrupt)?);
    }
    let campaign = Campaign::restore(
        unjson::<CampaignSpec>(&spec)?,
        items,
        states,
        unjson::<Vec<ReviewEvent>>(&open_events)?,
        parse_status(&status)?,
    )
    .map_err(|_| StoreError::Corrupt)?;
    // The row key and the spec must agree: a row moved by hand is refused, not served.
    if campaign.spec().id != id {
        return Err(StoreError::Corrupt);
    }
    Ok(StoredCampaign {
        campaign,
        item_versions: versions,
    })
}

async fn append_history(
    tx: &Transaction<'_>,
    tenant: &str,
    id: &str,
    entries: &[HistoryEntry],
    at: i64,
) -> Result<(), StoreError> {
    if entries.is_empty() {
        return Ok(());
    }
    let kinds: Vec<String> = entries.iter().map(|e| e.kind().to_owned()).collect();
    let bodies = entries
        .iter()
        .map(json)
        .collect::<Result<Vec<String>, StoreError>>()?;
    tx.execute(INSERT_HISTORY, &[&tenant, &id, &kinds, &bodies, &at])
        .await
        .map_err(|e| unavailable(&e))?;
    Ok(())
}

/// Writes a planned step: the changed items (each guarded by the version it was read at), the
/// status and the history, all in the caller's transaction.
async fn persist(
    tx: &Transaction<'_>,
    tenant: &str,
    id: &str,
    before: &StoredCampaign,
    step: &Transition,
    at: i64,
) -> Result<(), StoreError> {
    if !step.changed.is_empty() {
        let mut ids = Vec::with_capacity(step.changed.len());
        let mut states = Vec::with_capacity(step.changed.len());
        let mut old = Vec::with_capacity(step.changed.len());
        for &i in &step.changed {
            let item = step.campaign.items().get(i).ok_or(StoreError::Corrupt)?;
            let state = step
                .campaign
                .item_states()
                .get(i)
                .ok_or(StoreError::Corrupt)?;
            let was = before.item_versions.get(i).ok_or(StoreError::Corrupt)?;
            ids.push(item.id.clone());
            states.push(json(state)?);
            old.push(i64::try_from(*was).map_err(|_| StoreError::Corrupt)?);
        }
        let n = tx
            .execute(UPDATE_ITEMS, &[&tenant, &id, &ids, &states, &old])
            .await
            .map_err(|e| unavailable(&e))?;
        if usize::try_from(n).ok() != Some(step.changed.len()) {
            return Err(StoreError::VersionConflict);
        }
    }
    if step.campaign.status() != before.campaign.status() {
        tx.execute(
            UPDATE_STATUS,
            &[&tenant, &id, &status_text(step.campaign.status())],
        )
        .await
        .map_err(|e| unavailable(&e))?;
    }
    append_history(tx, tenant, id, &step.entries, at).await
}

impl PgCampaignStore {
    /// A store on `pool`. It does not touch the database until used.
    #[must_use]
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }

    /// Creates the tables, guards and role of this crate if missing (the SoD tables too).
    /// Idempotent; safe from several nodes at once.
    ///
    /// # Errors
    /// `access_review_pg.migration_failed`.
    pub async fn prepare(&self) -> AppResult<()> {
        crate::schema::prepare(&self.pool).await
    }

    async fn conn(&self) -> Result<Object, StoreError> {
        self.pool.get().await.map_err(|e| campaign_pool(&e))
    }

    #[allow(clippy::too_many_arguments)]
    async fn decide_tx(
        &self,
        tenant: &str,
        id: &str,
        item: &str,
        by: &str,
        decision: Decision,
        expected_version: u64,
        now: i64,
    ) -> Result<Outcome, StoreError> {
        let mut conn = self.conn().await?;
        let tx = conn.transaction().await.map_err(|e| unavailable(&e))?;
        let stored = load_tx(&tx, tenant, id, true).await?;
        let step = plan_decide(&stored, item, by, decision, expected_version, now)?;
        persist(&tx, tenant, id, &stored, &step, now).await?;
        tx.commit().await.map_err(|e| unavailable(&e))?;
        Ok(step.outcome)
    }

    /// Records a refused decision after its transaction rolled back. Best effort by design:
    /// the refusal already happened and the caller already has the error; a failure here
    /// changes nothing about it.
    async fn record_refusal(&self, tenant: &str, id: &str, entry: HistoryEntry, now: i64) {
        let Ok(mut conn) = self.conn().await else {
            return;
        };
        let Ok(tx) = conn.transaction().await else {
            return;
        };
        let locked = tx.query_opt(LOCK_CAMPAIGN, &[&tenant, &id]).await;
        if !matches!(locked, Ok(Some(_))) {
            return;
        }
        if append_history(&tx, tenant, id, &[entry], now).await.is_ok() {
            let _ = tx.commit().await;
        }
    }
}

impl CampaignStore for PgCampaignStore {
    fn open<'a>(
        &'a self,
        tenant: &'a TenantId,
        spec: CampaignSpec,
        items: Vec<ReviewItem>,
        now: i64,
    ) -> StoreFuture<'a, Result<Outcome, StoreError>> {
        Box::pin(async move {
            check_open(&spec, &items)?;
            let campaign = Campaign::open(spec, items, now)?;
            let tenant = tenant.to_string();
            let id = campaign.spec().id.clone();
            let outcome = Outcome {
                events: campaign.opening_events().to_vec(),
                actions: Vec::new(),
            };
            let spec_json = json(campaign.spec())?;
            let events_json = json(&outcome.events)?;
            let snapshots = campaign
                .items()
                .iter()
                .map(json)
                .collect::<Result<Vec<_>, _>>()?;
            let states = campaign
                .item_states()
                .iter()
                .map(json)
                .collect::<Result<Vec<_>, _>>()?;
            let ids: Vec<String> = campaign.items().iter().map(|i| i.id.clone()).collect();
            let count =
                i32::try_from(ids.len()).map_err(|_| StoreError::InputInvalid("item count"))?;

            let mut conn = self.conn().await?;
            let tx = conn.transaction().await.map_err(|e| unavailable(&e))?;
            tx.execute(
                INSERT_CAMPAIGN,
                &[
                    &tenant,
                    &id,
                    &campaign.spec().title,
                    &campaign.spec().deadline,
                    &count,
                    &spec_json,
                    &events_json,
                    &now,
                ],
            )
            .await
            .map_err(|e| {
                if is_unique(&e) {
                    StoreError::AlreadyExists
                } else {
                    unavailable(&e)
                }
            })?;
            tx.execute(INSERT_ITEMS, &[&tenant, &id, &ids, &snapshots, &states])
                .await
                .map_err(|e| unavailable(&e))?;
            append_history(&tx, &tenant, &id, &entries_of(&outcome), now).await?;
            tx.commit().await.map_err(|e| unavailable(&e))?;
            Ok(outcome)
        })
    }

    fn load<'a>(
        &'a self,
        tenant: &'a TenantId,
        campaign: &'a str,
    ) -> StoreFuture<'a, Result<StoredCampaign, StoreError>> {
        Box::pin(async move {
            check_id(campaign)?;
            let mut conn = self.conn().await?;
            // One snapshot for the campaign row and its items.
            let tx = conn
                .build_transaction()
                .isolation_level(tokio_postgres::IsolationLevel::RepeatableRead)
                .read_only(true)
                .start()
                .await
                .map_err(|e| unavailable(&e))?;
            load_tx(&tx, &tenant.to_string(), campaign, false).await
        })
    }

    fn decide<'a>(
        &'a self,
        tenant: &'a TenantId,
        campaign: &'a str,
        item: &'a str,
        by: &'a str,
        decision: Decision,
        expected_version: u64,
        now: i64,
    ) -> StoreFuture<'a, Result<Outcome, StoreError>> {
        Box::pin(async move {
            check_id(campaign)?;
            let tenant = tenant.to_string();
            let result = self
                .decide_tx(&tenant, campaign, item, by, decision, expected_version, now)
                .await;
            if let Err(error) = &result
                && let Some(entry) = refusal_entry(error, item, by)
            {
                self.record_refusal(&tenant, campaign, entry, now).await;
            }
            result
        })
    }

    fn tick<'a>(
        &'a self,
        tenant: &'a TenantId,
        campaign: &'a str,
        now: i64,
    ) -> StoreFuture<'a, Result<Outcome, StoreError>> {
        Box::pin(async move {
            check_id(campaign)?;
            let tenant = tenant.to_string();
            let mut conn = self.conn().await?;
            let tx = conn.transaction().await.map_err(|e| unavailable(&e))?;
            let stored = load_tx(&tx, &tenant, campaign, true).await?;
            let step = plan_tick(&stored, now);
            if step.changed.is_empty() && step.entries.is_empty() {
                return Ok(step.outcome);
            }
            persist(&tx, &tenant, campaign, &stored, &step, now).await?;
            tx.commit().await.map_err(|e| unavailable(&e))?;
            Ok(step.outcome)
        })
    }

    fn list<'a>(
        &'a self,
        tenant: &'a TenantId,
        after: Option<&'a str>,
        limit: u32,
    ) -> StoreFuture<'a, Result<Vec<CampaignSummary>, StoreError>> {
        Box::pin(async move {
            check_limit(limit)?;
            if let Some(after) = after {
                check_id(after)?;
            }
            let conn = self.conn().await?;
            let rows = conn
                .query(LIST, &[&tenant.to_string(), &after, &i64::from(limit)])
                .await
                .map_err(|e| unavailable(&e))?;
            rows.iter()
                .map(|r| {
                    Ok(CampaignSummary {
                        id: r.get(0),
                        title: r.get(1),
                        status: parse_status(&r.get::<_, String>(2))?,
                        deadline: r.get(3),
                        items: usize::try_from(r.get::<_, i32>(4))
                            .map_err(|_| StoreError::Corrupt)?,
                    })
                })
                .collect()
        })
    }

    fn history<'a>(
        &'a self,
        tenant: &'a TenantId,
        campaign: &'a str,
        after_seq: u64,
        limit: u32,
    ) -> StoreFuture<'a, Result<Vec<HistoryRecord>, StoreError>> {
        Box::pin(async move {
            check_limit(limit)?;
            check_id(campaign)?;
            let tenant = tenant.to_string();
            let after = i64::try_from(after_seq).unwrap_or(i64::MAX);
            let conn = self.conn().await?;
            if conn
                .query_opt(EXISTS, &[&tenant, &campaign])
                .await
                .map_err(|e| unavailable(&e))?
                .is_none()
            {
                return Err(StoreError::NotFound);
            }
            let rows = conn
                .query(HISTORY, &[&tenant, &campaign, &after, &i64::from(limit)])
                .await
                .map_err(|e| unavailable(&e))?;
            rows.iter()
                .map(|r| {
                    Ok(HistoryRecord {
                        seq: u64::try_from(r.get::<_, i64>(0)).map_err(|_| StoreError::Corrupt)?,
                        entry: unjson(&r.get::<_, String>(2))?,
                        at: r.get(3),
                    })
                })
                .collect()
        })
    }
}
