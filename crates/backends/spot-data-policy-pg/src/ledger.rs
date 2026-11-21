//! The PostgreSQL ledger: outcomes, checkpoints and proofs for one tenant, over the shared bridge.
//!
//! Every method here is fallible (`Result`), including the reads, and so are the
//! [`RetentionStore`] trait's: a failed read is `data_policy.store_failed` naming the operation
//! (`outcome`, `checkpoint`, ...), never "no row".
//!
//! [`RetentionStore`]: spot_data_policy::store::RetentionStore

use std::sync::{Arc, Mutex};
use std::time::Duration;

use spot_data_policy::DataPolicyError;
use spot_data_policy::DeletionProof;
use spot_data_policy::store::{ApplyItem, Checkpoint, ItemOutcome};
use spot_pg_bridge::{Bridge, BridgeConfig};
use spot_tenancy::TenantId;

use crate::codec::{
    decode_checkpoint, decode_outcome, decode_proof, encode_checkpoint, encode_outcome,
    encode_proof,
};
use crate::error::{OpError, PgPolicyError, map, read_error, store_failed};
use crate::schema::{prepare, scoped};
use crate::source::RecordSource;

/// Deadline of a single-row read or a checkpoint write.
pub const POINT_DEADLINE: Duration = Duration::from_secs(5);
/// Deadline of a read that returns every row of a run or every proof.
pub const SCAN_DEADLINE: Duration = Duration::from_secs(30);
/// Deadline of one item's transaction. It includes the record source's effect, which runs while
/// the transaction is open, so it is the longest.
pub const APPLY_DEADLINE: Duration = Duration::from_secs(60);
/// Deadline of the table creation.
pub const PREPARE_DEADLINE: Duration = Duration::from_secs(30);

/// The ledger of one tenant.
pub struct PgLedger {
    bridge: Arc<Bridge>,
    schema: String,
    tenant: TenantId,
}

impl PgLedger {
    /// Builds the ledger of `tenant` over `bridge`. `pool_size` is the size of the pool the bridge
    /// was built on; no I/O happens here (call [`PgLedger::prepare`] once at start-up).
    ///
    /// # Errors
    /// `data_policy_pg.unsupported_context` when the calling thread is inside a tokio runtime
    /// (the bridge would refuse every call), or when the bridge allows more calls in flight than
    /// the pool can serve.
    pub fn new(
        bridge: Arc<Bridge>,
        pool_size: usize,
        tenant: TenantId,
    ) -> Result<Self, PgPolicyError> {
        if tokio::runtime::Handle::try_current().is_ok() {
            return Err(PgPolicyError::UnsupportedContext {
                reason: "the store is built inside an async runtime; build it on a plain thread",
            });
        }
        if bridge.config().max_in_flight > BridgeConfig::largest_in_flight(pool_size) {
            return Err(PgPolicyError::UnsupportedContext {
                reason: "the bridge allows more calls in flight than the pool can serve",
            });
        }
        Ok(Self {
            schema: tenant.schema_name(),
            bridge,
            tenant,
        })
    }

    /// The tenant this ledger belongs to.
    #[must_use]
    pub fn tenant(&self) -> &TenantId {
        &self.tenant
    }

    /// Creates the tables in the tenant's schema if they are missing. Idempotent.
    ///
    /// # Errors
    /// `data_policy_pg.prepare_failed` when the DDL cannot be applied (for example the schema is
    /// absent) or the bridge refuses the call.
    pub fn prepare(&self) -> Result<(), PgPolicyError> {
        let schema = self.schema.clone();
        self.bridge
            .run(PREPARE_DEADLINE, move |mut conn| {
                Box::pin(async move { prepare(&mut conn, &schema).await })
            })
            .map_err(|_| PgPolicyError::PrepareFailed)
    }

    /// The outcome of `record_id` in `run_id`.
    ///
    /// # Errors
    /// [`DataPolicyError::StoreFailed`] when the database or the bridge fails.
    pub fn outcome(
        &self,
        run_id: &str,
        record_id: &str,
    ) -> Result<Option<ItemOutcome>, DataPolicyError> {
        let (schema, run, record) = (self.schema.clone(), run_id.to_owned(), record_id.to_owned());
        self.bridge
            .run(POINT_DEADLINE, move |mut conn| {
                Box::pin(async move {
                    let tx = scoped(&mut conn, &schema).await?;
                    read_outcome(&tx, &run, &record).await
                })
            })
            .map_err(|e| read_error!("outcome", e))
    }

    /// Every outcome of a run, ordered by record id (bytewise, as the in-memory store orders).
    ///
    /// # Errors
    /// [`DataPolicyError::StoreFailed`] when the database or the bridge fails.
    pub fn run_outcomes(&self, run_id: &str) -> Result<Vec<ItemOutcome>, DataPolicyError> {
        let (schema, run) = (self.schema.clone(), run_id.to_owned());
        self.bridge
            .run(SCAN_DEADLINE, move |mut conn| {
                Box::pin(async move {
                    let tx = scoped(&mut conn, &schema).await?;
                    let rows = tx
                        .query(
                            "SELECT record_id, outcome::text FROM spot_retention_outcome \
                             WHERE run_id = $1 ORDER BY record_id COLLATE \"C\"",
                            &[&run],
                        )
                        .await?;
                    let mut out = Vec::with_capacity(rows.len());
                    for row in rows {
                        out.push(decode_outcome(
                            run.clone(),
                            row.get(0),
                            row.get::<_, &str>(1),
                        )?);
                    }
                    Ok::<_, OpError>(out)
                })
            })
            .map_err(|e| read_error!("run_outcomes", e))
    }

    /// Replaces the checkpoint of its run.
    ///
    /// # Errors
    /// [`DataPolicyError::StoreFailed`] when the database or the bridge fails.
    pub fn save_checkpoint(&self, checkpoint: &Checkpoint) -> Result<(), DataPolicyError> {
        let doc = encode_checkpoint(checkpoint)
            .map_err(|e| map(spot_pg_bridge::BridgeError::Store(e)))?;
        let (schema, run) = (self.schema.clone(), checkpoint.run_id.clone());
        self.bridge
            .run(POINT_DEADLINE, move |mut conn| {
                Box::pin(async move {
                    let tx = scoped(&mut conn, &schema).await?;
                    tx.execute(
                        "INSERT INTO spot_retention_checkpoint (run_id, checkpoint) \
                         VALUES ($1, $2::text::jsonb) \
                         ON CONFLICT (run_id) DO UPDATE SET checkpoint = EXCLUDED.checkpoint",
                        &[&run, &doc],
                    )
                    .await?;
                    tx.commit().await?;
                    Ok::<_, OpError>(())
                })
            })
            .map_err(map)
    }

    /// The saved checkpoint of a run.
    ///
    /// # Errors
    /// [`DataPolicyError::StoreFailed`] when the database or the bridge fails.
    pub fn checkpoint(&self, run_id: &str) -> Result<Option<Checkpoint>, DataPolicyError> {
        let (schema, run) = (self.schema.clone(), run_id.to_owned());
        self.bridge
            .run(POINT_DEADLINE, move |mut conn| {
                Box::pin(async move {
                    let tx = scoped(&mut conn, &schema).await?;
                    let row = tx
                        .query_opt(
                            "SELECT checkpoint::text FROM spot_retention_checkpoint \
                             WHERE run_id = $1",
                            &[&run],
                        )
                        .await?;
                    row.map(|row| decode_checkpoint(run.clone(), row.get::<_, &str>(0)))
                        .transpose()
                })
            })
            .map_err(|e| read_error!("checkpoint", e))
    }

    /// The deletion proof of a record.
    ///
    /// # Errors
    /// [`DataPolicyError::StoreFailed`] when the database or the bridge fails.
    pub fn proof(&self, record_id: &str) -> Result<Option<DeletionProof>, DataPolicyError> {
        let (schema, record) = (self.schema.clone(), record_id.to_owned());
        self.bridge
            .run(POINT_DEADLINE, move |mut conn| {
                Box::pin(async move {
                    let tx = scoped(&mut conn, &schema).await?;
                    read_proof(&tx, &record).await
                })
            })
            .map_err(|e| read_error!("proof", e))
    }

    /// Every deletion proof, ordered by record id (bytewise).
    ///
    /// # Errors
    /// [`DataPolicyError::StoreFailed`] when the database or the bridge fails.
    pub fn proofs(&self) -> Result<Vec<DeletionProof>, DataPolicyError> {
        let schema = self.schema.clone();
        self.bridge
            .run(SCAN_DEADLINE, move |mut conn| {
                Box::pin(async move {
                    let tx = scoped(&mut conn, &schema).await?;
                    let rows = tx
                        .query(
                            "SELECT record_id, proof::text FROM spot_deletion_proof \
                             ORDER BY record_id COLLATE \"C\"",
                            &[],
                        )
                        .await?;
                    let mut out = Vec::with_capacity(rows.len());
                    for row in rows {
                        out.push(decode_proof(row.get(0), row.get::<_, &str>(1))?);
                    }
                    Ok::<_, OpError>(out)
                })
            })
            .map_err(|e| read_error!("proofs", e))
    }

    /// Commits one item: its outcome row, the effect through `source`, and the deletion proof, in
    /// one transaction.
    ///
    /// - If the item already has an outcome equal to the new one, nothing is applied and the call
    ///   succeeds (idempotent). A different existing outcome is a store failure.
    /// - The outcome row is inserted first, so a concurrent call for the same item waits on the
    ///   row, then finds it committed (and applies nothing) or rolled back (and applies the
    ///   effect itself).
    /// - A failing effect rolls the ledger rows back.
    ///
    /// # Errors
    /// [`DataPolicyError::StoreFailed`].
    pub fn apply_item<S: RecordSource>(
        &self,
        source: &Arc<Mutex<S>>,
        item: ApplyItem,
    ) -> Result<(), DataPolicyError> {
        let outcome_doc = encode_outcome(&item.outcome)
            .map_err(|e| map(spot_pg_bridge::BridgeError::Store(e)))?;
        let proof_doc = item
            .proof
            .as_ref()
            .map(encode_proof)
            .transpose()
            .map_err(|e| map(spot_pg_bridge::BridgeError::Store(e)))?;
        if let Some(proof) = &item.proof
            && proof.record_id != item.effect.record_id().unwrap_or(&proof.record_id)
        {
            return Err(store_failed("the deletion proof names a different record"));
        }
        let attempt = i64::from(item.outcome.attempt);
        let (schema, source) = (self.schema.clone(), Arc::clone(source));
        self.bridge
            .run(APPLY_DEADLINE, move |mut conn| {
                Box::pin(async move {
                    let tx = scoped(&mut conn, &schema).await?;
                    let run = item.outcome.run_id.clone();
                    let record = item.outcome.record_id.clone();
                    let inserted = tx
                        .execute(
                            "INSERT INTO spot_retention_outcome \
                             (run_id, record_id, status, attempt, outcome) \
                             VALUES ($1, $2, $3, $4, $5::text::jsonb) \
                             ON CONFLICT (run_id, record_id) DO NOTHING",
                            &[
                                &run,
                                &record,
                                &item.outcome.status.as_str(),
                                &attempt,
                                &outcome_doc,
                            ],
                        )
                        .await?;
                    if inserted == 0 {
                        // The first outcome is the truth: the same one is a no-op.
                        return match read_outcome(&tx, &run, &record).await? {
                            Some(existing) if existing == item.outcome => Ok(()),
                            _ => Err(OpError::Conflict(
                                "the item already has a different outcome",
                            )),
                        };
                    }
                    let item = run_effect(source, item).await?;
                    if let (Some(proof), Some(doc)) = (&item.proof, &proof_doc) {
                        let written = tx
                            .execute(
                                "INSERT INTO spot_deletion_proof (record_id, proof) \
                                 VALUES ($1, $2::text::jsonb) ON CONFLICT (record_id) DO NOTHING",
                                &[&proof.record_id, doc],
                            )
                            .await?;
                        if written == 0
                            && read_proof(&tx, &proof.record_id).await?.as_ref() != Some(proof)
                        {
                            return Err(OpError::Conflict(
                                "a different deletion proof is already on file for the record",
                            ));
                        }
                    }
                    tx.commit().await?;
                    Ok(())
                })
            })
            .map_err(map)
    }
}

/// Runs the source's effect on a blocking thread and hands the item back.
async fn run_effect<S: RecordSource>(
    source: Arc<Mutex<S>>,
    item: ApplyItem,
) -> Result<ApplyItem, OpError> {
    tokio::task::spawn_blocking(move || {
        let mut guard = source
            .lock()
            .map_err(|_| OpError::Effect(store_failed("the record source is no longer usable")))?;
        guard.apply_effect(&item).map_err(OpError::Effect)?;
        Ok(item)
    })
    .await
    .map_err(|_| OpError::Effect(store_failed("the record source stopped during the effect")))?
}

async fn read_outcome(
    tx: &deadpool_postgres::Transaction<'_>,
    run: &str,
    record: &str,
) -> Result<Option<ItemOutcome>, OpError> {
    let row = tx
        .query_opt(
            "SELECT outcome::text FROM spot_retention_outcome WHERE run_id = $1 AND record_id = $2",
            &[&run, &record],
        )
        .await?;
    row.map(|row| decode_outcome(run.to_owned(), record.to_owned(), row.get::<_, &str>(0)))
        .transpose()
}

async fn read_proof(
    tx: &deadpool_postgres::Transaction<'_>,
    record: &str,
) -> Result<Option<DeletionProof>, OpError> {
    let row = tx
        .query_opt(
            "SELECT proof::text FROM spot_deletion_proof WHERE record_id = $1",
            &[&record],
        )
        .await?;
    row.map(|row| decode_proof(record.to_owned(), row.get::<_, &str>(0)))
        .transpose()
}
