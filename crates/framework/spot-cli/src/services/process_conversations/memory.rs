//! The in-process pair: [`LinkResources`] over in-memory stores and a carrier that hands messages
//! from one site to another in the same process.
//!
//! This is what the module runs on when the link host is absent: a deployment that holds both
//! halves of a conversation (a demonstration, a single-organisation test bed) and the tests of
//! this module, which join two sites through the real link crates with no network. It is a
//! carrier adapter in the sense of `TransportParty`: it names the party the connection
//! authenticated (here, the sending site's own party), never a value read from a message.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use spot_bpm_conversations::{SharedLink, SharedSigner};
use spot_link_exchange::{
    IdSource, InboxStore, InputQuarantine, Lifetimes, MemoryInbox, MemoryOutbox, MemoryQuarantine,
    Message, MessageId, NoticeFeed, Outbox, OutboxStore, TransportParty,
};
use spot_tenant_link::{
    AuditSink, Error as LinkError, Link, LinkId, LinkState, LinkStore, MemoryAuditLog,
    MemoryRegionOracle, Party, RegionOracle, Result as LinkResult,
};

use super::host::ConversationCore;
use super::seams::LinkResources;

/// The links of a site, each behind its shared handle.
#[derive(Default)]
struct Links(RwLock<Vec<SharedLink>>);

impl Links {
    fn all(&self) -> Vec<SharedLink> {
        self.0
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn shared(&self, id: &LinkId) -> Option<SharedLink> {
        self.all()
            .into_iter()
            .find(|shared| &snapshot(shared).id == id)
    }
}

fn snapshot(shared: &SharedLink) -> Link {
    shared
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// A [`LinkStore`] over the links a site holds, read through their shared handles so a
/// revocation or expiry written there is seen by every reader at once. It is read-only: a link is
/// changed by replacing it in the site ([`InProcessSite::replace_link`]).
pub struct SharedLinkStore(Arc<Links>);

fn live(state: LinkState) -> bool {
    !matches!(state, LinkState::Revoked | LinkState::Expired)
}

impl LinkStore for SharedLinkStore {
    fn insert(&self, _link: Link) -> LinkResult<Link> {
        Err(LinkError::StoreFailed {
            reason: "the in-process link store is read-only",
        })
    }

    fn get(&self, id: &LinkId) -> LinkResult<Link> {
        self.0
            .shared(id)
            .map(|shared| snapshot(&shared))
            .ok_or_else(|| LinkError::NotFound {
                link: id.to_string(),
            })
    }

    fn update(&self, _link: Link, _expected_version: u64) -> LinkResult<Link> {
        Err(LinkError::StoreFailed {
            reason: "the in-process link store is read-only",
        })
    }

    fn find(&self, first: &Party, second: &Party) -> LinkResult<Option<Link>> {
        Ok(self.0.all().iter().map(snapshot).find(|link| {
            link.party_index(first).is_some()
                && link.party_index(second).is_some()
                && live(link.state)
        }))
    }

    fn list_for(&self, party: &Party) -> LinkResult<Vec<Link>> {
        Ok(self
            .0
            .all()
            .iter()
            .map(snapshot)
            .filter(|link| link.party_index(party).is_some())
            .collect())
    }
}

/// A message id source that counts: ids the outbox draws for its own needs.
#[derive(Default)]
struct Counter(AtomicU64);

impl IdSource for Counter {
    fn next_id(&self, now: u64) -> MessageId {
        let n = self.0.fetch_add(1, Ordering::SeqCst);
        let mut entropy = [0u8; 10];
        entropy[..8].copy_from_slice(&n.to_be_bytes());
        MessageId::new_v7(now, entropy)
    }
}

/// One site of an in-process pair: its party, its end of the link and its exchange stores.
pub struct InProcessSite {
    party: Party,
    first: LinkId,
    held: Arc<Links>,
    links: Arc<SharedLinkStore>,
    signer: SharedSigner,
    lifetimes: Lifetimes,
    region: Option<Arc<MemoryRegionOracle>>,
    /// The sender outbox.
    pub outbox: Arc<MemoryOutbox>,
    /// The receiver inbox (it is also the notice feed).
    pub inbox: Arc<MemoryInbox>,
    /// The input quarantine.
    pub quarantine: Arc<MemoryQuarantine>,
    /// The link audit chain.
    pub audit: Arc<MemoryAuditLog>,
}

impl InProcessSite {
    /// A site that is `party` on `link`, signing as `signer`.
    #[must_use]
    pub fn new(party: Party, link: Link, signer: SharedSigner, lifetimes: Lifetimes) -> Self {
        let first = link.id;
        let held = Arc::new(Links(RwLock::new(vec![Arc::new(RwLock::new(link))])));
        Self {
            party,
            first,
            links: Arc::new(SharedLinkStore(Arc::clone(&held))),
            held,
            signer,
            lifetimes,
            region: None,
            outbox: Arc::new(MemoryOutbox::new()),
            inbox: Arc::new(MemoryInbox::new()),
            quarantine: Arc::new(MemoryQuarantine::new()),
            audit: Arc::new(MemoryAuditLog::new()),
        }
    }

    /// The same site with a region oracle.
    #[must_use]
    pub fn with_region(mut self, oracle: Arc<MemoryRegionOracle>) -> Self {
        self.region = Some(oracle);
        self
    }

    /// Holds another link (a second peer).
    pub fn add_link(&self, link: Link) {
        self.held
            .0
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(Arc::new(RwLock::new(link)));
    }

    /// The id of the first link the site holds.
    #[must_use]
    pub fn id(&self) -> LinkId {
        self.first
    }

    /// The link `id` as the site holds it now.
    #[must_use]
    pub fn current(&self, id: &LinkId) -> Option<Link> {
        self.held.shared(id).map(|shared| snapshot(&shared))
    }

    /// Replaces the link of the same id: a revocation, an expiry or a new grant takes effect for
    /// the sender and the receiver of this site from the next message on. A link the site does
    /// not hold is ignored.
    pub fn replace_link(&self, link: Link) {
        if let Some(shared) = self.held.shared(&link.id) {
            *shared
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = link;
        }
    }

    /// The party the connection of a message from this site authenticates.
    #[must_use]
    pub fn connection(&self) -> TransportParty {
        TransportParty::from_verified_connection(self.party.clone())
    }

    fn oracle(&self) -> Option<&dyn RegionOracle> {
        self.region
            .as_deref()
            .map(|oracle| oracle as &dyn RegionOracle)
    }
}

impl LinkResources for InProcessSite {
    fn party(&self) -> Party {
        self.party.clone()
    }

    fn link(&self, id: &LinkId) -> Option<SharedLink> {
        self.held.shared(id)
    }

    fn links(&self) -> Arc<dyn LinkStore> {
        Arc::clone(&self.links) as Arc<dyn LinkStore>
    }

    fn outbox(&self) -> Arc<dyn OutboxStore> {
        Arc::clone(&self.outbox) as Arc<dyn OutboxStore>
    }

    fn inbox(&self) -> Arc<dyn InboxStore> {
        Arc::clone(&self.inbox) as Arc<dyn InboxStore>
    }

    fn feed(&self) -> Arc<dyn NoticeFeed> {
        Arc::clone(&self.inbox) as Arc<dyn NoticeFeed>
    }

    fn quarantine(&self) -> Arc<dyn InputQuarantine> {
        Arc::clone(&self.quarantine) as Arc<dyn InputQuarantine>
    }

    fn audit(&self) -> Arc<dyn AuditSink> {
        Arc::clone(&self.audit) as Arc<dyn AuditSink>
    }

    fn region(&self) -> Option<Arc<dyn RegionOracle>> {
        self.region
            .as_ref()
            .map(|oracle| Arc::clone(oracle) as Arc<dyn RegionOracle>)
    }

    fn lifetimes(&self) -> Lifetimes {
        self.lifetimes.clone()
    }
}

/// Hands everything `from` has queued (up to `limit` messages) to `to`'s core once, in order, and
/// returns the receipts to `from`, as a carrier does. Returns the messages carried.
///
/// A message the receiver does not take is not retried here: the next call carries it again,
/// because the outbox still holds it.
pub fn carry(
    from: &InProcessSite,
    to: &InProcessSite,
    to_core: &ConversationCore,
    link: &LinkId,
    now_ms: u64,
    limit: usize,
) -> Vec<Message> {
    let ids = Counter::default();
    let outbox = Outbox::new(
        &*from.outbox,
        &*from.audit,
        &*from.signer,
        &ids,
        from.lifetimes.clone(),
    );
    let Some(link) = from.current(link) else {
        return Vec::new();
    };
    let batch = outbox
        .next_batch(&link, now_ms, limit, "system", from.oracle())
        .unwrap_or_default();
    for message in &batch {
        let Ok(outcome) = to_core.receive_message(&link.id, &from.connection(), message) else {
            continue;
        };
        for receipt in &outcome.receipts {
            let _ = outbox.on_receipt_from(&link, &to.connection(), receipt, now_ms);
        }
    }
    batch
}
