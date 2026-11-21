//! `PgConditionStore` (WP-RTB-01.T2): the durable side of `spot-conditions`.
//!
//! [`PgConditionStore::load_open`] is what [`ConditionEngine::restore`](spot_conditions::ConditionEngine::restore)
//! reloads a site's engine from after a restart. [`PgConditionStore::apply`] is the write path:
//! given the audit events one [`ConditionEngine`](spot_conditions::ConditionEngine) call
//! produced, it updates `spot_condition_open`, appends to `spot_condition_history` (bounded to
//! [`crate::schema::HISTORY_MAX_ROWS_PER_SUBJECT`] rows and
//! [`crate::schema::HISTORY_RETENTION_DAYS`] days per subject) and writes the matching
//! `sys_condition_raised` / `sys_condition_cleared` outbox event — all in **one** transaction, so
//! a reader never observes the open table, the history or the outbox disagreeing about a
//! transition that is still in flight, and a crash mid-write leaves none of the three changed.
//!
//! # No document, no catalog entity
//!
//! Conditions are not documents: `spot_condition_open` and `spot_condition_history` carry no
//! entity of their own in the site's [`Catalog`]. This store only uses the raw-SQL seam
//! (`PgTransaction::raw_query`/`raw_execute_with`) for its own two tables and the transaction's
//! [`Outbox::enqueue`] for the event, exactly as a sibling `*-pg` crate that owns tables beside
//! the document ones does (see `spot-pg-search`). The `Arc<Catalog>` [`PgConditionStore::new`]
//! takes is only what [`PgTransaction::begin`](spot_postgres::PgBackend::begin) requires to open
//! a transaction on the site's pool; this crate never reads an entity from it.
//!
//! # The outbox `NodeRef`
//!
//! [`spot_storage_contract::OutboxEvent::subject`] is a `NodeRef` of a real document in every
//! other producer. A condition's subject is opaque text (`node:branch7`, a tenant id), not a
//! document id, so this store gives the event a fresh [`RecordId::new_v7`] under the fixed
//! entity name `condition` — it only has to be *some* valid v7 id, because nothing here relies
//! on it for ordering (`subject_version` is always `None`) or looks it up as a real record. See
//! the task's open questions.

use std::sync::Arc;

use serde_json::{Map, Value};
use spot_conditions::{AuditEvent, ConditionId, ConditionParams, Millis, Open, Severity, Subject};
use spot_postgres::{PgBackend, PgTransaction};
use spot_storage_contract::{
    Catalog, Isolation, Name, NodeRef, Outbox, OutboxEvent, RecordId, StorageBackend, Transaction,
    TxOptions,
};
use tokio_postgres::Row;

use crate::error::ConditionsPgError;
use crate::schema::{DDL, HISTORY_MAX_ROWS_PER_SUBJECT, HISTORY_RETENTION_DAYS};

/// The entity name the outbox `NodeRef` of every condition event carries. Not a real entity in
/// any site's catalog: the store never looks one up under it.
const OUTBOX_ENTITY: &str = "condition";

/// The default cap on records one `apply` transaction may write, high enough that an ordinary
/// batch of transitions never trips it (see [`PgConditionStore::with_max_writes`] for a store
/// built with a different one, which only a test needs).
const DEFAULT_MAX_WRITES: u32 = 10_000;

fn unavailable<E>(_: E) -> ConditionsPgError {
    ConditionsPgError::Unavailable
}

fn label(name: &str) -> Result<Name, ConditionsPgError> {
    // `conditions_*` is lower snake case and well under the 63-byte limit, so this does not fail
    // for a name this crate writes; a name that is not valid can never be written, hence Corrupt.
    Name::new(name).map_err(|_| ConditionsPgError::Corrupt)
}

/// The PostgreSQL store behind `spot-conditions`, one per site (ADR-34: one tenant, one
/// database, so no row here carries a tenant column).
pub struct PgConditionStore {
    backend: PgBackend,
    catalog: Arc<Catalog>,
    max_writes: u32,
}

impl PgConditionStore {
    /// A store over `backend`'s pool, using `catalog` only to open a transaction (see the
    /// module documentation: no entity of it is ever read).
    #[must_use]
    pub fn new(backend: PgBackend, catalog: Arc<Catalog>) -> Self {
        Self {
            backend,
            catalog,
            max_writes: DEFAULT_MAX_WRITES,
        }
    }

    /// Like [`PgConditionStore::new`], with the `apply` transaction's write budget set to
    /// `max_writes` instead of the default.
    ///
    /// Only a test needs this: it is how the atomicity test forces `apply` to fail partway
    /// through a transaction (the write budget is exceeded before the transaction can commit)
    /// without any debug-only hook in the production path.
    #[must_use]
    pub fn with_max_writes(backend: PgBackend, catalog: Arc<Catalog>, max_writes: u32) -> Self {
        Self {
            backend,
            catalog,
            max_writes,
        }
    }

    fn write_options(&self) -> Result<TxOptions, ConditionsPgError> {
        let mut options = TxOptions::new(label("conditions_apply")?, Isolation::ReadCommitted);
        options.max_writes = self.max_writes;
        Ok(options)
    }

    async fn begin_write(&self) -> Result<PgTransaction, ConditionsPgError> {
        self.backend
            .begin(Arc::clone(&self.catalog), self.write_options()?)
            .await
            .map_err(unavailable)
    }

    /// Creates `spot_condition_open` and `spot_condition_history` if they do not exist.
    /// Idempotent and safe to call from every node at start-up.
    ///
    /// # Errors
    ///
    /// [`ConditionsPgError::MigrationFailed`] when the database refuses the DDL.
    pub async fn prepare(&self) -> Result<(), ConditionsPgError> {
        let options = TxOptions::new(label("conditions_prepare")?, Isolation::ReadCommitted);
        let mut tx = self
            .backend
            .begin(Arc::clone(&self.catalog), options)
            .await
            .map_err(|_| ConditionsPgError::MigrationFailed)?;
        for statement in DDL.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            tx.raw_execute_with(PgTransaction::READ, statement, &[])
                .await
                .map_err(|_| ConditionsPgError::MigrationFailed)?;
        }
        tx.commit()
            .await
            .map_err(|_| ConditionsPgError::MigrationFailed)
    }

    /// Every condition `spot_condition_open` has a row for: what
    /// [`ConditionEngine::restore`](spot_conditions::ConditionEngine::restore) rebuilds the
    /// in-memory engine from after a restart.
    ///
    /// # Errors
    ///
    /// [`ConditionsPgError::Unavailable`] when the store cannot be read;
    /// [`ConditionsPgError::Corrupt`] when a row does not decode (an id, a severity or a
    /// parameter value this build does not know).
    pub async fn load_open(&self) -> Result<Vec<Open>, ConditionsPgError> {
        let options =
            TxOptions::new(label("conditions_load")?, Isolation::ReadCommitted).read_only();
        let mut tx = self
            .backend
            .begin(Arc::clone(&self.catalog), options)
            .await
            .map_err(unavailable)?;
        let rows = tx
            .raw_query(
                "SELECT id, subject, severity, params, raised_at_millis, last_seen_millis \
                 FROM spot_condition_open ORDER BY id, subject",
                &[],
                None,
            )
            .await
            .map_err(unavailable)?;
        let _ = tx.rollback().await;
        rows.iter().map(decode_open).collect()
    }

    /// Writes `events` (the audit events one engine call produced) to
    /// `spot_condition_open`, `spot_condition_history` and the outbox, all in one transaction.
    ///
    /// Processed in order: a raise or an escalation upserts the open row, a clear deletes it;
    /// every transition appends one history row (pruned to
    /// [`crate::schema::HISTORY_MAX_ROWS_PER_SUBJECT`] rows and
    /// [`crate::schema::HISTORY_RETENTION_DAYS`] days for that subject) and enqueues the
    /// matching outbox event. Nothing is committed until every event of the batch wrote
    /// cleanly: a failure partway rolls the whole call back, so a reader never sees the open
    /// table, the history or the outbox reflect only some of the events.
    ///
    /// # Errors
    ///
    /// [`ConditionsPgError::Unavailable`] for a connection, statement or write-budget failure
    /// (the whole batch is then not written, not partially written).
    pub async fn apply(&self, events: &[AuditEvent]) -> Result<(), ConditionsPgError> {
        if events.is_empty() {
            return Ok(());
        }
        let mut tx = self.begin_write().await?;
        for event in events {
            write_open(&mut tx, event).await?;
            insert_history(&mut tx, event).await?;
            prune_history(&mut tx, event.subject().as_str()).await?;
            tx.enqueue(&outbox_event(event)?)
                .await
                .map_err(unavailable)?;
        }
        tx.commit().await.map_err(unavailable)
    }
}

async fn write_open(tx: &mut PgTransaction, event: &AuditEvent) -> Result<(), ConditionsPgError> {
    match event {
        AuditEvent::Raised {
            id,
            subject,
            severity,
            at_millis,
            params,
        } => {
            tx.raw_execute_with(
                PgTransaction::write(1),
                "INSERT INTO spot_condition_open \
                   (id, subject, severity, params, raised_at_millis, last_seen_millis) \
                 VALUES ($1, $2, $3, $4, $5, $5) \
                 ON CONFLICT (id, subject) DO UPDATE SET \
                   severity = EXCLUDED.severity, \
                   params = EXCLUDED.params, \
                   raised_at_millis = EXCLUDED.raised_at_millis, \
                   last_seen_millis = EXCLUDED.last_seen_millis",
                &[
                    &id.as_str(),
                    &subject.as_str(),
                    &severity.as_str(),
                    &params_json(params),
                    &millis_i64(*at_millis)?,
                ],
            )
            .await
            .map_err(unavailable)?;
        }
        AuditEvent::Escalated {
            id,
            subject,
            to,
            at_millis,
            params,
            ..
        } => {
            tx.raw_execute_with(
                PgTransaction::write(1),
                "UPDATE spot_condition_open \
                 SET severity = $3, params = $4, last_seen_millis = $5 \
                 WHERE id = $1 AND subject = $2",
                &[
                    &id.as_str(),
                    &subject.as_str(),
                    &to.as_str(),
                    &params_json(params),
                    &millis_i64(*at_millis)?,
                ],
            )
            .await
            .map_err(unavailable)?;
        }
        AuditEvent::Cleared { id, subject, .. } => {
            tx.raw_execute_with(
                PgTransaction::write(1),
                "DELETE FROM spot_condition_open WHERE id = $1 AND subject = $2",
                &[&id.as_str(), &subject.as_str()],
            )
            .await
            .map_err(unavailable)?;
        }
    }
    Ok(())
}

async fn insert_history(
    tx: &mut PgTransaction,
    event: &AuditEvent,
) -> Result<(), ConditionsPgError> {
    tx.raw_execute_with(
        PgTransaction::write(1),
        "INSERT INTO spot_condition_history \
           (id, subject, transition, severity, params, at_millis) \
         VALUES ($1, $2, $3, $4, $5, $6)",
        &[
            &event.id().as_str(),
            &event.subject().as_str(),
            &event.transition().as_str(),
            &event.severity().as_str(),
            &params_json(event.params()),
            &millis_i64(event.at_millis())?,
        ],
    )
    .await
    .map_err(unavailable)?;
    Ok(())
}

/// Keeps at most [`HISTORY_MAX_ROWS_PER_SUBJECT`] rows younger than
/// [`HISTORY_RETENTION_DAYS`] for `subject`. The limit and the retention are this crate's own
/// constants, never request data, so embedding them in the statement text carries nothing a
/// caller supplied (ADR-45's raw-SQL rule is about caller values, which still travel as bound
/// parameters here).
async fn prune_history(tx: &mut PgTransaction, subject: &str) -> Result<(), ConditionsPgError> {
    let sql = format!(
        "DELETE FROM spot_condition_history \
         WHERE subject = $1 \
           AND (recorded_at < now() - interval '{HISTORY_RETENTION_DAYS} days' \
                OR seq NOT IN ( \
                  SELECT seq FROM spot_condition_history \
                  WHERE subject = $1 ORDER BY seq DESC LIMIT {HISTORY_MAX_ROWS_PER_SUBJECT}))"
    );
    tx.raw_execute_with(PgTransaction::write(1), &sql, &[&subject])
        .await
        .map_err(unavailable)?;
    Ok(())
}

fn millis_i64(value: Millis) -> Result<i64, ConditionsPgError> {
    i64::try_from(value.as_u64()).map_err(|_| ConditionsPgError::Corrupt)
}

fn params_json(params: &ConditionParams) -> Value {
    let mut map = Map::new();
    for (name, value) in params.iter() {
        map.insert(name.to_owned(), Value::String(value.to_owned()));
    }
    Value::Object(map)
}

fn severity_from_str(text: &str) -> Result<Severity, ConditionsPgError> {
    Severity::ALL
        .into_iter()
        .find(|s| s.as_str() == text)
        .ok_or(ConditionsPgError::Corrupt)
}

fn params_from_json(value: &Value) -> Result<ConditionParams, ConditionsPgError> {
    let object = value.as_object().ok_or(ConditionsPgError::Corrupt)?;
    let mut params = ConditionParams::new();
    for (name, value) in object {
        let text = value.as_str().ok_or(ConditionsPgError::Corrupt)?;
        params.set(name, text);
    }
    Ok(params)
}

fn decode_open(row: &Row) -> Result<Open, ConditionsPgError> {
    let id: String = row.try_get(0).map_err(unavailable)?;
    let subject: String = row.try_get(1).map_err(unavailable)?;
    let severity: String = row.try_get(2).map_err(unavailable)?;
    let params: Value = row.try_get(3).map_err(unavailable)?;
    let raised_at: i64 = row.try_get(4).map_err(unavailable)?;
    let last_seen: i64 = row.try_get(5).map_err(unavailable)?;
    Ok(Open {
        id: ConditionId::parse(&id).map_err(|_| ConditionsPgError::Corrupt)?,
        subject: Subject::new(subject),
        severity: severity_from_str(&severity)?,
        params: params_from_json(&params)?,
        raised_at_millis: Millis::new(
            u64::try_from(raised_at).map_err(|_| ConditionsPgError::Corrupt)?,
        ),
        last_seen_millis: Millis::new(
            u64::try_from(last_seen).map_err(|_| ConditionsPgError::Corrupt)?,
        ),
    })
}

/// The outbox event one audit event becomes (the events contract's `sys_condition_raised` and
/// `sys_condition_cleared` v1 payload: `transition`, `id`, `subject`, `at_millis`, `params`, and
/// `severity` for a raise or a clear, `from`/`to` for an escalation).
fn outbox_event(event: &AuditEvent) -> Result<OutboxEvent, ConditionsPgError> {
    let mut payload = Map::new();
    payload.insert(
        "transition".to_owned(),
        Value::String(event.transition().as_str().to_owned()),
    );
    payload.insert(
        "id".to_owned(),
        Value::String(event.id().as_str().to_owned()),
    );
    payload.insert(
        "subject".to_owned(),
        Value::String(event.subject().as_str().to_owned()),
    );
    payload.insert(
        "at_millis".to_owned(),
        Value::Number(millis_i64(event.at_millis())?.into()),
    );
    payload.insert("params".to_owned(), params_json(event.params()));
    match event {
        AuditEvent::Raised { severity, .. } | AuditEvent::Cleared { severity, .. } => {
            payload.insert(
                "severity".to_owned(),
                Value::String(severity.as_str().to_owned()),
            );
        }
        AuditEvent::Escalated { from, to, .. } => {
            payload.insert("from".to_owned(), Value::String(from.as_str().to_owned()));
            payload.insert("to".to_owned(), Value::String(to.as_str().to_owned()));
        }
    }
    Ok(OutboxEvent {
        id: RecordId::new_v7(),
        topic: label(event.topic().as_str())?,
        subject: NodeRef {
            entity: label(OUTBOX_ENTITY)?,
            id: RecordId::new_v7(),
        },
        subject_version: None,
        schema_version: 1,
        payload: Value::Object(payload).to_string(),
    })
}
