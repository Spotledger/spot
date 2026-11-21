//! [`PgQuarantineStore`]: the PostgreSQL [`QuarantineStore`], over `spot_quarantine_item` and
//! `spot_quarantine_payload`.

use deadpool_postgres::{Object, Pool};
use spot_app_api::AppResult;
use spot_quarantine::item::check_payload;
use spot_quarantine::{
    CloseAuthority, Closure, ClosureRecord, ItemFilter, ItemId, NewItem, QuarantineError,
    QuarantineItem, QuarantineStore, RecordRef, State, StoreFuture,
};
use spot_replica_apply::ReceivedRecord;
use spot_sync_protocol::{RecordName, SourceId};
use tokio_postgres::types::ToSql;

use crate::convert::{item_from_row, record_from_row, state_text, to_i64};
use crate::error::{invalid, pg, pool};
use crate::{schema, sql};

/// The quarantine queue in PostgreSQL.
///
/// See the crate documentation for the rules it keeps. Call [`PgQuarantineStore::prepare`] once
/// at start-up; clones share the pool, so a restart is a new store over the same database and
/// sees the same queue.
#[derive(Clone)]
pub struct PgQuarantineStore {
    pool: Pool,
}

impl std::fmt::Debug for PgQuarantineStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgQuarantineStore").finish_non_exhaustive()
    }
}

impl PgQuarantineStore {
    /// A store on `pool`, the tenant's own pool. It does not touch the database until used.
    #[must_use]
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }

    /// Creates the tables, triggers and indexes if missing. Idempotent; safe from several nodes
    /// at once.
    ///
    /// # Errors
    /// `quarantine_pg.migration_failed` when the database cannot apply the DDL.
    pub async fn prepare(&self) -> AppResult<()> {
        schema::prepare(&self.pool).await
    }

    async fn conn(&self) -> Result<Object, QuarantineError> {
        self.pool.get().await.map_err(|e| pool(&e))
    }

    async fn insert_item(
        &self,
        item: NewItem,
        record: &ReceivedRecord,
    ) -> Result<QuarantineItem, QuarantineError> {
        // The item must describe the payload it is stored with: a mismatch would leave a
        // payload that does not belong to the key `find` returns it under.
        if item.source != record.source
            || item.seq != record.seq
            || item.record_ref != RecordRef::of(record)
        {
            return Err(invalid(
                "the item does not describe the record it is stored with",
            ));
        }
        item.check_sizes()?;
        check_payload(record)?;
        let payload = record
            .payload_json()
            .map_err(|_| invalid("the record image could not be serialised"))?;
        let seq = to_i64(item.seq, "sequence")?;
        let version = to_i64(item.record_ref.version, "record version")?;
        let topic = record.topic.as_ref().map(|t| t.as_str().to_owned());

        let mut conn = self.conn().await?;
        let tx = conn.transaction().await.map_err(|e| pg(&e))?;
        let row = tx
            .query_one(
                sql::INSERT_ITEM,
                &[
                    &item.source.as_str(),
                    &seq,
                    &item.record_ref.name.as_str(),
                    &version,
                    &item.record_ref.entity,
                    &item.record_ref.id,
                    &item.reason_code,
                    &item.detail,
                    &item.first_seen,
                ],
            )
            .await
            .map_err(|e| pg(&e))?;
        let id: i64 = row.get(0);
        tx.execute(
            sql::INSERT_PAYLOAD,
            &[&id, &payload, &topic, &record.conversation],
        )
        .await
        .map_err(|e| pg(&e))?;
        tx.commit().await.map_err(|e| pg(&e))?;
        Ok(QuarantineItem {
            id: ItemId(crate::convert::to_u64(id, "item id")?),
            source: item.source,
            seq: item.seq,
            record_ref: item.record_ref,
            reason_code: item.reason_code,
            detail: item.detail,
            first_seen: item.first_seen,
            attempts: 0,
            state: State::Held,
        })
    }

    async fn read_item(&self, id: ItemId) -> Result<Option<QuarantineItem>, QuarantineError> {
        let Ok(id) = i64::try_from(id.0) else {
            return Ok(None);
        };
        let conn = self.conn().await?;
        let row = conn
            .query_opt(sql::SELECT_ITEM, &[&id])
            .await
            .map_err(|e| pg(&e))?;
        row.as_ref().map(item_from_row).transpose()
    }

    async fn read_closure(&self, id: ItemId) -> Result<Option<ClosureRecord>, QuarantineError> {
        let Ok(id) = i64::try_from(id.0) else {
            return Ok(None);
        };
        let conn = self.conn().await?;
        let row = conn
            .query_opt(sql::SELECT_CLOSURE, &[&id])
            .await
            .map_err(|e| pg(&e))?;
        Ok(row.map(|row| ClosureRecord {
            by: row.get(0),
            at: row.get(1),
        }))
    }

    async fn read_record(&self, id: ItemId) -> Result<Option<ReceivedRecord>, QuarantineError> {
        let Ok(id) = i64::try_from(id.0) else {
            return Ok(None);
        };
        let conn = self.conn().await?;
        let row = conn
            .query_opt(sql::SELECT_PAYLOAD, &[&id])
            .await
            .map_err(|e| pg(&e))?;
        row.as_ref().map(record_from_row).transpose()
    }

    async fn write_item(&self, item: &QuarantineItem) -> Result<(), QuarantineError> {
        let not_found = QuarantineError::NotFound { id: item.id };
        let Ok(id) = i64::try_from(item.id.0) else {
            return Err(not_found);
        };
        // An update never closes an item (that is `close`) and never carries a closure.
        if !item.state.is_open() {
            return Err(QuarantineError::ItemClosed { id: item.id });
        }
        let sizes = NewItem {
            source: item.source.clone(),
            seq: item.seq,
            record_ref: item.record_ref.clone(),
            reason_code: item.reason_code.clone(),
            detail: item.detail.clone(),
            first_seen: item.first_seen,
        };
        sizes.check_sizes()?;
        let seq = to_i64(item.seq, "sequence")?;
        let version = to_i64(item.record_ref.version, "record version")?;
        let attempts =
            i32::try_from(item.attempts).map_err(|_| invalid("attempts is too large to store"))?;
        let state = state_text(item.state);
        let conn = self.conn().await?;
        let changed = conn
            .execute(
                sql::UPDATE_ITEM,
                &[
                    &id,
                    &item.reason_code,
                    &item.detail,
                    &attempts,
                    &state,
                    &item.source.as_str(),
                    &seq,
                    &item.record_ref.name.as_str(),
                    &version,
                    &item.record_ref.entity,
                    &item.record_ref.id,
                    &item.first_seen,
                ],
            )
            .await
            .map_err(|e| pg(&e))?;
        if changed == 1 {
            return Ok(());
        }
        // No row changed: an unknown id, a closed item, or a change to the identity.
        let row = conn
            .query_opt(sql::ITEM_STATE, &[&id])
            .await
            .map_err(|e| pg(&e))?;
        match row {
            None => Err(not_found),
            Some(row) => {
                let stored: String = row.get(0);
                if !crate::convert::state_from(&stored)?.is_open() {
                    Err(QuarantineError::ItemClosed { id: item.id })
                } else {
                    Err(invalid("an item's identity cannot change"))
                }
            }
        }
    }

    async fn close_item(
        &self,
        id: ItemId,
        closure: Closure,
        authority: CloseAuthority,
    ) -> Result<QuarantineItem, QuarantineError> {
        if authority.item() != id || authority.outcome() != closure.outcome {
            return Err(QuarantineError::Denied {
                access: "close",
                id,
            });
        }
        let Ok(raw) = i64::try_from(id.0) else {
            return Err(QuarantineError::NotFound { id });
        };
        let state = state_text(closure.outcome.state());
        let conn = self.conn().await?;
        let row = conn
            .query_opt(sql::CLOSE_ITEM, &[&raw, &state, &closure.by, &closure.at])
            .await
            .map_err(|e| pg(&e))?;
        if let Some(row) = row {
            return item_from_row(&row);
        }
        let row = conn
            .query_opt(sql::ITEM_STATE, &[&raw])
            .await
            .map_err(|e| pg(&e))?;
        Err(match row {
            None => QuarantineError::NotFound { id },
            Some(_) => QuarantineError::ItemClosed { id },
        })
    }

    async fn read_list(&self, filter: &ItemFilter) -> Result<Vec<QuarantineItem>, QuarantineError> {
        let state = filter.state.map(state_text);
        let reason = filter.reason_code.as_deref();
        let source = filter.source.as_ref().map(SourceId::as_str);
        let mut params: Vec<&(dyn ToSql + Sync)> = Vec::new();
        if let Some(state) = &state {
            params.push(state);
        }
        if let Some(reason) = &reason {
            params.push(reason);
        }
        if let Some(source) = &source {
            params.push(source);
        }
        let statement = sql::list_sql(state.is_some(), reason.is_some(), source.is_some());
        let conn = self.conn().await?;
        let rows = conn
            .query(statement.as_str(), &params)
            .await
            .map_err(|e| pg(&e))?;
        rows.iter().map(item_from_row).collect()
    }

    async fn read_open(
        &self,
        source: &SourceId,
        name: &RecordName,
        version: u64,
    ) -> Result<Option<QuarantineItem>, QuarantineError> {
        // A version past `i64` cannot be stored, so no item can have it.
        let Ok(version) = i64::try_from(version) else {
            return Ok(None);
        };
        let conn = self.conn().await?;
        let row = conn
            .query_opt(
                sql::FIND_OPEN,
                &[&source.as_str(), &name.as_str(), &version],
            )
            .await
            .map_err(|e| pg(&e))?;
        row.as_ref().map(item_from_row).transpose()
    }
}

impl QuarantineStore for PgQuarantineStore {
    fn insert<'a>(
        &'a self,
        item: NewItem,
        record: ReceivedRecord,
    ) -> StoreFuture<'a, QuarantineItem> {
        Box::pin(async move { self.insert_item(item, &record).await })
    }

    fn item<'a>(&'a self, id: ItemId) -> StoreFuture<'a, Option<QuarantineItem>> {
        Box::pin(self.read_item(id))
    }

    fn record<'a>(&'a self, id: ItemId) -> StoreFuture<'a, Option<ReceivedRecord>> {
        Box::pin(self.read_record(id))
    }

    fn update<'a>(&'a self, item: &'a QuarantineItem) -> StoreFuture<'a, ()> {
        Box::pin(self.write_item(item))
    }

    fn close<'a>(
        &'a self,
        id: ItemId,
        closure: Closure,
        authority: CloseAuthority,
    ) -> StoreFuture<'a, QuarantineItem> {
        Box::pin(self.close_item(id, closure, authority))
    }

    fn closure<'a>(&'a self, id: ItemId) -> StoreFuture<'a, Option<ClosureRecord>> {
        Box::pin(self.read_closure(id))
    }

    fn list<'a>(&'a self, filter: &'a ItemFilter) -> StoreFuture<'a, Vec<QuarantineItem>> {
        Box::pin(self.read_list(filter))
    }

    fn find<'a>(
        &'a self,
        source: &'a SourceId,
        name: &'a RecordName,
        version: u64,
    ) -> StoreFuture<'a, Option<QuarantineItem>> {
        Box::pin(self.read_open(source, name, version))
    }
}
