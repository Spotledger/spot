//! [`PgConversationStore`]: the PostgreSQL `ConversationStore`, through the shared bridge.

use std::sync::Arc;
use std::time::Duration;

use deadpool_postgres::Pool;
use spot_bpm_conversations::record::MAX_REMEMBERED_REJECTIONS;
use spot_bpm_conversations::{ConversationError, ConversationRecord, ConversationStore, Result};
use spot_link_exchange::{Conversation, MessageId, RejectCode};
use spot_pg_bridge::{BoxFuture, Bridge, PooledConn};
use spot_tenant_link::LinkId;

use crate::codec::{code_from, code_name, encode, record_from_row};
use crate::error::{SetupError, corrupt, from_bridge, pg};
use crate::{schema, sql};

/// Deadline of a read.
pub const READ_DEADLINE: Duration = Duration::from_secs(5);
/// Deadline of a write (one statement or one short transaction).
pub const WRITE_DEADLINE: Duration = Duration::from_secs(10);
/// Deadline of the table creation.
pub const PREPARE_DEADLINE: Duration = Duration::from_secs(30);

/// The longest a tenant name may be.
const MAX_TENANT_LEN: usize = 128;

/// The conversation records in PostgreSQL. Build it with [`PgConversationStore::new`], create the
/// tables once with [`PgConversationStore::prepare`], then give it to the conversation driver
/// as the `ConversationStore`.
///
/// The store is bound to **one tenant** for its whole life: the tenant is stamped on every row
/// and filtered on in every statement, and no method takes one.
///
/// **Call it from a plain thread, never from inside a tokio runtime** (see the crate docs): a
/// call from a runtime thread is refused with `bpm_conversations.store_failed`, never run.
pub struct PgConversationStore {
    bridge: Arc<Bridge>,
    tenant: Arc<str>,
}

impl std::fmt::Debug for PgConversationStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgConversationStore")
            .field("tenant", &self.tenant)
            .finish_non_exhaustive()
    }
}

impl PgConversationStore {
    /// Builds the store for `tenant` on the process's shared `bridge` over `pool`.
    ///
    /// `pool` is the pool the bridge was built on; the bridge configuration is validated against
    /// its size again here.
    ///
    /// # Errors
    /// `bpm_conversations_pg.bad_setup` when the tenant is empty, longer than 128 characters or
    /// holds a control character, or when the bridge configuration cannot hold against the pool.
    pub fn new(
        bridge: Arc<Bridge>,
        pool: &Pool,
        tenant: &str,
    ) -> std::result::Result<Self, SetupError> {
        if tenant.is_empty()
            || tenant.len() > MAX_TENANT_LEN
            || tenant.chars().any(char::is_control)
        {
            return Err(SetupError::bad_setup("the tenant name is not usable"));
        }
        bridge
            .config()
            .validate(pool.status().max_size)
            .map_err(|_| {
                SetupError::bad_setup("the bridge configuration does not hold against the pool")
            })?;
        Ok(Self {
            bridge,
            tenant: Arc::from(tenant),
        })
    }

    /// The tenant every row is stamped with and every statement filters by.
    #[must_use]
    pub fn tenant(&self) -> &str {
        &self.tenant
    }

    /// Creates the tables and indexes if missing, in one transaction under an advisory lock.
    /// Idempotent; safe from several nodes at once.
    ///
    /// # Errors
    /// `bpm_conversations_pg.migration_failed` with a fixed sentence.
    pub fn prepare(&self) -> std::result::Result<(), SetupError> {
        self.bridge
            .run(PREPARE_DEADLINE, |mut conn: PooledConn| {
                Box::pin(async move { schema::prepare(&mut conn).await.map_err(|_| ()) })
            })
            .map_err(|_| SetupError::migration("the conversation tables could not be created"))
    }

    fn read<T, F>(&self, op: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(PooledConn) -> BoxFuture<'static, Result<T>> + Send + 'static,
    {
        from_bridge(self.bridge.run(READ_DEADLINE, op))
    }

    fn write<T, F>(&self, op: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(PooledConn) -> BoxFuture<'static, Result<T>> + Send + 'static,
    {
        from_bridge(self.bridge.run(WRITE_DEADLINE, op))
    }

    fn tenant_string(&self) -> String {
        self.tenant.to_string()
    }
}

impl ConversationStore for PgConversationStore {
    fn load(
        &self,
        link: &LinkId,
        conversation: &Conversation,
    ) -> Result<Option<ConversationRecord>> {
        let tenant = self.tenant_string();
        let link = link.to_string();
        let conversation = conversation.as_str().to_owned();
        self.read(move |conn| {
            Box::pin(async move {
                conn.query_opt(sql::LOAD, &[&tenant, &link, &conversation])
                    .await
                    .map_err(|e| pg(&e))?
                    .as_ref()
                    .map(record_from_row)
                    .transpose()
            })
        })
    }

    fn create(&self, mut record: ConversationRecord) -> Result<ConversationRecord> {
        record.version = 1;
        let encoded = encode(&record, 1)?;
        let tenant = self.tenant_string();
        let link = record.link.to_string();
        let conversation = record.conversation.as_str().to_owned();
        self.write(move |conn| {
            Box::pin(async move {
                let inserted = conn
                    .query_opt(
                        sql::INSERT,
                        &[
                            &tenant,
                            &link,
                            &conversation,
                            &encoded.case,
                            &encoded.open,
                            &encoded.waiting,
                            &encoded.body,
                        ],
                    )
                    .await
                    .map_err(|e| pg(&e))?;
                if inserted.is_some() {
                    Ok(record)
                } else {
                    Err(ConversationError::Conflict)
                }
            })
        })
    }

    fn save(&self, mut record: ConversationRecord) -> Result<ConversationRecord> {
        let expected = i64::try_from(record.version).map_err(|_| ConversationError::Store)?;
        let next = record.version.saturating_add(1);
        if next > i64::MAX as u64 {
            return Err(ConversationError::Store);
        }
        let encoded = encode(&record, next)?;
        record.version = next;
        let tenant = self.tenant_string();
        let link = record.link.to_string();
        let conversation = record.conversation.as_str().to_owned();
        self.write(move |conn| {
            Box::pin(async move {
                let updated = conn
                    .query_opt(
                        sql::SAVE,
                        &[
                            &tenant,
                            &link,
                            &conversation,
                            &expected,
                            &encoded.case,
                            &encoded.open,
                            &encoded.waiting,
                            &encoded.body,
                        ],
                    )
                    .await
                    .map_err(|e| pg(&e))?;
                if updated.is_some() {
                    Ok(record)
                } else {
                    Err(ConversationError::Conflict)
                }
            })
        })
    }

    fn waiting(&self) -> Result<Vec<ConversationRecord>> {
        let tenant = self.tenant_string();
        self.read(move |conn| {
            Box::pin(async move {
                let rows = conn
                    .query(sql::WAITING, &[&tenant])
                    .await
                    .map_err(|e| pg(&e))?;
                rows.iter().map(record_from_row).collect()
            })
        })
    }

    fn of_case(&self, case: &str) -> Result<Vec<ConversationRecord>> {
        let tenant = self.tenant_string();
        let case = case.to_owned();
        self.read(move |conn| {
            Box::pin(async move {
                let rows = conn
                    .query(sql::OF_CASE, &[&tenant, &case])
                    .await
                    .map_err(|e| pg(&e))?;
                rows.iter().map(record_from_row).collect()
            })
        })
    }

    fn open_on(&self, link: &LinkId) -> Result<usize> {
        let tenant = self.tenant_string();
        let link = link.to_string();
        self.read(move |conn| {
            Box::pin(async move {
                let row = conn
                    .query_one(sql::OPEN_ON, &[&tenant, &link])
                    .await
                    .map_err(|e| pg(&e))?;
                let count: i64 = row.try_get(0).map_err(|_| corrupt())?;
                usize::try_from(count).map_err(|_| corrupt())
            })
        })
    }

    fn rejection(&self, link: &LinkId, id: &MessageId) -> Result<Option<RejectCode>> {
        let tenant = self.tenant_string();
        let link = link.to_string();
        let id = id.to_string();
        self.read(move |conn| {
            Box::pin(async move {
                let row = conn
                    .query_opt(sql::REJECTION, &[&tenant, &link, &id])
                    .await
                    .map_err(|e| pg(&e))?;
                match row {
                    None => Ok(None),
                    Some(row) => {
                        let code: String = row.try_get(0).map_err(|_| corrupt())?;
                        code_from(&code).map(Some)
                    }
                }
            })
        })
    }

    fn note_rejection(&self, link: LinkId, id: MessageId, code: RejectCode) -> Result<()> {
        let tenant = self.tenant_string();
        let link = link.to_string();
        let id = id.to_string();
        let code = code_name(code)?;
        let keep = i64::try_from(MAX_REMEMBERED_REJECTIONS).map_err(|_| corrupt())?;
        self.write(move |mut conn| {
            Box::pin(async move {
                let tx = conn.transaction().await.map_err(|e| pg(&e))?;
                tx.execute(sql::NOTE_REJECTION, &[&tenant, &link, &id, &code])
                    .await
                    .map_err(|e| pg(&e))?;
                tx.execute(sql::TRIM_REJECTIONS, &[&tenant, &keep])
                    .await
                    .map_err(|e| pg(&e))?;
                tx.commit().await.map_err(|e| pg(&e))
            })
        })
    }
}
