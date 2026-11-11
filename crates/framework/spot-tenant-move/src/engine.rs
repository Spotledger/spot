//! The move engine: the persisted state machine over the ports.
//!
//! [`MoveEngine::advance`] performs exactly one state transition (and, inside a transfer
//! state, every chunk that state needs). It persists after every transition and every
//! chunk, so a process that dies can call [`MoveEngine::run`] again and continue from the
//! last checkpoint. [`MoveEngine::abort`] rolls back everything before `CutOver`.

use spot_tenancy::TenantId;

use crate::MAX_CHUNK_BYTES;
use crate::error::{Error, Result};
use crate::placement::PlacementPort;
use crate::plan::{MoveHalt, MoveId, MovePlan, MoveState};
use crate::quiesce::QuiescePort;
use crate::residency::ResidencyPolicy;
use crate::store::{MoveRecord, MoveStore};
use crate::transfer::TransferPort;
use crate::verify::{FingerprintSource, Side, Verification};

/// The number of transitions [`MoveEngine::run`] will take before declaring no progress.
const MAX_TRANSITIONS: usize = 32;

/// The engine. Every port is borrowed for the lifetime of one call; the store is the
/// only durable collaborator.
pub struct MoveEngine<'a> {
    /// The persisted move progress.
    pub store: &'a dyn MoveStore,
    /// The per-tenant logical dump/restore provider.
    pub transfer: &'a dyn TransferPort,
    /// The source's gate and side-effect hold.
    pub quiesce: &'a dyn QuiescePort,
    /// The placement map and routing caches.
    pub placement: &'a dyn PlacementPort,
    /// The residency policy.
    pub residency: &'a dyn ResidencyPolicy,
    /// The fingerprint engine.
    pub fingerprints: &'a dyn FingerprintSource,
}

impl<'a> MoveEngine<'a> {
    /// Records a new move in `Planned`, after the residency policy allows the target
    /// region. Nothing is quiesced yet.
    ///
    /// # Errors
    /// [`Error::InvalidPlan`], [`Error::ResidencyRefused`], [`Error::AlreadyExists`] or a
    /// store failure.
    pub fn start(&self, plan: MovePlan, id: MoveId, now: u64) -> Result<MoveRecord> {
        plan.validate()?;
        self.residency.check(&plan)?;
        let record = MoveRecord::new(id, plan, now);
        self.store.create(record.clone())?;
        Ok(record)
    }

    /// Reads the move of `tenant`.
    ///
    /// # Errors
    /// [`Error::NotFound`] or a store failure.
    pub fn load(&self, tenant: &TenantId) -> Result<MoveRecord> {
        self.store.get(tenant)?.ok_or_else(|| Error::NotFound {
            tenant: tenant.to_string(),
        })
    }

    /// Performs one transition. At a transfer state it consumes every remaining chunk of
    /// that pass, saving the cursor after each one.
    ///
    /// # Errors
    /// A port or store error. The record keeps the last checkpoint it saved, so the same
    /// call can be repeated to resume.
    pub async fn advance(&self, tenant: &TenantId, now: u64) -> Result<MoveRecord> {
        let record = self.load(tenant)?;
        match record.state {
            MoveState::Planned => self.quiesce_step(record, now).await,
            MoveState::Quiesced => self.export_step(record, now).await,
            MoveState::Exported => self.import_step(record, now).await,
            MoveState::Imported => self.verify_step(record, now).await,
            MoveState::Verified => self.cut_over_step(record, now).await,
            MoveState::CutOver => self.clean_step(record, now).await,
            // Terminal states are idempotent: a retried run changes nothing.
            MoveState::Cleaned | MoveState::Aborted => Ok(record),
        }
    }

    /// Advances until the move is terminal, returning the final record.
    ///
    /// # Errors
    /// The first error [`advance`](Self::advance) reports. The caller decides whether to
    /// retry (the record has its checkpoint) or [`abort`](Self::abort).
    pub async fn run(&self, tenant: &TenantId, now: u64) -> Result<MoveRecord> {
        let mut record = self.load(tenant)?;
        for _ in 0..MAX_TRANSITIONS {
            if record.is_terminal() {
                return Ok(record);
            }
            record = self.advance(tenant, now).await?;
        }
        Err(Error::StepFailed {
            tenant: tenant.to_string(),
            state: record.state,
        })
    }

    /// Rolls the move back before cut-over: releases the hold, opens the gate, restores
    /// any placement a partial cut-over changed and discards the transfer staging. The
    /// source serves again and the record ends in `Aborted`.
    ///
    /// # Errors
    /// [`Error::AbortTooLate`] from `CutOver` onward, or a port/store failure. An already
    /// aborted move is returned unchanged.
    pub async fn abort(
        &self,
        tenant: &TenantId,
        now: u64,
        reason: &'static str,
    ) -> Result<MoveRecord> {
        let mut record = self.load(tenant)?;
        if record.state == MoveState::Aborted {
            return Ok(record);
        }
        if !record.state.is_before_cut_over() {
            return Err(Error::AbortTooLate {
                tenant: tenant.to_string(),
            });
        }
        self.quiesce.resume(&record.plan, &record.id, now).await?;
        self.placement.restore(&record.plan).await?;
        self.transfer.cleanup(&record.plan).await?;
        record.abort(now, reason.to_owned())?;
        self.persist(record)
    }

    fn persist(&self, record: MoveRecord) -> Result<MoveRecord> {
        let expected = record.revision;
        self.store.save(record, expected)
    }

    async fn quiesce_step(&self, mut record: MoveRecord, now: u64) -> Result<MoveRecord> {
        // The policy is asked again in case it changed while the plan waited in the queue.
        self.residency.check(&record.plan)?;
        self.quiesce.quiesce(&record.plan, &record.id, now).await?;
        record.advance(MoveState::Quiesced, now)?;
        self.persist(record)
    }

    async fn export_step(&self, mut record: MoveRecord, now: u64) -> Result<MoveRecord> {
        loop {
            let after = record
                .export_next
                .checked_sub(1)
                .filter(|_| record.export_next > 0);
            let Some(chunk) = self.transfer.export_chunk(&record.plan, after).await? else {
                record.advance(MoveState::Exported, now)?;
                return self.persist(record);
            };
            check_chunk(&record, &chunk)?;
            record.exported.add(chunk.len());
            record.export_next = record.export_next.saturating_add(1);
            record.updated_at = now;
            record = self.persist(record)?;
        }
    }

    async fn import_step(&self, mut record: MoveRecord, now: u64) -> Result<MoveRecord> {
        loop {
            let after = record
                .import_next
                .checked_sub(1)
                .filter(|_| record.import_next > 0);
            let Some(chunk) = self.transfer.export_chunk(&record.plan, after).await? else {
                self.transfer.finish_import(&record.plan).await?;
                record.advance(MoveState::Imported, now)?;
                return self.persist(record);
            };
            check_chunk(&record, &chunk)?;
            self.transfer.import_chunk(&record.plan, &chunk).await?;
            record.imported.add(chunk.len());
            record.import_next = record.import_next.saturating_add(1);
            record.updated_at = now;
            record = self.persist(record)?;
        }
    }

    async fn verify_step(&self, mut record: MoveRecord, now: u64) -> Result<MoveRecord> {
        let source = self
            .fingerprints
            .measure(&record.plan, Side::Source)
            .await?;
        let target = self
            .fingerprints
            .measure(&record.plan, Side::Target)
            .await?;
        let verification = Verification::between(&source, &target);
        record.verification = Some(verification.clone());
        record.updated_at = now;
        if !verification.matched {
            record.halt = Some(MoveHalt::FingerprintMismatch {
                source_digest: verification.source_digest.clone(),
                target_digest: verification.target_digest.clone(),
                differences: verification.differences,
            });
            let tenant = record.plan.tenant.to_string();
            self.persist(record)?;
            return Err(Error::FingerprintMismatch {
                tenant,
                differences: verification.differences,
            });
        }
        record.advance(MoveState::Verified, now)?;
        self.persist(record)
    }

    async fn cut_over_step(&self, mut record: MoveRecord, now: u64) -> Result<MoveRecord> {
        self.placement.cut_over(&record.plan, now).await?;
        record.advance(MoveState::CutOver, now)?;
        self.persist(record)
    }

    async fn clean_step(&self, mut record: MoveRecord, now: u64) -> Result<MoveRecord> {
        self.quiesce.resume(&record.plan, &record.id, now).await?;
        self.transfer.cleanup(&record.plan).await?;
        record.advance(MoveState::Cleaned, now)?;
        self.persist(record)
    }
}

/// Rejects a chunk that is out of order, over the accounting bound or checksum-broken.
fn check_chunk(record: &MoveRecord, chunk: &crate::transfer::Chunk) -> Result<()> {
    let expected = if record.state == MoveState::Quiesced {
        record.export_next
    } else {
        record.import_next
    };
    if chunk.sequence != expected {
        return Err(Error::TransferFailed {
            tenant: record.plan.tenant.to_string(),
            reason: "the transfer stream is not sequential",
        });
    }
    if chunk.len() > u64::try_from(MAX_CHUNK_BYTES).unwrap_or(u64::MAX) {
        return Err(Error::TransferFailed {
            tenant: record.plan.tenant.to_string(),
            reason: "a transfer chunk is larger than the bound",
        });
    }
    if !chunk.verify() {
        return Err(Error::ChunkCorrupt {
            tenant: record.plan.tenant.to_string(),
            sequence: chunk.sequence,
        });
    }
    Ok(())
}
