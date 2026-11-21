//! What every handler and the driver share: the contracts this tenant holds, the roles it plays,
//! the routes to its peers and the conversation store.

use std::collections::BTreeMap;
use std::sync::Arc;

use spot_bpm_contract::{Problem, ProcessContract, check};
use spot_tenant_link::LinkId;

use crate::audit::GateAudit;
use crate::limits::LinkLimits;
use crate::record::{ConversationStore, WaitFor};
use crate::sender::MessageSender;

/// The contract versions this tenant holds and the role it plays in each.
///
/// A conversation stays on the version it started on, so every version a running conversation
/// uses must stay in the book until it ends.
#[derive(Default, Clone)]
pub struct ContractBook {
    contracts: BTreeMap<(String, u32), Arc<ProcessContract>>,
    plays: BTreeMap<String, String>,
}

impl ContractBook {
    /// An empty book.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a contract version, which this tenant plays as `role`.
    ///
    /// The contract is checked first and a contract with any problem is refused whole, because a
    /// partner may have supplied it.
    ///
    /// # Errors
    /// Every [`Problem`] the contract checker found; also a problem when `role` is not one of the
    /// contract's participants.
    pub fn add(
        &mut self,
        contract: ProcessContract,
        role: impl Into<String>,
    ) -> Result<(), Vec<Problem>> {
        let role = role.into();
        let mut problems = check(&contract);
        if contract.participant(&role).is_none() {
            problems.push(Problem::new(
                spot_bpm_contract::codes::CONFORMANCE_PARTICIPANTS,
                role.clone(),
                format!("this tenant plays role `{role}`, which the contract does not declare"),
            ));
        }
        if !problems.is_empty() {
            return Err(problems);
        }
        self.plays.insert(contract.id.clone(), role);
        self.contracts
            .insert((contract.id.clone(), contract.version), Arc::new(contract));
        Ok(())
    }

    /// The contract with this id and version.
    #[must_use]
    pub fn get(&self, id: &str, version: u32) -> Option<&Arc<ProcessContract>> {
        self.contracts.get(&(id.to_owned(), version))
    }

    /// The highest version held of this contract.
    #[must_use]
    pub fn latest(&self, id: &str) -> Option<&Arc<ProcessContract>> {
        self.contracts
            .iter()
            .filter(|((candidate, _), _)| candidate == id)
            .max_by_key(|((_, version), _)| *version)
            .map(|(_, contract)| contract)
    }

    /// The role this tenant plays in the contract.
    #[must_use]
    pub fn role(&self, id: &str) -> Option<&str> {
        self.plays.get(id).map(String::as_str)
    }
}

/// How to reach each peer role.
pub trait PeerDirectory: Send + Sync {
    /// The link that reaches `role` in `contract`.
    fn route(&self, contract: &str, role: &str) -> Option<LinkId>;

    /// The role the peer on `link` plays in `contract`: what a message arriving on the link is
    /// held to.
    fn peer_role(&self, link: &LinkId, contract: &str) -> Option<String>;

    /// The sender for `link`, when this tenant can send on it.
    fn sender(&self, link: &LinkId) -> Option<Arc<dyn MessageSender>>;
}

struct Route {
    contract: String,
    peer_role: String,
    link: LinkId,
    sender: Option<Arc<dyn MessageSender>>,
}

/// A fixed table of routes, for a tenant whose links are known up front.
#[derive(Default)]
pub struct StaticPeers {
    routes: Vec<Route>,
}

impl StaticPeers {
    /// An empty table.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Says that the peer on `link` plays `peer_role` in `contract`, and how to send to it.
    #[must_use]
    pub fn with_peer(
        mut self,
        contract: impl Into<String>,
        peer_role: impl Into<String>,
        link: LinkId,
        sender: Option<Arc<dyn MessageSender>>,
    ) -> Self {
        self.routes.push(Route {
            contract: contract.into(),
            peer_role: peer_role.into(),
            link,
            sender,
        });
        self
    }
}

impl PeerDirectory for StaticPeers {
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
        self.routes
            .iter()
            .find(|route| &route.link == link)
            .and_then(|route| route.sender.clone())
    }
}

/// The shared state of a conversation runtime.
pub struct Runtime {
    /// The contracts and roles.
    pub book: ContractBook,
    /// The conversation records.
    pub store: Arc<dyn ConversationStore>,
    /// The routes to peers.
    pub peers: Arc<dyn PeerDirectory>,
    /// Where the receiving gate writes its refusals.
    pub audit: Arc<dyn GateAudit>,
    /// How much one link may make this tenant accumulate.
    pub limits: Arc<LinkLimits>,
    /// How far a send must be delivered before its step continues, when the node does not say.
    pub default_wait_for: WaitFor,
}

impl Runtime {
    /// A runtime over a book, a store, a directory and an audit trail; sends continue once
    /// queued.
    ///
    /// The audit trail is not optional: every refusal a gate decides has to be reviewable
    /// afterwards, so a host wires one ([`LinkAudit`](crate::audit::LinkAudit) over the link's
    /// chain, or its own).
    #[must_use]
    pub fn new(
        book: ContractBook,
        store: Arc<dyn ConversationStore>,
        peers: Arc<dyn PeerDirectory>,
        audit: Arc<dyn GateAudit>,
    ) -> Self {
        Self {
            book,
            store,
            peers,
            audit,
            limits: Arc::new(LinkLimits::new()),
            default_wait_for: WaitFor::Queued,
        }
    }

    /// The same runtime with other per-link bounds.
    #[must_use]
    pub fn with_limits(mut self, limits: Arc<LinkLimits>) -> Self {
        self.limits = limits;
        self
    }

    /// The same runtime with another default delivery requirement.
    #[must_use]
    pub fn with_default_wait_for(mut self, wait_for: WaitFor) -> Self {
        self.default_wait_for = wait_for;
        self
    }
}
