//! [`PgRetentionStore`]: the [`RetentionStore`] seam over the PostgreSQL ledger and a
//! [`RecordSource`].
//!
//! # Reads that can fail
//!
//! Every read of the trait returns a `Result`: a database failure is
//! `data_policy.store_failed` naming the operation (`outcome`, `checkpoint`, ...), never "no
//! row". The engine stops at the first failed read, applies nothing further and leaves its
//! checkpoint as it was; a later run with a healthy store resumes from the outcome rows.

use std::sync::{Arc, Mutex};

use spot_data_policy::store::{ApplyItem, Checkpoint, ItemOutcome, RetentionStore};
use spot_data_policy::{DataPolicyError, DeletionProof, Record};
use spot_pg_bridge::Bridge;
use spot_tenancy::TenantId;

use crate::error::{PgPolicyError, store_failed};
use crate::ledger::PgLedger;
use crate::source::RecordSource;

/// The retention store of one tenant: ledger in PostgreSQL, records from `S`.
pub struct PgRetentionStore<S: RecordSource> {
    ledger: PgLedger,
    source: Arc<Mutex<S>>,
}

impl<S: RecordSource> PgRetentionStore<S> {
    /// Builds the store. `pool_size` is the size of the pool the bridge was built on. Call
    /// [`PgRetentionStore::prepare`] once before first use.
    ///
    /// # Errors
    /// `data_policy_pg.unsupported_context`, see [`PgLedger::new`].
    pub fn new(
        bridge: Arc<Bridge>,
        pool_size: usize,
        tenant: TenantId,
        source: S,
    ) -> Result<Self, PgPolicyError> {
        Ok(Self {
            ledger: PgLedger::new(bridge, pool_size, tenant)?,
            source: Arc::new(Mutex::new(source)),
        })
    }

    /// Creates the ledger tables if missing.
    ///
    /// # Errors
    /// `data_policy_pg.prepare_failed`.
    pub fn prepare(&self) -> Result<(), PgPolicyError> {
        self.ledger.prepare()
    }

    /// The ledger, for callers that want its fallible reads.
    #[must_use]
    pub fn ledger(&self) -> &PgLedger {
        &self.ledger
    }
}

impl<S: RecordSource> RetentionStore for PgRetentionStore<S> {
    fn records(&self) -> Result<Vec<Record>, DataPolicyError> {
        // A poisoned source lock means the source panicked: the live view is gone, so say so
        // instead of presenting an empty tenant.
        self.source
            .lock()
            .map_err(|_| store_failed("the records read failed: the record source is unusable"))?
            .records()
    }

    fn record(&self, id: &str) -> Result<Option<Record>, DataPolicyError> {
        self.source
            .lock()
            .map_err(|_| store_failed("the record read failed: the record source is unusable"))?
            .record(id)
    }

    fn apply_item(&mut self, item: ApplyItem) -> Result<(), DataPolicyError> {
        self.ledger.apply_item(&self.source, item)
    }

    fn outcome(
        &self,
        run_id: &str,
        record_id: &str,
    ) -> Result<Option<ItemOutcome>, DataPolicyError> {
        self.ledger.outcome(run_id, record_id)
    }

    fn run_outcomes(&self, run_id: &str) -> Result<Vec<ItemOutcome>, DataPolicyError> {
        self.ledger.run_outcomes(run_id)
    }

    fn save_checkpoint(&mut self, checkpoint: Checkpoint) -> Result<(), DataPolicyError> {
        self.ledger.save_checkpoint(&checkpoint)
    }

    fn checkpoint(&self, run_id: &str) -> Result<Option<Checkpoint>, DataPolicyError> {
        self.ledger.checkpoint(run_id)
    }

    fn proof(&self, record_id: &str) -> Result<Option<DeletionProof>, DataPolicyError> {
        self.ledger.proof(record_id)
    }

    fn proofs(&self) -> Result<Vec<DeletionProof>, DataPolicyError> {
        self.ledger.proofs()
    }
}
