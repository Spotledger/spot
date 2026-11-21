//! The sender's store: one row per message with its state, plus the producer events.
//!
//! Each row carries the whole [`OutboxEntry`] as a document and, beside it, the columns a query
//! needs (conversation order, state, whether the entry is still live, the slot filler's id and
//! the optimistic version). `update` is a compare-and-set on the version; `insert` is a guarded
//! insert whose condition is "this is the conversation's next sequence number".

use spot_link_exchange::{
    Conversation, Error, MessageId, OutboxEntry, OutboxStore, ProducerEvent, Result,
};
use spot_tenant_link::LinkId;
use uuid::Uuid;

use crate::codec::{from_json, json, to_i64, to_u64};
use crate::error::{conflict, q};
use crate::scope::Scope;

/// The PostgreSQL [`OutboxStore`].
#[derive(Clone)]
pub struct PgOutbox {
    scope: Scope,
}

impl std::fmt::Debug for PgOutbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgOutbox").finish_non_exhaustive()
    }
}

impl PgOutbox {
    pub(crate) fn new(scope: Scope) -> Self {
        Self { scope }
    }

    async fn insert_async(&self, mut entry: OutboxEntry) -> Result<()> {
        entry.version = 1;
        let filler = entry.filler.as_ref().map(|f| f.message.id.as_uuid());
        let doc = json(&entry)?;
        let mut conn = self.scope.conn().await?;
        let tx = self.scope.begin(&mut conn).await?;
        let inserted = tx
            .execute(
                "INSERT INTO spot_link_outbox \
                    (id, link, conversation, seq, state, state_at, live, filler_id, version, entry) \
                 SELECT $1::uuid, $2::uuid, $3::text, $4::bigint, $5::text, $6::bigint, \
                        $7::boolean, $8::uuid, 1, $9::text::jsonb \
                 WHERE $4::bigint = (SELECT COALESCE(MAX(seq), 0) + 1 FROM spot_link_outbox \
                                     WHERE link = $2::uuid AND conversation = $3::text)",
                &[
                    &entry.message.id.as_uuid(),
                    &entry.message.link.as_uuid(),
                    &entry.message.conversation.as_str(),
                    &to_i64(entry.message.seq)?,
                    &entry.state.as_str(),
                    &to_i64(entry.state_at)?,
                    &entry.is_live(),
                    &filler,
                    &doc,
                ],
            )
            .await
            .map_err(q)?;
        if inserted == 0 {
            return Err(conflict(
                "the sequence number is not the conversation's next",
            ));
        }
        tx.commit().await.map_err(q)
    }

    async fn find_async(&self, id: MessageId) -> Result<Option<OutboxEntry>> {
        let mut conn = self.scope.conn().await?;
        let tx = self.scope.begin(&mut conn).await?;
        let row = tx
            .query_opt(
                "SELECT entry::text FROM spot_link_outbox WHERE id = $1 OR filler_id = $1",
                &[&id.as_uuid()],
            )
            .await
            .map_err(q)?;
        row.map(|row| from_json(&row.get::<_, String>(0)))
            .transpose()
    }

    async fn update_async(&self, mut entry: OutboxEntry) -> Result<()> {
        let expected = entry.version;
        entry.version = expected
            .checked_add(1)
            .ok_or_else(|| conflict("the entry version overflowed"))?;
        let filler = entry.filler.as_ref().map(|f| f.message.id.as_uuid());
        let doc = json(&entry)?;
        let mut conn = self.scope.conn().await?;
        let tx = self.scope.begin(&mut conn).await?;
        let changed = tx
            .execute(
                "UPDATE spot_link_outbox SET state = $2, state_at = $3, live = $4, filler_id = $5, \
                        version = $6, entry = $7::text::jsonb \
                 WHERE id = $1 AND version = $8",
                &[
                    &entry.message.id.as_uuid(),
                    &entry.state.as_str(),
                    &to_i64(entry.state_at)?,
                    &entry.is_live(),
                    &filler,
                    &to_i64(entry.version)?,
                    &doc,
                    &to_i64(expected)?,
                ],
            )
            .await
            .map_err(q)?;
        if changed == 0 {
            let exists = tx
                .query_opt(
                    "SELECT 1 FROM spot_link_outbox WHERE id = $1",
                    &[&entry.message.id.as_uuid()],
                )
                .await
                .map_err(q)?
                .is_some();
            return Err(if exists {
                conflict("the entry changed since it was read")
            } else {
                Error::NotFound
            });
        }
        tx.commit().await.map_err(q)
    }

    /// The version-guarded update and the producer event in **one** transaction: a crash cannot
    /// leave the new state without its event, and a lost race writes neither.
    async fn settle_async(
        &self,
        mut entry: OutboxEntry,
        event: Option<ProducerEvent>,
    ) -> Result<()> {
        let expected = entry.version;
        entry.version = expected
            .checked_add(1)
            .ok_or_else(|| conflict("the entry version overflowed"))?;
        let filler = entry.filler.as_ref().map(|f| f.message.id.as_uuid());
        let doc = json(&entry)?;
        let mut conn = self.scope.conn().await?;
        let tx = self.scope.begin(&mut conn).await?;
        let changed = tx
            .execute(
                "UPDATE spot_link_outbox SET state = $2, state_at = $3, live = $4, filler_id = $5, \
                        version = $6, entry = $7::text::jsonb \
                 WHERE id = $1 AND version = $8",
                &[
                    &entry.message.id.as_uuid(),
                    &entry.state.as_str(),
                    &to_i64(entry.state_at)?,
                    &entry.is_live(),
                    &filler,
                    &to_i64(entry.version)?,
                    &doc,
                    &to_i64(expected)?,
                ],
            )
            .await
            .map_err(q)?;
        if changed == 0 {
            let exists = tx
                .query_opt(
                    "SELECT 1 FROM spot_link_outbox WHERE id = $1",
                    &[&entry.message.id.as_uuid()],
                )
                .await
                .map_err(q)?
                .is_some();
            return Err(if exists {
                conflict("the entry changed since it was read")
            } else {
                Error::NotFound
            });
        }
        if let Some(event) = event {
            let doc = json(&event)?;
            tx.execute(
                "WITH n AS (UPDATE spot_link_cursor SET value = value + 1 WHERE name = 'event' \
                            RETURNING value) \
                 INSERT INTO spot_link_outbox_event (pos, event) SELECT value, $1::text::jsonb FROM n",
                &[&doc],
            )
            .await
            .map_err(q)?;
        }
        tx.commit().await.map_err(q)
    }

    async fn last_seq_async(&self, link: LinkId, conversation: Conversation) -> Result<u64> {
        let mut conn = self.scope.conn().await?;
        let tx = self.scope.begin(&mut conn).await?;
        let row = tx
            .query_one(
                "SELECT COALESCE(MAX(seq), 0) FROM spot_link_outbox \
                 WHERE link = $1 AND conversation = $2",
                &[&link.as_uuid(), &conversation.as_str()],
            )
            .await
            .map_err(q)?;
        to_u64(row.get(0))
    }

    async fn live_async(&self, link: Option<LinkId>) -> Result<Vec<OutboxEntry>> {
        let mut conn = self.scope.conn().await?;
        let tx = self.scope.begin(&mut conn).await?;
        let link: Option<Uuid> = link.map(|l| l.as_uuid());
        let rows = tx
            .query(
                "SELECT entry::text FROM spot_link_outbox \
                 WHERE live AND ($1::uuid IS NULL OR link = $1::uuid) \
                 ORDER BY link, conversation COLLATE \"C\", seq",
                &[&link],
            )
            .await
            .map_err(q)?;
        rows.iter()
            .map(|row| from_json(&row.get::<_, String>(0)))
            .collect()
    }

    async fn push_event_async(&self, event: ProducerEvent) -> Result<()> {
        let doc = json(&event)?;
        let mut conn = self.scope.conn().await?;
        let tx = self.scope.begin(&mut conn).await?;
        // The number comes from the counter row, locked until this transaction ends, so event
        // positions are gapless and in commit order (see the crate documentation).
        tx.execute(
            "WITH n AS (UPDATE spot_link_cursor SET value = value + 1 WHERE name = 'event' \
                        RETURNING value) \
             INSERT INTO spot_link_outbox_event (pos, event) SELECT value, $1::text::jsonb FROM n",
            &[&doc],
        )
        .await
        .map_err(q)?;
        tx.commit().await.map_err(q)
    }

    async fn events_from_async(
        &self,
        after: u64,
        limit: usize,
    ) -> Result<Vec<(u64, ProducerEvent)>> {
        let mut conn = self.scope.conn().await?;
        let tx = self.scope.begin(&mut conn).await?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = tx
            .query(
                "SELECT pos, event::text FROM spot_link_outbox_event \
                 WHERE pos > $1 ORDER BY pos LIMIT $2",
                &[&to_i64(after)?, &limit],
            )
            .await
            .map_err(q)?;
        rows.iter()
            .map(|row| Ok((to_u64(row.get(0))?, from_json(&row.get::<_, String>(1))?)))
            .collect()
    }
}

impl OutboxStore for PgOutbox {
    fn insert(&self, entry: OutboxEntry) -> Result<()> {
        self.scope.run(self.insert_async(entry))
    }

    fn find(&self, id: &MessageId) -> Result<Option<OutboxEntry>> {
        self.scope.run(self.find_async(*id))
    }

    fn update(&self, entry: OutboxEntry) -> Result<()> {
        self.scope.run(self.update_async(entry))
    }

    fn last_seq(&self, link: &LinkId, conversation: &Conversation) -> Result<u64> {
        self.scope
            .run(self.last_seq_async(*link, conversation.clone()))
    }

    fn live(&self, link: Option<&LinkId>) -> Result<Vec<OutboxEntry>> {
        self.scope.run(self.live_async(link.copied()))
    }

    fn settle(&self, entry: OutboxEntry, event: Option<ProducerEvent>) -> Result<()> {
        self.scope.run(self.settle_async(entry, event))
    }

    fn push_event(&self, event: ProducerEvent) -> Result<()> {
        self.scope.run(self.push_event_async(event))
    }

    fn events_from(&self, after: u64, limit: usize) -> Result<Vec<(u64, ProducerEvent)>> {
        self.scope.run(self.events_from_async(after, limit))
    }
}

impl PgOutbox {
    /// Empties the payload of settled entries (applied, rejected or expired, with any slot
    /// filler confirmed) whose state is older than `older_than`, keeping the row, its state, its
    /// ids and its times for audit. Returns how many rows were purged; at most `limit` per call.
    ///
    /// # Errors
    /// A store failure.
    pub fn purge_settled(&self, older_than: u64, now: u64, limit: usize) -> Result<u64> {
        self.scope.run(self.purge_async(older_than, now, limit))
    }

    async fn purge_async(&self, older_than: u64, now: u64, limit: usize) -> Result<u64> {
        let mut conn = self.scope.conn().await?;
        let tx = self.scope.begin(&mut conn).await?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let purged = tx
            .execute(
                "WITH due AS (SELECT id FROM spot_link_outbox \
                              WHERE NOT live AND purged_at IS NULL AND state_at <= $1 \
                              ORDER BY state_at LIMIT $2 FOR UPDATE SKIP LOCKED) \
                 UPDATE spot_link_outbox o \
                    SET entry = jsonb_set(o.entry, '{message,payload}', to_jsonb($3::text)), \
                        purged_at = $4 \
                 FROM due WHERE o.id = due.id",
                &[
                    &to_i64(older_than)?,
                    &limit,
                    &spot_link_exchange::TOMBSTONE_PAYLOAD,
                    &to_i64(now)?,
                ],
            )
            .await
            .map_err(q)?;
        tx.commit().await.map_err(q)?;
        Ok(purged)
    }
}
