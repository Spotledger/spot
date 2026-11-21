//! The receiver's store: conversations, entries taken into sequence, parked messages, the seen
//! table (the dedupe) and the notices, changed together in one transaction.
//!
//! # One transaction per receive
//!
//! [`InboxStore::commit`] applies a whole plan, **the delivery notices included**, in a single
//! transaction ([`apply_commit`]): the entries, the parking changes, the rejections, the
//! conversation cursor and the notices commit together or not at all. There is no code path that
//! appends a notice outside the inbox transaction, so a crash between the inbox write and the
//! notice is impossible by construction; the kill tests prove it by failing the commit and by
//! terminating the backend before it.
//!
//! The first statement locks the conversation's cursor row (`FOR UPDATE`), so two receives for one
//! conversation take turns and the loser sees `Conflict`, which `Inbox::receive` retries against
//! the new state. The notice counter is locked last, which keeps the serialised tail short.

use spot_link_exchange::{
    Conversation, ConversationView, EntryStatus, InboxCommit, InboxEntry, InboxStore, LinkUsage,
    MessageId, NoticeDraft, ParkedEntry, PurgedPayload, RejectCode, Result, SeenKind,
};
use spot_tenant_link::{LinkId, SignatureBytes};
use tokio_postgres::Transaction;
use uuid::Uuid;

use crate::codec::{corrupt, from_json, json, to_i64, to_u64};
use crate::error::{conflict, q};
use crate::scope::Scope;

/// The PostgreSQL [`InboxStore`].
#[derive(Clone)]
pub struct PgInbox {
    scope: Scope,
}

impl std::fmt::Debug for PgInbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgInbox").finish_non_exhaustive()
    }
}

fn status_class(status: EntryStatus) -> &'static str {
    match status {
        EntryStatus::PendingApply => "pending_apply",
        EntryStatus::Applied => "applied",
        EntryStatus::Failed(_) => "failed",
    }
}

fn link_of(uuid: Uuid) -> Result<LinkId> {
    LinkId::from_uuid(uuid).map_err(|_| corrupt())
}

/// Appends one notice. The counter row is locked until the transaction ends, so numbers are
/// handed out in commit order without gaps.
async fn append_notice(tx: &Transaction<'_>, draft: &NoticeDraft) -> Result<()> {
    let row = tx
        .query_one(
            "UPDATE spot_link_cursor SET value = value + 1 WHERE name = 'notice' RETURNING value",
            &[],
        )
        .await
        .map_err(q)?;
    let cursor: i64 = row.get(0);
    tx.execute(
        "INSERT INTO spot_link_notice \
            (cursor, link, topic, message_id, conversation, source, received_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
        &[
            &cursor,
            &draft.link.as_uuid(),
            &draft.topic.as_str(),
            &draft.message_id.as_uuid(),
            &draft.conversation.as_str(),
            &draft.source.canonical(),
            &to_i64(draft.received_at)?,
        ],
    )
    .await
    .map_err(q)?;
    Ok(())
}

async fn upsert_seen(
    tx: &Transaction<'_>,
    id: MessageId,
    link: LinkId,
    kind: &str,
    detail: &str,
) -> Result<()> {
    // The dedupe key is `(link, id)`, so a row of another link is never touched: `ON CONFLICT`
    // cannot reach it. A row this link may not replace (anything but its own `parked` row) makes
    // the upsert affect no row, and that is a conflict, not a silent overwrite.
    let changed = tx
        .execute(
            "INSERT INTO spot_link_seen (id, link, kind, detail) VALUES ($1, $2, $3, $4) \
             ON CONFLICT (link, id) DO UPDATE SET kind = EXCLUDED.kind, detail = EXCLUDED.detail \
             WHERE spot_link_seen.kind = 'parked' \
               AND spot_link_seen.link = EXCLUDED.link",
            &[&id.as_uuid(), &link.as_uuid(), &kind, &detail],
        )
        .await
        .map_err(q)?;
    if changed == 0 {
        return Err(conflict("a message in the plan is already known"));
    }
    Ok(())
}

/// Applies `commit` inside `tx`, which must already be scoped to the tenant's schema. The
/// caller commits or rolls back; nothing here commits.
///
/// This is the one place the inbox rows and the notices are written, so they can only ever
/// change together.
///
/// # Errors
/// `Conflict` when the conversation moved on, a message in the plan is already known, or the
/// transaction lost a race; a store failure otherwise.
pub async fn apply_commit(tx: &Transaction<'_>, commit: &InboxCommit) -> Result<()> {
    let link = commit.link.as_uuid();
    let conversation = commit.conversation.as_str();
    tx.execute(
        "INSERT INTO spot_link_inbox_conv (link, conversation, next_seq) VALUES ($1, $2, 1) \
         ON CONFLICT (link, conversation) DO NOTHING",
        &[&link, &conversation],
    )
    .await
    .map_err(q)?;
    let row = tx
        .query_one(
            "SELECT next_seq FROM spot_link_inbox_conv WHERE link = $1 AND conversation = $2 FOR UPDATE",
            &[&link, &conversation],
        )
        .await
        .map_err(q)?;
    if to_u64(row.get(0))? != commit.expected_next {
        return Err(conflict("the conversation moved on"));
    }

    let leaving: Vec<Uuid> = commit.unpark.iter().map(MessageId::as_uuid).collect();
    let planned: Vec<Uuid> = commit
        .store
        .iter()
        .map(|e| e.message.id.as_uuid())
        .chain(commit.rejected.iter().map(|(id, _, _)| id.as_uuid()))
        .chain(commit.park.iter().map(|p| p.message.id.as_uuid()))
        .collect();
    let known = tx
        .query(
            "SELECT id, kind FROM spot_link_seen WHERE link = $1 AND id = ANY($2)",
            &[&link, &planned],
        )
        .await
        .map_err(q)?;
    for known in &known {
        let id: Uuid = known.get(0);
        let kind: String = known.get(1);
        let parks_it = commit.park.iter().any(|p| p.message.id.as_uuid() == id);
        if parks_it || kind != "parked" || !leaving.contains(&id) {
            return Err(conflict("a message in the plan is already known"));
        }
    }

    tx.execute(
        "DELETE FROM spot_link_parked WHERE id = ANY($1) AND link = $2 AND conversation = $3",
        &[&leaving, &link, &conversation],
    )
    .await
    .map_err(q)?;
    // A parked message that leaves without being stored or refused was a tombstone: its slot is
    // consumed and a repeat is answered as applied.
    let settled: Vec<Uuid> = commit
        .store
        .iter()
        .map(|e| e.message.id.as_uuid())
        .chain(commit.rejected.iter().map(|(id, _, _)| id.as_uuid()))
        .collect();
    let applied = json(&EntryStatus::Applied)?;
    for id in leaving.iter().filter(|id| !settled.contains(id)) {
        tx.execute(
            "UPDATE spot_link_seen SET kind = 'stored', detail = $3 \
             WHERE link = $1 AND id = $2",
            &[&link, id, &applied],
        )
        .await
        .map_err(q)?;
    }
    for parked in &commit.park {
        tx.execute(
            "INSERT INTO spot_link_parked (id, link, conversation, seq, parked_at, entry) \
             VALUES ($1, $2, $3, $4, $5, $6::text::jsonb)",
            &[
                &parked.message.id.as_uuid(),
                &link,
                &conversation,
                &to_i64(parked.message.seq)?,
                &to_i64(parked.parked_at)?,
                &json(parked)?,
            ],
        )
        .await
        .map_err(q)?;
        upsert_seen(tx, parked.message.id, commit.link, "parked", "").await?;
    }
    for entry in &commit.store {
        tx.execute(
            "INSERT INTO spot_link_inbox (id, link, conversation, seq, status, received_at, entry) \
             VALUES ($1, $2, $3, $4, $5, $6, $7::text::jsonb)",
            &[
                &entry.message.id.as_uuid(),
                &link,
                &conversation,
                &to_i64(entry.message.seq)?,
                &status_class(entry.status),
                &to_i64(entry.received_at)?,
                &json(entry)?,
            ],
        )
        .await
        .map_err(q)?;
        upsert_seen(
            tx,
            entry.message.id,
            commit.link,
            "stored",
            &json(&entry.status)?,
        )
        .await?;
    }
    for (id, rejected_link, code) in &commit.rejected {
        upsert_seen(tx, *id, *rejected_link, "rejected", &json(code)?).await?;
    }
    tx.execute(
        "UPDATE spot_link_inbox_conv SET next_seq = $3 WHERE link = $1 AND conversation = $2",
        &[&link, &conversation, &to_i64(commit.next_seq)?],
    )
    .await
    .map_err(q)?;
    // Last, and in the same transaction: the notices.
    for notice in &commit.notices {
        append_notice(tx, notice).await?;
    }
    Ok(())
}

impl PgInbox {
    pub(crate) fn new(scope: Scope) -> Self {
        Self { scope }
    }

    async fn usage_async(&self, link: LinkId) -> Result<LinkUsage> {
        let mut conn = self.scope.conn().await?;
        let tx = self.scope.begin(&mut conn).await?;
        let row = tx
            .query_one(
                "SELECT (SELECT count(*) FROM spot_link_parked WHERE link = $1), \
                        (SELECT COALESCE(sum(octet_length(entry #>> '{message,payload}')), 0) \
                           FROM spot_link_parked WHERE link = $1), \
                        (SELECT count(*) FROM spot_link_inbox_conv WHERE link = $1)",
                &[&link.as_uuid()],
            )
            .await
            .map_err(q)?;
        Ok(LinkUsage {
            parked: to_u64(row.get(0))?,
            parked_bytes: to_u64(row.get(1))?,
            conversations: to_u64(row.get(2))?,
        })
    }

    async fn seen_async(&self, link: LinkId, id: MessageId) -> Result<Option<SeenKind>> {
        let mut conn = self.scope.conn().await?;
        let tx = self.scope.begin(&mut conn).await?;
        let row = tx
            .query_opt(
                "SELECT kind, detail FROM spot_link_seen WHERE link = $1 AND id = $2",
                &[&link.as_uuid(), &id.as_uuid()],
            )
            .await
            .map_err(q)?;
        let Some(row) = row else { return Ok(None) };
        let kind: String = row.get(0);
        let detail: String = row.get(1);
        Ok(Some(match kind.as_str() {
            "stored" => SeenKind::Stored(from_json::<EntryStatus>(&detail)?),
            "parked" => SeenKind::Parked,
            "rejected" => SeenKind::Rejected(from_json::<RejectCode>(&detail)?),
            _ => return Err(corrupt()),
        }))
    }

    async fn conversation_async(
        &self,
        link: LinkId,
        conversation: Conversation,
    ) -> Result<ConversationView> {
        let mut conn = self.scope.conn().await?;
        let tx = self.scope.begin(&mut conn).await?;
        let next = tx
            .query_opt(
                "SELECT next_seq FROM spot_link_inbox_conv WHERE link = $1 AND conversation = $2",
                &[&link.as_uuid(), &conversation.as_str()],
            )
            .await
            .map_err(q)?
            .map_or(Ok(1), |row| to_u64(row.get(0)))?;
        let rows = tx
            .query(
                "SELECT entry::text FROM spot_link_parked WHERE link = $1 AND conversation = $2 \
                 ORDER BY seq",
                &[&link.as_uuid(), &conversation.as_str()],
            )
            .await
            .map_err(q)?;
        let parked = rows
            .iter()
            .map(|row| from_json::<ParkedEntry>(&row.get::<_, String>(0)))
            .collect::<Result<Vec<_>>>()?;
        Ok(ConversationView {
            next_seq: next.max(1),
            parked,
        })
    }

    async fn commit_async(&self, commit: InboxCommit) -> Result<()> {
        let mut conn = self.scope.conn().await?;
        let tx = self.scope.begin(&mut conn).await?;
        apply_commit(&tx, &commit).await?;
        tx.commit().await.map_err(q)
    }

    async fn finish_async(
        &self,
        id: MessageId,
        link: LinkId,
        status: EntryStatus,
        notice: Option<NoticeDraft>,
    ) -> Result<Option<InboxEntry>> {
        let mut conn = self.scope.conn().await?;
        let tx = self.scope.begin(&mut conn).await?;
        let row = tx
            .query_opt(
                "SELECT status, entry::text FROM spot_link_inbox \
                 WHERE link = $1 AND id = $2 FOR UPDATE",
                &[&link.as_uuid(), &id.as_uuid()],
            )
            .await
            .map_err(q)?;
        let Some(row) = row else { return Ok(None) };
        let current: String = row.get(0);
        if current != "pending_apply" {
            return Ok(None);
        }
        let mut entry: InboxEntry = from_json(&row.get::<_, String>(1))?;
        entry.status = status;
        tx.execute(
            "UPDATE spot_link_inbox SET status = $3, entry = $4::text::jsonb \
             WHERE link = $1 AND id = $2",
            &[
                &link.as_uuid(),
                &id.as_uuid(),
                &status_class(status),
                &json(&entry)?,
            ],
        )
        .await
        .map_err(q)?;
        tx.execute(
            "UPDATE spot_link_seen SET detail = $3 \
             WHERE link = $1 AND id = $2 AND kind = 'stored'",
            &[&link.as_uuid(), &id.as_uuid(), &json(&status)?],
        )
        .await
        .map_err(q)?;
        if let Some(notice) = &notice {
            append_notice(&tx, notice).await?;
        }
        tx.commit().await.map_err(q)?;
        Ok(Some(entry))
    }

    async fn pending_async(&self, limit: usize) -> Result<Vec<(LinkId, InboxEntry)>> {
        let mut conn = self.scope.conn().await?;
        let tx = self.scope.begin(&mut conn).await?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows = tx
            .query(
                "SELECT link, entry::text FROM spot_link_inbox WHERE status = 'pending_apply' \
                 ORDER BY link, conversation COLLATE \"C\", seq LIMIT $1",
                &[&limit],
            )
            .await
            .map_err(q)?;
        rows.iter()
            .map(|row| Ok((link_of(row.get(0))?, from_json(&row.get::<_, String>(1))?)))
            .collect()
    }

    async fn parked_async(&self) -> Result<Vec<(LinkId, ParkedEntry)>> {
        let mut conn = self.scope.conn().await?;
        let tx = self.scope.begin(&mut conn).await?;
        let rows = tx
            .query(
                "SELECT link, entry::text FROM spot_link_parked \
                 ORDER BY link, conversation COLLATE \"C\", seq",
                &[],
            )
            .await
            .map_err(q)?;
        rows.iter()
            .map(|row| Ok((link_of(row.get(0))?, from_json(&row.get::<_, String>(1))?)))
            .collect()
    }

    async fn entry_async(&self, link: LinkId, id: MessageId) -> Result<Option<InboxEntry>> {
        let mut conn = self.scope.conn().await?;
        let tx = self.scope.begin(&mut conn).await?;
        let row = tx
            .query_opt(
                "SELECT entry::text FROM spot_link_inbox WHERE link = $1 AND id = $2",
                &[&link.as_uuid(), &id.as_uuid()],
            )
            .await
            .map_err(q)?;
        row.map(|row| from_json(&row.get::<_, String>(0)))
            .transpose()
    }

    /// Opens a transaction scoped to the tenant's schema for a caller that wants to call
    /// [`apply_commit`] itself (the kill tests do, to fail the transaction on purpose).
    ///
    /// # Errors
    /// A store failure when no connection is available.
    pub async fn connection(&self) -> Result<deadpool_postgres::Object> {
        self.scope.conn().await
    }

    /// Scopes `conn`'s next transaction to the tenant's schema.
    ///
    /// # Errors
    /// A store failure.
    pub async fn begin<'c>(
        &self,
        conn: &'c mut deadpool_postgres::Object,
    ) -> Result<deadpool_postgres::Transaction<'c>> {
        self.scope.begin(conn).await
    }
}

impl InboxStore for PgInbox {
    fn seen(&self, link: &LinkId, id: &MessageId) -> Result<Option<SeenKind>> {
        self.scope.run(self.seen_async(*link, *id))
    }

    fn usage(&self, link: &LinkId) -> Result<LinkUsage> {
        self.scope.run(self.usage_async(*link))
    }

    fn conversation(&self, link: &LinkId, conversation: &Conversation) -> Result<ConversationView> {
        self.scope
            .run(self.conversation_async(*link, conversation.clone()))
    }

    fn commit(&self, commit: InboxCommit) -> Result<()> {
        self.scope.run(self.commit_async(commit))
    }

    fn finish_record(
        &self,
        id: &MessageId,
        link: &LinkId,
        status: EntryStatus,
        notice: Option<NoticeDraft>,
    ) -> Result<Option<InboxEntry>> {
        self.scope
            .run(self.finish_async(*id, *link, status, notice))
    }

    fn pending_records(&self, limit: usize) -> Result<Vec<(LinkId, InboxEntry)>> {
        self.scope.run(self.pending_async(limit))
    }

    fn parked(&self) -> Result<Vec<(LinkId, ParkedEntry)>> {
        self.scope.run(self.parked_async())
    }

    fn entry(&self, link: &LinkId, id: &MessageId) -> Result<Option<InboxEntry>> {
        self.scope.run(self.entry_async(*link, *id))
    }

    fn purged(&self, link: &LinkId, id: &MessageId) -> Result<Option<PurgedPayload>> {
        self.scope.run(self.purged_async(*link, *id))
    }

    fn purge_applied(&self, older_than: u64, now: u64, limit: usize) -> Result<u64> {
        self.scope.run(self.purge_async(older_than, now, limit))
    }
}

impl PgInbox {
    async fn purged_async(&self, link: LinkId, id: MessageId) -> Result<Option<PurgedPayload>> {
        let mut conn = self.scope.conn().await?;
        let tx = self.scope.begin(&mut conn).await?;
        let row = tx
            .query_opt(
                "SELECT entry #>> '{purged,digest}', entry #>> '{purged,signature}', \
                        entry #>> '{purged,purged_at}' \
                 FROM spot_link_inbox WHERE link = $1 AND id = $2 AND purged_at IS NOT NULL",
                &[&link.as_uuid(), &id.as_uuid()],
            )
            .await
            .map_err(q)?;
        let Some(row) = row else { return Ok(None) };
        let digest: String = row.get(0);
        let signature: String = row.get(1);
        let purged_at: String = row.get(2);
        if digest.len() != PurgedPayload::DIGEST_HEX_LEN {
            return Err(corrupt());
        }
        Ok(Some(PurgedPayload {
            link,
            id,
            digest,
            signature: SignatureBytes::try_from(signature).map_err(|_| corrupt())?,
            purged_at: purged_at.parse().map_err(|_| corrupt())?,
        }))
    }

    /// Empties the payload and writes the digest and signature it had, in **one** transaction, so
    /// a purged row never exists without the evidence that it is the message the sender signed
    ///.
    async fn purge_async(&self, older_than: u64, now: u64, limit: usize) -> Result<u64> {
        let mut conn = self.scope.conn().await?;
        let tx = self.scope.begin(&mut conn).await?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let purged = tx
            .execute(
                "WITH due AS (SELECT link, id, \
                                     encode(sha256(convert_to(entry #>> '{message,payload}', 'UTF8')), \
                                            'hex') AS digest, \
                                     entry #>> '{message,sender_signature}' AS signature \
                              FROM spot_link_inbox \
                              WHERE status = 'applied' AND purged_at IS NULL AND received_at <= $1 \
                              ORDER BY received_at LIMIT $2 FOR UPDATE SKIP LOCKED) \
                 UPDATE spot_link_inbox i \
                    SET entry = jsonb_set(i.entry, '{message,payload}', to_jsonb($3::text)) || \
                                jsonb_build_object('purged', jsonb_build_object( \
                                    'digest', due.digest, \
                                    'signature', due.signature, \
                                    'purged_at', $4::bigint)), \
                        purged_at = $4 \
                 FROM due WHERE i.link = due.link AND i.id = due.id",
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
