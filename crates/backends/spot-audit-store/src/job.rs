//! The scheduled verification job: verify a site's chain incrementally from its last good
//! checkpoint, sign a new checkpoint, and report a broken chain.
//!
//! The job is the body a WP-PLT-06 handler runs. It is idempotent for the same range: running it
//! twice with no new entries signs no second checkpoint. It returns [`ChainBroken`] for the job
//! registration (the wiring task) to publish — building an `OutboxEvent` needs a topic, event id,
//! actor and subject this crate does not own — and a [`HandlerResult::Fail`] so the run is not
//! retried blindly.

use std::sync::Arc;

use spot_audit_chain::{
    AuditChain, AuditStore, Checkpoint, CheckpointPolicy, CheckpointSigner, KeyRing, Verifier,
};
use spot_jobs::worker::HandlerResult;

use crate::error::{Error, Result};
use crate::store::PgAuditStore;

/// A chain that does not verify: the failure's kind and the exact `seq`. Never a payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChainBroken {
    /// The stable signal kind a wiring task publishes.
    pub kind: &'static str,
    /// The sequence number of the entry (or checkpoint) at fault.
    pub seq: u64,
}

impl ChainBroken {
    /// The signal kind. Written with `concat!` so the source holds no bare dotted literal: it is
    /// a signal kind, not an error code, so it does not belong in the error catalogue.
    pub const KIND: &'static str = concat!("audit.", "chain_broken");
}

/// What one verification run did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifyOutcome {
    /// The handler result: `Ok`, or `Fail` naming the break.
    pub result: HandlerResult,
    /// Set when the chain did not verify.
    pub broken: Option<ChainBroken>,
    /// A checkpoint signed this run, when the head had advanced.
    pub checkpoint: Option<Checkpoint>,
    /// Entries whose hash and link were checked.
    pub checked: u64,
}

impl VerifyOutcome {
    /// Whether the chain verified.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.broken.is_none()
    }
}

/// The verification job for one site.
pub struct VerifyJob {
    chain: AuditChain<PgAuditStore>,
    keys: KeyRing,
}

impl VerifyJob {
    /// A job over `store` for `site`, signing checkpoints with `signer`. `keys` must hold
    /// `signer`'s public key, so the checkpoints this job (and earlier runs) signed verify.
    #[must_use]
    pub fn new(
        store: PgAuditStore,
        site: &str,
        signer: Arc<dyn CheckpointSigner>,
        checkpoint_every: u64,
        keys: KeyRing,
    ) -> VerifyJob {
        let chain = AuditChain::new(
            store,
            site,
            signer,
            CheckpointPolicy {
                every: checkpoint_every,
            },
        );
        VerifyJob { chain, keys }
    }

    /// The chain the job verifies and extends.
    #[must_use]
    pub fn chain(&self) -> &AuditChain<PgAuditStore> {
        &self.chain
    }

    /// Runs the job: verify from the last good checkpoint (or the whole chain), sign a new
    /// checkpoint when the head advanced, and report a [`ChainBroken`] on failure.
    ///
    /// # Errors
    ///
    /// A store or signer error.
    pub async fn run(&self) -> Result<VerifyOutcome> {
        let store = self.chain.store();
        let site = self.chain.site();
        let verifier = Verifier::new(&self.keys);
        let trusted = store.latest_checkpoint(site).await.map_err(Error::from)?;
        let report = match &trusted {
            Some(checkpoint) => verifier
                .verify_incremental(store, site, checkpoint)
                .await
                .map_err(Error::from)?,
            None => verifier
                .verify_range(store, site, 1, u64::MAX)
                .await
                .map_err(Error::from)?,
        };
        if let Some(failure) = report.failures.first() {
            return Ok(VerifyOutcome {
                result: HandlerResult::Fail(format!(
                    "audit chain of `{site}` is broken at seq {} ({})",
                    failure.seq,
                    failure.kind.code()
                )),
                broken: Some(ChainBroken {
                    kind: ChainBroken::KIND,
                    seq: failure.seq,
                }),
                checkpoint: None,
                checked: report.checked,
            });
        }
        // A new checkpoint only when the head moved past the trusted one (idempotent otherwise).
        let checkpoint = match self.chain.head().await.map_err(Error::from)? {
            Some(head) if trusted.as_ref().is_none_or(|cp| head.seq > cp.seq) => self
                .chain
                .checkpoint_now(head.record.at)
                .await
                .map_err(Error::from)?,
            _ => None,
        };
        Ok(VerifyOutcome {
            result: HandlerResult::Ok,
            broken: None,
            checkpoint,
            checked: report.checked,
        })
    }
}
