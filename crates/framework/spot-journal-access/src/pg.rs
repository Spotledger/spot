//! The PostgreSQL grant store and access audit.
//!
//! * [`PgGrantStore`]: table `spot_journal_grant`, written in the **same transaction** as its
//!   `sys_journal_grant` outbox event (so no grant exists without its record, nor the reverse).
//! * [`PgAccessAudit`]: one `sys_journal_access` outbox event per access, in its own
//!   transaction; a failure refuses the call.
//!
//! Both work on the site's own pool (ADR-34: one tenant, one database), through the raw-SQL seam
//! and the transaction's outbox, as `spot-conditions-pg` does. The DDL is published as [`DDL`]
//! and created by `spot migrate` through the extra-table hook; the host refuses to start with
//! the table missing ([`PgGrantStore::check`]) and never creates it. Every value is a bind
//! parameter. An error carries a fixed sentence, never the database's message.

use std::sync::Arc;

use serde_json::Value as Json;
use spot_postgres::{PgBackend, PgTransaction};
use spot_storage_contract::{
    Catalog, Isolation, Name, NodeRef, Outbox, OutboxEvent, RecordId, StorageBackend, Transaction,
    TxOptions,
};
use tokio_postgres::Row;

use crate::error::AccessError;
use crate::model::{ACCESS_TOPIC, AccessRecord, Fut, GRANT_TOPIC, Grant, GrantEvent};
use crate::reader::AccessAudit;
use crate::store::GrantStore;

/// The idempotent DDL: one statement per object, each an `IF NOT EXISTS` creation.
pub const DDL: &str = r#"
CREATE TABLE IF NOT EXISTS spot_journal_grant (
    id          text PRIMARY KEY,
    grantee     text NOT NULL,
    grantor     text NOT NULL,
    expires_at  bigint NOT NULL,
    external    boolean NOT NULL,
    created_at  bigint NOT NULL,
    revoked_at  bigint,
    revoked_by  text,
    CONSTRAINT spot_journal_grant_ids CHECK (length(id) BETWEEN 1 AND 128 AND length(grantee) BETWEEN 1 AND 128 AND length(grantor) BETWEEN 1 AND 128),
    CONSTRAINT spot_journal_grant_lifetime CHECK (expires_at > created_at),
    CONSTRAINT spot_journal_grant_revocation CHECK ((revoked_at IS NULL) = (revoked_by IS NULL))
);
CREATE INDEX IF NOT EXISTS spot_journal_grant_grantee_idx ON spot_journal_grant (grantee, created_at);
"#;

const COLUMNS: &str =
    "id, grantee, grantor, expires_at, external, created_at, revoked_at, revoked_by";

fn store_failed<E>(_: E) -> AccessError {
    AccessError::Store
}

fn name(text: &str) -> Result<Name, AccessError> {
    Name::new(text).map_err(store_failed)
}

fn outbox_event(
    topic: &'static str,
    entity: &str,
    payload: &Json,
) -> Result<OutboxEvent, AccessError> {
    Ok(OutboxEvent {
        id: RecordId::new_v7(),
        topic: name(topic)?,
        subject: NodeRef {
            entity: name(entity)?,
            id: RecordId::new_v7(),
        },
        subject_version: None,
        schema_version: 1,
        payload: payload.to_string(),
    })
}

async fn begin(
    backend: &PgBackend,
    catalog: &Arc<Catalog>,
    label: &str,
    read_only: bool,
) -> Result<PgTransaction, AccessError> {
    let mut options = TxOptions::new(name(label)?, Isolation::ReadCommitted);
    if read_only {
        options = options.read_only();
    }
    backend
        .begin(Arc::clone(catalog), options)
        .await
        .map_err(store_failed)
}

fn grant_of(row: &Row) -> Result<Grant, AccessError> {
    Ok(Grant {
        id: row.try_get(0).map_err(store_failed)?,
        grantee: row.try_get(1).map_err(store_failed)?,
        grantor: row.try_get(2).map_err(store_failed)?,
        expires_at_millis: row.try_get(3).map_err(store_failed)?,
        external: row.try_get(4).map_err(store_failed)?,
        created_at_millis: row.try_get(5).map_err(store_failed)?,
        revoked_at_millis: row.try_get(6).map_err(store_failed)?,
        revoked_by: row.try_get(7).map_err(store_failed)?,
    })
}

/// The grant store over a tenant's PostgreSQL pool.
pub struct PgGrantStore {
    backend: PgBackend,
    catalog: Arc<Catalog>,
}

impl PgGrantStore {
    /// A store over `backend`'s pool. `catalog` is only what a transaction needs to open (no
    /// entity of it is read), as for the other `*-pg` stores.
    #[must_use]
    pub fn new(backend: PgBackend, catalog: Arc<Catalog>) -> Self {
        Self { backend, catalog }
    }

    /// Whether the table exists (the host's start-up check; it never creates it).
    ///
    /// # Errors
    /// [`AccessError::Store`] when the database cannot be reached or the table is missing.
    pub async fn check(&self) -> Result<(), AccessError> {
        let mut tx = begin(&self.backend, &self.catalog, "journal_grant_check", true).await?;
        let rows = tx
            .raw_query(
                "SELECT to_regclass('spot_journal_grant') IS NOT NULL",
                &[],
                None,
            )
            .await
            .map_err(store_failed)?;
        let _ = tx.rollback().await;
        let present: bool = rows
            .first()
            .ok_or(AccessError::Store)?
            .try_get(0)
            .map_err(store_failed)?;
        if present {
            Ok(())
        } else {
            Err(AccessError::Store)
        }
    }

    async fn fetch(tx: &mut PgTransaction, id: &str) -> Result<Option<Grant>, AccessError> {
        let sql = format!("SELECT {COLUMNS} FROM spot_journal_grant WHERE id = $1");
        let rows = tx
            .raw_query(&sql, &[&id], None)
            .await
            .map_err(store_failed)?;
        rows.first().map(grant_of).transpose()
    }
}

impl GrantStore for PgGrantStore {
    fn insert<'a>(&'a self, grant: &'a Grant, event: &'a GrantEvent) -> Fut<'a, ()> {
        Box::pin(async move {
            let mut tx = begin(&self.backend, &self.catalog, "journal_grant_insert", false).await?;
            tx.raw_execute_with(
                PgTransaction::write(1),
                "INSERT INTO spot_journal_grant \
                   (id, grantee, grantor, expires_at, external, created_at) \
                 VALUES ($1, $2, $3, $4, $5, $6)",
                &[
                    &grant.id,
                    &grant.grantee,
                    &grant.grantor,
                    &grant.expires_at_millis,
                    &grant.external,
                    &grant.created_at_millis,
                ],
            )
            .await
            .map_err(store_failed)?;
            tx.enqueue(&outbox_event(
                GRANT_TOPIC,
                "journal_grant",
                &event.to_payload(),
            )?)
            .await
            .map_err(store_failed)?;
            tx.commit().await.map_err(store_failed)
        })
    }

    fn renew<'a>(
        &'a self,
        id: &'a str,
        expires_at_millis: i64,
        event: &'a GrantEvent,
    ) -> Fut<'a, Grant> {
        Box::pin(async move {
            let mut tx = begin(&self.backend, &self.catalog, "journal_grant_renew", false).await?;
            let changed = tx
                .raw_execute_with(
                    PgTransaction::write(1),
                    "UPDATE spot_journal_grant SET expires_at = $2 \
                     WHERE id = $1 AND revoked_at IS NULL",
                    &[&id, &expires_at_millis],
                )
                .await
                .map_err(store_failed)?;
            if changed == 0 {
                let _ = tx.rollback().await;
                return Err(AccessError::GrantUnknown);
            }
            let grant = Self::fetch(&mut tx, id)
                .await?
                .ok_or(AccessError::GrantUnknown)?;
            tx.enqueue(&outbox_event(
                GRANT_TOPIC,
                "journal_grant",
                &event.to_payload(),
            )?)
            .await
            .map_err(store_failed)?;
            tx.commit().await.map_err(store_failed)?;
            Ok(grant)
        })
    }

    fn revoke<'a>(
        &'a self,
        id: &'a str,
        by: &'a str,
        at_millis: i64,
        event: &'a GrantEvent,
    ) -> Fut<'a, Grant> {
        Box::pin(async move {
            let mut tx = begin(&self.backend, &self.catalog, "journal_grant_revoke", false).await?;
            let changed = tx
                .raw_execute_with(
                    PgTransaction::write(1),
                    "UPDATE spot_journal_grant SET revoked_at = $2, revoked_by = $3 \
                     WHERE id = $1 AND revoked_at IS NULL",
                    &[&id, &at_millis, &by],
                )
                .await
                .map_err(store_failed)?;
            if changed == 0 {
                let _ = tx.rollback().await;
                return Err(AccessError::GrantUnknown);
            }
            let grant = Self::fetch(&mut tx, id)
                .await?
                .ok_or(AccessError::GrantUnknown)?;
            tx.enqueue(&outbox_event(
                GRANT_TOPIC,
                "journal_grant",
                &event.to_payload(),
            )?)
            .await
            .map_err(store_failed)?;
            tx.commit().await.map_err(store_failed)?;
            Ok(grant)
        })
    }

    fn get<'a>(&'a self, id: &'a str) -> Fut<'a, Option<Grant>> {
        Box::pin(async move {
            let mut tx = begin(&self.backend, &self.catalog, "journal_grant_get", true).await?;
            let grant = Self::fetch(&mut tx, id).await;
            let _ = tx.rollback().await;
            grant
        })
    }

    fn for_grantee<'a>(&'a self, grantee: &'a str) -> Fut<'a, Vec<Grant>> {
        Box::pin(async move {
            let mut tx = begin(&self.backend, &self.catalog, "journal_grant_for", true).await?;
            let sql = format!(
                "SELECT {COLUMNS} FROM spot_journal_grant \
                 WHERE grantee = $1 ORDER BY created_at DESC"
            );
            let rows = tx
                .raw_query(&sql, &[&grantee], None)
                .await
                .map_err(store_failed);
            let _ = tx.rollback().await;
            rows?.iter().map(grant_of).collect()
        })
    }

    fn list<'a>(&'a self) -> Fut<'a, Vec<Grant>> {
        Box::pin(async move {
            let mut tx = begin(&self.backend, &self.catalog, "journal_grant_list", true).await?;
            let sql = format!(
                "SELECT {COLUMNS} FROM spot_journal_grant ORDER BY created_at DESC LIMIT 1000"
            );
            let rows = tx.raw_query(&sql, &[], None).await.map_err(store_failed);
            let _ = tx.rollback().await;
            rows?.iter().map(grant_of).collect()
        })
    }

    fn record_event<'a>(&'a self, event: &'a GrantEvent) -> Fut<'a, ()> {
        Box::pin(async move {
            let mut tx = begin(&self.backend, &self.catalog, "journal_grant_event", false).await?;
            tx.enqueue(&outbox_event(
                GRANT_TOPIC,
                "journal_grant",
                &event.to_payload(),
            )?)
            .await
            .map_err(store_failed)?;
            tx.commit().await.map_err(store_failed)
        })
    }
}

/// The `sys_journal_access` audit over the site's outbox.
pub struct PgAccessAudit {
    backend: PgBackend,
    catalog: Arc<Catalog>,
}

impl PgAccessAudit {
    /// An audit writer over `backend`'s pool.
    #[must_use]
    pub fn new(backend: PgBackend, catalog: Arc<Catalog>) -> Self {
        Self { backend, catalog }
    }
}

impl AccessAudit for PgAccessAudit {
    fn record<'a>(&'a self, record: &'a AccessRecord) -> Fut<'a, ()> {
        Box::pin(async move {
            let mut tx = begin(&self.backend, &self.catalog, "journal_access_audit", false).await?;
            tx.enqueue(&outbox_event(
                ACCESS_TOPIC,
                "journal_access",
                &record.to_payload(),
            )?)
            .await
            .map_err(store_failed)?;
            tx.commit().await.map_err(store_failed)
        })
    }
}
