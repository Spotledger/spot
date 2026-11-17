//! The seams of the conversation module: what it asks of the link host, what it offers the link
//! host, and the peer directory it builds from its configuration.
//!
//! The link host (accepting inbound link traffic in `spot serve`, the link stores, the transport
//! carriers) belongs to another hosting task. This module is written against two traits:
//!
//! - [`LinkResources`], which the link host provides as a port: the link as the host holds it
//!   (so a revocation or an expiry there stops the sender and the receiver at once), the link
//!   stores, the outbox, inbox, notice feed and quarantine of the exchange, the link audit chain
//!   and the optional region oracle.
//! - [`LinkMessageSink`], which this module provides as a port: the link host hands every
//!   inbound message, with the party its carrier authenticated, to the sink and sends the
//!   receipts the answer carries back to the peer.
//!
//! Without the link host nothing registers; the in-process pair in [`super::memory`] implements
//! [`LinkResources`] over in-memory stores and carries messages between two sites in one process.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use spot_bpm_conversations::{
    LinkSender, MessageSender, PeerDirectory, ReceiveError, SharedLink, SharedSigner,
};
use spot_link_exchange::{
    InboxStore, InputQuarantine, Lifetimes, Message, NoticeFeed, OutboxStore, ReceiveOutcome,
    TransportParty,
};
use spot_tenant_link::{AuditSink, LinkId, LinkStore, Party, RegionOracle};

use super::settings::PeerSetting;

/// What the link host offers this module. Every handle is the host's own: nothing here is a copy
/// that could go stale, and nothing here is shared between sites.
pub trait LinkResources: Send + Sync {
    /// The party this site is on its links.
    fn party(&self) -> Party;

    /// The link as the host currently holds it; revoking or expiring it there stops the sender
    /// and the receiver. `None` for a link this site does not know.
    fn link(&self, id: &LinkId) -> Option<SharedLink>;

    /// The link store the inbox re-reads a link from before it marks a record applied.
    fn links(&self) -> Arc<dyn LinkStore>;

    /// The sender outbox.
    fn outbox(&self) -> Arc<dyn OutboxStore>;

    /// The receiver inbox.
    fn inbox(&self) -> Arc<dyn InboxStore>;

    /// The delivery notice feed.
    fn feed(&self) -> Arc<dyn NoticeFeed>;

    /// The input quarantine, and the count the bounds read from it.
    fn quarantine(&self) -> Arc<dyn InputQuarantine>;

    /// The link audit chain.
    fn audit(&self) -> Arc<dyn AuditSink>;

    /// The oracle that places a peer, for a grant that requires a peer region. `None` refuses
    /// every such grant (ADR-058 rule 6, fail closed).
    fn region(&self) -> Option<Arc<dyn RegionOracle>>;

    /// How long the outbox keeps what it has sent.
    fn lifetimes(&self) -> Lifetimes;
}

/// What this module offers the link host.
pub trait LinkMessageSink: Send + Sync {
    /// Takes one message the carrier delivered on `link`.
    ///
    /// `transport` is the party the carrier authenticated for the connection, never a value read
    /// from the message. The answer carries the receipts the host returns to the peer. A message
    /// the sink does not take (a limit, an unknown link) is answered `Busy` without a receipt,
    /// so the sender keeps it and sends again later.
    ///
    /// # Errors
    /// [`ReceiveError`] when the exchange, the audit trail or the conversation store fails:
    /// nothing was acknowledged and the sender keeps the message.
    fn receive(
        &self,
        link: &LinkId,
        transport: &TransportParty,
        message: &Message,
    ) -> Result<ReceiveOutcome, ReceiveError>;
}

/// One configured route: the peer on `link` plays `peer_role` in `contract`.
#[derive(Clone)]
struct Route {
    contract: String,
    peer_role: String,
    link: LinkId,
}

/// The peer directory a site builds from `[[services.process_conversations.peers]]`: a fixed
/// table of routes, with one sender per link built on first use over the host's own handles.
pub struct ConfigPeers {
    routes: Vec<Route>,
    resources: Arc<dyn LinkResources>,
    signer: SharedSigner,
    senders: Mutex<BTreeMap<LinkId, Arc<LinkSender>>>,
}

impl ConfigPeers {
    /// The directory of `peers`. A route whose link text is not a link id is skipped (settings
    /// validation already refused it).
    #[must_use]
    pub fn new(
        peers: &[PeerSetting],
        resources: Arc<dyn LinkResources>,
        signer: SharedSigner,
    ) -> Self {
        let routes = peers
            .iter()
            .filter_map(|peer| {
                Some(Route {
                    contract: peer.contract.clone(),
                    peer_role: peer.role.clone(),
                    link: peer.link.parse().ok()?,
                })
            })
            .collect();
        Self {
            routes,
            resources,
            signer,
            senders: Mutex::new(BTreeMap::new()),
        }
    }
}

impl PeerDirectory for ConfigPeers {
    fn route(&self, contract: &str, role: &str) -> Option<LinkId> {
        self.routes
            .iter()
            .find(|route| route.contract == contract && route.peer_role == role)
            .map(|route| route.link)
    }

    fn peer_role(&self, link: &LinkId, contract: &str) -> Option<String> {
        self.routes
            .iter()
            .find(|route| &route.link == link && route.contract == contract)
            .map(|route| route.peer_role.clone())
    }

    fn sender(&self, link: &LinkId) -> Option<Arc<dyn MessageSender>> {
        // Only a configured link has a sender: a link id that is not in the table never reaches
        // the host's stores.
        if !self.routes.iter().any(|route| &route.link == link) {
            return None;
        }
        let mut senders = self
            .senders
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(sender) = senders.get(link) {
            return Some(Arc::clone(sender) as Arc<dyn MessageSender>);
        }
        let shared = self.resources.link(link)?;
        let sender = Arc::new(LinkSender::new(
            shared,
            self.resources.outbox(),
            self.resources.audit(),
            Arc::clone(&self.signer),
            self.resources.lifetimes(),
        ));
        senders.insert(*link, Arc::clone(&sender));
        Some(sender as Arc<dyn MessageSender>)
    }
}
