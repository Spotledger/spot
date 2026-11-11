//! The delivery notice feed (ADR-058 rule 9) over PostgreSQL.
//!
//! Notices are appended only by [`crate::PgInbox`], inside the same transaction as the inbox
//! write they announce; this type is the consumer side. Cursors are gapless and in commit order
//! (see the crate documentation), so a consumer that remembers the last cursor it handled and
//! reads `cursor > bookmark` can never skip a notice that committed late. Delivery is at least
//! once: a consumer that crashes between handling and acknowledging reads the notice again.

use spot_link_exchange::{Conversation, MessageId, Notice, NoticeFeed, Result};
use spot_tenant_link::{LinkId, Party, Topic};
use tokio_postgres::Row;

use crate::codec::{corrupt, to_i64, to_u64};
use crate::error::q;
use crate::scope::Scope;

/// The PostgreSQL [`NoticeFeed`].
#[derive(Clone)]
pub struct PgNoticeFeed {
    scope: Scope,
}

impl std::fmt::Debug for PgNoticeFeed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgNoticeFeed").finish_non_exhaustive()
    }
}

pub(crate) const NOTICE_COLUMNS: &str =
    "cursor, link, topic, message_id, conversation, source, received_at";

pub(crate) fn notice_of(row: &Row) -> Result<Notice> {
    Ok(Notice {
        cursor: to_u64(row.get(0))?,
        link: LinkId::from_uuid(row.get(1)).map_err(|_| corrupt())?,
        topic: Topic::new(row.get::<_, String>(2)).map_err(|_| corrupt())?,
        message_id: MessageId::from_uuid(row.get(3))?,
        conversation: Conversation::new(row.get::<_, String>(4))?,
        source: Party::try_from(row.get::<_, String>(5)).map_err(|_| corrupt())?,
        received_at: to_u64(row.get(6))?,
    })
}

impl PgNoticeFeed {
    pub(crate) fn new(scope: Scope) -> Self {
        Self { scope }
    }

    async fn read_async(&self, after: u64, limit: usize) -> Result<Vec<Notice>> {
        let mut conn = self.scope.conn().await?;
        let tx = self.scope.begin(&mut conn).await?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = tx
            .query(
                &format!(
                    "SELECT {NOTICE_COLUMNS} FROM spot_link_notice WHERE cursor > $1 \
                     ORDER BY cursor LIMIT $2"
                ),
                &[&to_i64(after)?, &limit],
            )
            .await
            .map_err(q)?;
        rows.iter().map(notice_of).collect()
    }

    async fn ack_async(&self, consumer: String, cursor: u64) -> Result<()> {
        let mut conn = self.scope.conn().await?;
        let tx = self.scope.begin(&mut conn).await?;
        tx.execute(
            "INSERT INTO spot_link_notice_ack (consumer, cursor) VALUES ($1, $2) \
             ON CONFLICT (consumer) DO UPDATE \
                SET cursor = GREATEST(spot_link_notice_ack.cursor, EXCLUDED.cursor)",
            &[&consumer, &to_i64(cursor)?],
        )
        .await
        .map_err(q)?;
        tx.commit().await.map_err(q)
    }

    async fn acked_async(&self, consumer: String) -> Result<u64> {
        let mut conn = self.scope.conn().await?;
        let tx = self.scope.begin(&mut conn).await?;
        let row = tx
            .query_opt(
                "SELECT cursor FROM spot_link_notice_ack WHERE consumer = $1",
                &[&consumer],
            )
            .await
            .map_err(q)?;
        row.map_or(Ok(0), |row| to_u64(row.get(0)))
    }
}

impl NoticeFeed for PgNoticeFeed {
    fn read_from(&self, after: u64, limit: usize) -> Result<Vec<Notice>> {
        self.scope.run(self.read_async(after, limit))
    }

    fn ack(&self, consumer: &str, cursor: u64) -> Result<()> {
        self.scope.run(self.ack_async(consumer.to_owned(), cursor))
    }

    fn acked(&self, consumer: &str) -> Result<u64> {
        self.scope.run(self.acked_async(consumer.to_owned()))
    }
}
