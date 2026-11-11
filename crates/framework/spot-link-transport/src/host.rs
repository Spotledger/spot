//! What a side brings: its link store, its exchange stores, its audit trail and its key.
//!
//! Both the [`LinkServer`](crate::LinkServer) and the [`LinkClient`](crate::LinkClient) are built
//! over one [`LinkHost`]. Each request builds a short-lived `Inbox` or `Outbox` over the host's
//! stores, so nothing here keeps state between requests: the stores are the state.

use std::sync::Arc;

use spot_core::Clock;
use spot_link_exchange::{
    IdSource, Inbox, InboxConfig, InboxStore, InputQuarantine, Lifetimes, Outbox, OutboxConfig,
    OutboxStore,
};
use spot_tenant_link::{AuditSink, IdentitySigner, LinkStore, RegionOracle};

use crate::error::{Error, Result};

/// An identity key the host can share between tasks.
pub type SharedSigner = Arc<dyn IdentitySigner + Send + Sync>;

/// One side of a link: everything the exchange needs.
#[derive(Clone)]
pub struct LinkHost {
    /// The side's links.
    pub links: Arc<dyn LinkStore>,
    /// The sender's durable state.
    pub outbox: Arc<dyn OutboxStore>,
    /// The receiver's durable state.
    pub inbox: Arc<dyn InboxStore>,
    /// The audit trail.
    pub audit: Arc<dyn AuditSink>,
    /// Where refused inputs go.
    pub quarantine: Arc<dyn InputQuarantine>,
    /// This side's identity key (signs receipts and tombstones).
    pub signer: SharedSigner,
    /// Message ids.
    pub ids: Arc<dyn IdSource>,
    /// Message lifetimes.
    pub lifetimes: Lifetimes,
    /// Receiver limits.
    pub inbox_config: InboxConfig,
    /// Sender tuning.
    pub outbox_config: OutboxConfig,
    /// The clock every instant comes from.
    pub clock: Arc<dyn Clock>,
    /// Places the peer for a grant that requires a peer region (ADR-058 rule 6). `None` refuses
    /// every message on such a grant.
    pub region: Option<Arc<dyn RegionOracle + Send + Sync>>,
}

impl LinkHost {
    /// The current Unix milliseconds.
    ///
    /// # Errors
    /// [`Error::unavailable`] when the clock is before 1970 or out of range: nothing is done on
    /// a clock that cannot be trusted.
    pub fn now_ms(&self) -> Result<u64> {
        let nanos = self.clock.now_utc().unix_timestamp_nanos();
        u64::try_from(nanos / 1_000_000).map_err(|_| Error::unavailable())
    }

    /// The receiver over the host's stores.
    #[must_use]
    pub fn inbox(&self) -> Inbox<'_> {
        let inbox = Inbox::new(
            self.inbox.as_ref(),
            self.audit.as_ref(),
            self.quarantine.as_ref(),
            self.signer.as_ref(),
        )
        .with_config(self.inbox_config)
        // The apply path re-reads the link before it records a message as applied, so a link
        // revoked while a record was in flight never produces an applied row.
        .with_links(self.links.as_ref());
        match &self.region {
            Some(oracle) => inbox.with_region_oracle(oracle.as_ref()),
            None => inbox,
        }
    }

    /// The sender over the host's stores.
    #[must_use]
    pub fn outbox(&self) -> Outbox<'_> {
        Outbox::new(
            self.outbox.as_ref(),
            self.audit.as_ref(),
            self.signer.as_ref(),
            self.ids.as_ref(),
            self.lifetimes.clone(),
        )
        .with_config(self.outbox_config)
    }
}

impl std::fmt::Debug for LinkHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinkHost").finish_non_exhaustive()
    }
}

/// Runs a store-touching closure off the async threads: the stores are synchronous and a
/// PostgreSQL one blocks on I/O.
pub(crate) async fn blocking<T, F>(work: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| Error::unavailable())
}
