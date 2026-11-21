//! Audit in both chains: every promotion transition is recorded in the **source** site's and
//! the **target** site's migration audit chain (`spot-migration-audit` over the site's
//! hash-chained `PgAuditStore`), so each site can show the promotion on its own.
//!
//! The chains are written from a thread outside any runtime, so each write goes through the
//! runtime handle. A chain that is not guarded (the audit tables' role and triggers are not in
//! place) is not written: the promotion is refused instead (fail closed).

use std::sync::Arc;

use spot_audit_chain::{AuditChain, AuditError as ChainError, CheckpointPolicy, CheckpointSigner};
use spot_audit_store::PgAuditStore;
use spot_migration_audit::{
    ApplyEvent, ApprovalEvent, ChainRunRecorder, FinishEvent, RehearsalEvent, RunId,
};
use spot_postgres::PgBackend;
use spot_promotion::ports::PromotionFuture;
use spot_promotion::{MigrationAudit, OpenRun, PromotionAudit};

/// A signer that never signs: the chain's automatic checkpoints are off (the site key provider
/// is WP-SEC-03), so it is never asked, and a refusal keeps an unsigned checkpoint from being
/// written.
struct NoSigner;

impl CheckpointSigner for NoSigner {
    fn key_id(&self) -> String {
        String::new()
    }

    fn sign(&self, _message: &[u8]) -> Result<Vec<u8>, ChainError> {
        Err(ChainError::Signer("no checkpoint key is configured".into()))
    }
}

/// The migration audit of one site's chain.
///
/// # Errors
/// A message when the audit tables cannot be checked or are not guarded.
pub async fn chain_of(backend: &PgBackend, site: &str) -> Result<Arc<MigrationAudit>, String> {
    let protection = spot_audit_store::verify(&backend.pool())
        .await
        .map_err(|_| format!("the audit chain of `{site}` cannot be checked"))?;
    if !protection.is_complete() {
        return Err(format!(
            "the audit chain of `{site}` is not guarded; start the site once so it is prepared"
        ));
    }
    let chain = AuditChain::new(
        PgAuditStore::new(backend.pool()),
        site,
        Arc::new(NoSigner),
        CheckpointPolicy { every: 0 },
    );
    Ok(Arc::new(MigrationAudit::new(Arc::new(
        ChainRunRecorder::new(chain),
    ))))
}

/// Writes every transition to two chains, the target's first.
pub struct DualAudit {
    handle: tokio::runtime::Handle,
    target: Arc<dyn PromotionAudit>,
    source: Arc<dyn PromotionAudit>,
}

impl DualAudit {
    /// An audit over the two chains; `handle` is the runtime their stores run on.
    #[must_use]
    pub fn new(
        handle: tokio::runtime::Handle,
        target: Arc<dyn PromotionAudit>,
        source: Arc<dyn PromotionAudit>,
    ) -> DualAudit {
        DualAudit {
            handle,
            target,
            source,
        }
    }
}

impl PromotionAudit for DualAudit {
    fn open<'a>(&'a self, request: OpenRun<'a>) -> PromotionFuture<'a> {
        Box::pin(async move {
            let second = OpenRun {
                promotion: request.promotion,
                run: request.run.clone(),
                plan_digest: request.plan_digest,
                plan: request.plan.clone(),
                actor: request.actor,
                at: request.at,
            };
            self.handle.block_on(self.target.open(request))?;
            self.handle.block_on(self.source.open(second))
        })
    }

    fn approval<'a>(&'a self, promotion: &'a RunId, event: ApprovalEvent) -> PromotionFuture<'a> {
        Box::pin(async move {
            self.handle
                .block_on(self.target.approval(promotion, event.clone()))?;
            self.handle.block_on(self.source.approval(promotion, event))
        })
    }

    fn rehearsal<'a>(&'a self, promotion: &'a RunId, event: RehearsalEvent) -> PromotionFuture<'a> {
        Box::pin(async move {
            self.handle
                .block_on(self.target.rehearsal(promotion, event.clone()))?;
            self.handle
                .block_on(self.source.rehearsal(promotion, event))
        })
    }

    fn applied<'a>(&'a self, promotion: &'a RunId, event: ApplyEvent) -> PromotionFuture<'a> {
        Box::pin(async move {
            self.handle
                .block_on(self.target.applied(promotion, event.clone()))?;
            self.handle.block_on(self.source.applied(promotion, event))
        })
    }

    fn finish<'a>(&'a self, promotion: &'a RunId, event: FinishEvent) -> PromotionFuture<'a> {
        Box::pin(async move {
            self.handle
                .block_on(self.target.finish(promotion, event.clone()))?;
            self.handle.block_on(self.source.finish(promotion, event))
        })
    }
}
