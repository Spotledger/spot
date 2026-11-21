//! The real relay as a [`ShipRelay`], and the factory that opens it over any journal store.

use std::sync::Arc;

use spot_journal_manifest::{Fork, VerifyingKeys};
use spot_journal_relay::{ManifestSigner, Relay, RelayConfig, RelayError};
use spot_journal_store::{JournalStore, StoreError};

use crate::error::ShipFault;
use crate::ports::{PortFuture, RelayFactory, ShipRelay};

/// The shipper's view of a relay failure: the same split the commit driver makes.
pub(crate) fn classify(error: &RelayError) -> ShipFault {
    let code = error.code();
    match error {
        RelayError::Store(StoreError::Io { .. } | StoreError::Backend { .. })
        | RelayError::ResumeRequired => ShipFault::StoreDown { code },
        _ => ShipFault::Stopped { code },
    }
}

impl<S: JournalStore> ShipRelay for Relay<S> {
    async fn seal_if_due(&mut self, now_millis: i64) -> Result<bool, ShipFault> {
        Relay::seal_if_due(self, now_millis)
            .await
            .map_err(|error| classify(&error))
    }

    fn sealed_through(&self) -> u64 {
        self.manifest().last_seq()
    }
}

/// Builds the manifest signer a new relay takes (the signer is not `Clone`, so each relay gets
/// its own).
pub type SignerFn = dyn Fn() -> Box<dyn ManifestSigner> + Send + Sync;

/// Opens real relays over stores made by `make_store`.
pub struct StoreRelayFactory<S: JournalStore + 'static> {
    make_store: Arc<dyn Fn() -> Result<S, ShipFault> + Send + Sync>,
    make_signer: Arc<SignerFn>,
    keys: VerifyingKeys,
    config: RelayConfig,
}

impl<S: JournalStore + 'static> StoreRelayFactory<S> {
    /// A factory for `config`'s tenant, source and durability. `make_store` opens a handle on
    /// the journal store; `make_signer` a signer under the tenant's journal key; `keys` is the
    /// public key set the manifest is verified against.
    pub fn new(
        make_store: Arc<dyn Fn() -> Result<S, ShipFault> + Send + Sync>,
        make_signer: Arc<SignerFn>,
        keys: VerifyingKeys,
        config: RelayConfig,
    ) -> Self {
        Self {
            make_store,
            make_signer,
            keys,
            config,
        }
    }
}

impl<S: JournalStore + 'static> RelayFactory for StoreRelayFactory<S> {
    type Relay = Relay<S>;

    fn open<'a>(
        &'a self,
        generation: u64,
        fork: Option<Fork>,
        now_millis: i64,
    ) -> PortFuture<'a, Result<Self::Relay, ShipFault>> {
        Box::pin(async move {
            let store = (self.make_store)()?;
            let signer = (self.make_signer)();
            let config = self.config.clone();
            let opened = match fork {
                None if generation == 1 => {
                    Relay::open(store, signer, self.keys.clone(), config, now_millis).await
                }
                Some(fork) if fork.generation == generation => {
                    Relay::open_fork(store, signer, self.keys.clone(), config, fork, now_millis)
                        .await
                }
                _ => {
                    return Err(ShipFault::Config(
                        "a generation after the first needs its fork record",
                    ));
                }
            };
            opened.map_err(|error| classify(&error))
        })
    }
}
