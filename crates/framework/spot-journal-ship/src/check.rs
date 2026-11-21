//! Continuous verification: a light pass over the stored chain of one generation.

use spot_journal_manifest::VerifyingKeys;
use spot_journal_store::{JournalStore, TenantId};
use spot_journal_verify::{Mode, PinnedExpected, verify_pinned};

use crate::error::ShipFault;
use crate::ports::{ChainChecker, ChainState, PortFuture};

/// Verifies the tenant's journal in `store`: the manifest signature and ownership, every listed
/// segment present with the hash the manifest lists. Tail chunks are not looked at.
pub struct StoreChecker<S: JournalStore> {
    store: S,
    keys: VerifyingKeys,
    tenant: TenantId,
    source: String,
}

impl<S: JournalStore> StoreChecker<S> {
    /// A checker for `tenant` and `source` over its own handle on the store.
    ///
    /// # Errors
    /// [`ShipFault::Config`] when the tenant id is not one safe path component.
    pub fn new(
        store: S,
        keys: VerifyingKeys,
        tenant: &str,
        source: &str,
    ) -> Result<Self, ShipFault> {
        let tenant = TenantId::new(tenant)
            .map_err(|_| ShipFault::Config("the tenant id is not one safe path component"))?;
        Ok(Self {
            store,
            keys,
            tenant,
            source: source.to_owned(),
        })
    }
}

impl<S: JournalStore> ChainChecker for StoreChecker<S> {
    fn check(&self, generation: u64) -> PortFuture<'_, Result<ChainState, ShipFault>> {
        Box::pin(async move {
            let expected =
                PinnedExpected::new(self.tenant.clone(), self.source.clone(), generation)
                    .map_err(|_| ShipFault::Config("the verification question is invalid"))?;
            match verify_pinned(&self.store, &self.keys, &expected, Mode::Light).await {
                // An unlisted segment or a position beyond the manifest is the ordinary crash
                // between sealing and the manifest update (the relay repairs it): not damage.
                Ok(report) if !report.has_tampering() => Ok(ChainState::Clean),
                Ok(report) => {
                    let code = report
                        .findings
                        .iter()
                        .find(|finding| finding.is_tampering())
                        .map_or("journal_verify.finding", |finding| finding.code());
                    Ok(ChainState::Damaged { code })
                }
                // The store could not be read: that is an outage, not damage.
                Err(_) => Err(ShipFault::StoreDown {
                    code: "journal_verify.store_failed",
                }),
            }
        })
    }
}
