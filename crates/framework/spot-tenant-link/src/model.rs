//! The link record, its parties, carrier and states, and the draft a proposal starts from.
//!
//! A link joins exactly two parties (ADR-058 rules 3 and 10). A party is either a platform
//! tenant ([`spot_tenancy::TenantId`]) or an external peer ([`ExternalPeerId`]); the control
//! plane records that the link exists, never the link's data.
//!
//! Time is always an argument: every instant is Unix milliseconds supplied by the caller, so the
//! boundary tests stay exact and no function reads a clock.

use std::fmt;

use serde::{Deserialize, Serialize};
use spot_tenancy::TenantId;

use crate::error::{Error, Result};
use crate::grant::{Direction, Grant, Offer};
use crate::ids::{ExternalPeerId, LinkId, Topic};
use crate::region::RegionOracle;

/// One side of a link.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Party {
    /// A platform tenant.
    Tenant(TenantId),
    /// A peer that is not a platform tenant (ADR-058 rule 12).
    External(ExternalPeerId),
}

impl Party {
    /// Whether this party is a platform tenant.
    #[must_use]
    pub fn is_tenant(&self) -> bool {
        matches!(self, Self::Tenant(_))
    }

    /// A stable text form: `tenant:<uuid>` or `external:<id>`.
    #[must_use]
    pub fn canonical(&self) -> String {
        match self {
            Self::Tenant(id) => format!("tenant:{id}"),
            Self::External(id) => format!("external:{id}"),
        }
    }

    pub(crate) fn encode(&self, out: &mut Vec<u8>) {
        crate::util::put_field(out, self.canonical().as_bytes());
    }
}

impl fmt::Display for Party {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.canonical())
    }
}

impl fmt::Debug for Party {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Party({})", self.canonical())
    }
}

impl TryFrom<String> for Party {
    type Error = Error;

    fn try_from(value: String) -> Result<Self> {
        if let Some(rest) = value.strip_prefix("tenant:") {
            let uuid = uuid::Uuid::parse_str(rest).map_err(|_| Error::InvalidParty {
                reason: "a tenant party is tenant:<uuid>",
            })?;
            return Ok(Self::Tenant(TenantId::from_uuid(uuid).map_err(|_| {
                Error::InvalidParty {
                    reason: "a tenant party names a UUIDv7",
                }
            })?));
        }
        if let Some(rest) = value.strip_prefix("external:") {
            return Ok(Self::External(ExternalPeerId::new(rest)?));
        }
        Err(Error::InvalidParty {
            reason: "a party is tenant:<uuid> or external:<id>",
        })
    }
}

impl From<Party> for String {
    fn from(value: Party) -> Self {
        value.canonical()
    }
}

/// How bytes move between the two sides (ADR-058 rule 11).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Carrier {
    /// Each side publishes an inbound endpoint; the sender dials it over mutual TLS.
    Direct,
    /// The side that cannot accept inbound connections fetches from a mailbox.
    Pull,
    /// The peer is not a platform tenant; a connector or signed webhook carries bytes.
    External,
}

impl Carrier {
    /// The carrier as metadata text.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Pull => "pull",
            Self::External => "external",
        }
    }
}

impl fmt::Display for Carrier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The lifecycle of a link: `Proposed -> AcceptedByPeer -> Active | Revoked | Expired`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkState {
    /// One side proposed; the other has not accepted.
    Proposed,
    /// The peer accepted; keys and activation follow.
    AcceptedByPeer,
    /// Both consents, both keys and the agreed grants are in place.
    Active,
    /// Either side revoked it; no new exchange is allowed.
    Revoked,
    /// Its expiry passed; no new exchange is allowed.
    Expired,
}

impl LinkState {
    /// The state as metadata text.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Proposed => "proposed",
            Self::AcceptedByPeer => "accepted_by_peer",
            Self::Active => "active",
            Self::Revoked => "revoked",
            Self::Expired => "expired",
        }
    }

    /// Whether the state machine allows the move.
    #[must_use]
    pub fn can_become(self, next: Self) -> bool {
        match self {
            Self::Proposed => matches!(next, Self::AcceptedByPeer | Self::Revoked),
            Self::AcceptedByPeer => matches!(next, Self::Active | Self::Revoked),
            Self::Active => matches!(next, Self::Revoked | Self::Expired),
            Self::Revoked | Self::Expired => false,
        }
    }
}

impl fmt::Display for LinkState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a link was revoked.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RevokeReason {
    /// The agreed relationship ended.
    TermsEnded,
    /// A security concern on either side.
    SecurityConcern,
    /// A new link replaces this one.
    Superseded,
}

impl RevokeReason {
    /// The reason as metadata text.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TermsEnded => "terms_ended",
            Self::SecurityConcern => "security_concern",
            Self::Superseded => "superseded",
        }
    }
}

/// Who revoked a link, when and why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Revocation {
    /// The party that revoked it.
    pub by: Party,
    /// Why.
    pub reason: RevokeReason,
    /// Unix milliseconds.
    pub at: u64,
}

/// A tenant link.
///
/// `local` names the side that holds this copy. The record is the same object on both sides
/// except for `local`, the direction-relative grants and the two fields each side enforces for
/// itself (`require_peer_region`, `notify`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Link {
    /// The link id.
    pub id: LinkId,
    /// The two parties, in canonical (`Ord`) order so both sides hold the same pair.
    pub parties: [Party; 2],
    /// The side that holds this copy.
    pub local: Party,
    /// How bytes move.
    pub carrier: Carrier,
    /// The state.
    pub state: LinkState,
    /// Unix milliseconds the proposal was created.
    pub created: u64,
    /// Each party's identity key, once exchanged.
    pub peer_keys: [Option<crate::keys::PeerKey>; 2],
    /// The agreed grants, read from `local`'s side.
    pub grants: Vec<Grant>,
    /// The consents recorded so far; activation needs two from distinct parties and actors.
    pub consents: Vec<crate::consent::Consent>,
    /// Why and by whom it was revoked, if it was.
    pub revocation: Option<Revocation>,
    /// An optional link-wide expiry, Unix milliseconds.
    pub expires: Option<u64>,
    /// The store's optimistic version; 0 before it is stored.
    pub version: u64,
}

impl Link {
    /// The side that holds this copy.
    #[must_use]
    pub fn local(&self) -> &Party {
        &self.local
    }

    /// The other side.
    #[must_use]
    pub fn peer(&self) -> Option<&Party> {
        self.parties.iter().find(|party| **party != self.local)
    }

    /// The index of `party` in `parties`.
    #[must_use]
    pub fn party_index(&self, party: &Party) -> Option<usize> {
        self.parties.iter().position(|candidate| candidate == party)
    }

    /// The state at `now`, taking the link-wide expiry into account.
    #[must_use]
    pub fn state_at(&self, now: u64) -> LinkState {
        if self.state == LinkState::Revoked {
            return LinkState::Revoked;
        }
        if self.expires.is_some_and(|expires| expires <= now) {
            return LinkState::Expired;
        }
        if self.state == LinkState::Active
            && !self.grants.is_empty()
            && self.grants.iter().all(|grant| grant.expires <= now)
        {
            return LinkState::Expired;
        }
        self.state
    }

    /// The grant for `topic`, if any.
    #[must_use]
    pub fn grant(&self, topic: &Topic) -> Option<&Grant> {
        self.grants.iter().find(|grant| &grant.topic == topic)
    }

    /// The single check every sender and receiver calls when it holds no region oracle: may
    /// this side exchange `topic` in `direction` at `now`?
    ///
    /// Revocation is effective immediately: a revoked link refuses every topic from the instant
    /// the state changed. An expiry is effective the same way. A topic with no grant, a grant
    /// past its expiry and a direction the grant does not cover are refused with the specific
    /// reason.
    ///
    /// A grant that carries a peer-region requirement (ADR-058 rule 6) is **refused** here with
    /// [`Error::PeerRegionMismatch`] (found `unknown`): without an oracle the peer's region is
    /// not known, and unknown is never assumed to pass. A caller that has an oracle uses
    /// [`Link::allows_with_region`].
    ///
    /// # Errors
    /// [`Error::Revoked`], [`Error::LinkExpired`], [`Error::NotActive`],
    /// [`Error::TopicNotGranted`], [`Error::GrantExpired`], [`Error::WrongDirection`] or
    /// [`Error::PeerRegionMismatch`].
    pub fn allows(&self, topic: &Topic, direction: Direction, now: u64) -> Result<&Grant> {
        let grant = self.allows_without_region_check(topic, direction, now)?;
        if let Some(required) = &grant.require_peer_region {
            return Err(Error::PeerRegionMismatch {
                required: required.to_string(),
                found: "unknown".to_owned(),
            });
        }
        Ok(grant)
    }

    /// Everything [`Link::allows`] checks except the grant's peer-region requirement. Only
    /// [`Link::allows_with_region`] calls it, and it adds the region check itself; it is not
    /// exported, so no caller can skip the requirement by choosing the cheaper check.
    pub(crate) fn allows_without_region_check(
        &self,
        topic: &Topic,
        direction: Direction,
        now: u64,
    ) -> Result<&Grant> {
        match self.state_at(now) {
            LinkState::Revoked => {
                return Err(Error::Revoked {
                    link: self.id.to_string(),
                });
            }
            LinkState::Expired => {
                return Err(Error::LinkExpired {
                    link: self.id.to_string(),
                });
            }
            LinkState::Active => {}
            state => {
                return Err(Error::NotActive {
                    link: self.id.to_string(),
                    state,
                });
            }
        }
        let grant = self.grant(topic).ok_or_else(|| Error::TopicNotGranted {
            link: self.id.to_string(),
            topic: topic.as_str().to_owned(),
        })?;
        if grant.expires <= now {
            return Err(Error::GrantExpired {
                link: self.id.to_string(),
                topic: topic.as_str().to_owned(),
            });
        }
        if !grant.direction.covers(direction) {
            return Err(Error::WrongDirection {
                link: self.id.to_string(),
                topic: topic.as_str().to_owned(),
            });
        }
        Ok(grant)
    }

    /// [`Link::allows`] plus the grant's peer-region requirement (ADR-058 rule 6).
    ///
    /// # Errors
    /// Everything [`Link::allows`] refuses, plus [`Error::RegionRefused`] and
    /// [`Error::PeerRegionMismatch`].
    pub fn allows_with_region(
        &self,
        topic: &Topic,
        direction: Direction,
        now: u64,
        oracle: &dyn RegionOracle,
    ) -> Result<&Grant> {
        let grant = self.allows_without_region_check(topic, direction, now)?;
        let peer = self.peer().ok_or(Error::InvalidParty {
            reason: "the link's local side is not one of its parties",
        })?;
        grant.check_peer_region(peer, oracle)?;
        Ok(grant)
    }

    /// Checks the record's invariants.
    ///
    /// # Errors
    /// A malformed party pair, consent, key slot, grant or expiry.
    pub fn validate(&self) -> Result<()> {
        if self.parties[0] == self.parties[1] {
            return Err(Error::InvalidParty {
                reason: "a link joins two different parties",
            });
        }
        if self.parties[0] > self.parties[1] {
            return Err(Error::InvalidParty {
                reason: "the parties are kept in canonical order",
            });
        }
        if self.party_index(&self.local).is_none() {
            return Err(Error::InvalidParty {
                reason: "the local side is one of the link's parties",
            });
        }
        if self.expires.is_some_and(|expires| expires <= self.created) {
            return Err(Error::InvalidExpiry {
                reason: "a link expires after it was created",
            });
        }
        let mut seen_topics: Vec<&Topic> = Vec::new();
        for grant in &self.grants {
            grant.validate(self.created)?;
            if self.carrier == Carrier::External && grant.kind == crate::grant::TopicKind::Record {
                return Err(Error::ExternalRecordTopic {
                    topic: grant.topic.as_str().to_owned(),
                });
            }
            if seen_topics.contains(&&grant.topic) {
                return Err(Error::InvalidOffer {
                    reason: "a link lists a topic only once",
                });
            }
            seen_topics.push(&grant.topic);
        }
        let mut parties = Vec::new();
        let mut actors = Vec::new();
        for consent in &self.consents {
            if self.party_index(&consent.party).is_none() {
                return Err(Error::ConsentInvalid {
                    reason: "a consent names one of the link's parties",
                });
            }
            if parties.contains(&&consent.party) || actors.contains(&&consent.actor) {
                return Err(Error::ConsentDuplicate {
                    actor: consent.actor.clone(),
                });
            }
            parties.push(&consent.party);
            actors.push(&consent.actor);
        }
        if self.consents.len() > 2 {
            return Err(Error::ConsentInvalid {
                reason: "a link records one consent per party",
            });
        }
        Ok(())
    }

    /// Revokes the link from `by` at `at`. The returned record is a new value; the caller stores
    /// it and audits the change.
    ///
    /// # Errors
    /// [`Error::InvalidTransition`] when the link is already revoked or expired,
    /// [`Error::InvalidParty`] when `by` is not one of its parties.
    pub fn revoked_by(&self, by: &Party, reason: RevokeReason, at: u64) -> Result<Self> {
        if !self.parties.contains(by) {
            return Err(Error::InvalidParty {
                reason: "only a party of the link can revoke it",
            });
        }
        let to = LinkState::Revoked;
        if !self.state.can_become(to) {
            return Err(Error::InvalidTransition {
                link: self.id.to_string(),
                from: self.state,
                to,
            });
        }
        let mut next = self.clone();
        next.state = to;
        next.revocation = Some(Revocation {
            by: by.clone(),
            reason,
            at,
        });
        Ok(next)
    }

    /// Moves an expired link to `Expired`; keeps its grants and history for the record.
    ///
    /// # Errors
    /// [`Error::InvalidTransition`] when the link is already terminal,
    /// [`Error::InvalidExpiry`] when neither the link nor all of its grants have expired at `at`.
    pub fn expired_at(&self, at: u64) -> Result<Self> {
        let to = LinkState::Expired;
        if !self.state.can_become(to) {
            return Err(Error::InvalidTransition {
                link: self.id.to_string(),
                from: self.state,
                to,
            });
        }
        let expired = self.expires.is_some_and(|expires| expires <= at)
            || (!self.grants.is_empty() && self.grants.iter().all(|grant| grant.expires <= at));
        if !expired {
            return Err(Error::InvalidExpiry {
                reason: "the link has not reached its expiry",
            });
        }
        let mut next = self.clone();
        next.state = to;
        Ok(next)
    }
}

/// What one side proposes to the other, before any state exists.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkDraft {
    /// The id the proposer chose.
    pub id: LinkId,
    /// The proposing side.
    pub local: Party,
    /// The invited side.
    pub peer: Party,
    /// How bytes will move.
    pub carrier: Carrier,
    /// Unix milliseconds.
    pub created: u64,
    /// An optional link-wide expiry, Unix milliseconds.
    pub expires: Option<u64>,
    /// The topics the proposer offers; `offer.side` must equal `local`.
    pub offer: Offer,
}

impl LinkDraft {
    /// A draft, checked against itself.
    ///
    /// # Errors
    /// [`Error::InvalidParty`] when the two sides are the same,
    /// [`Error::InvalidOffer`] when the offer was made by another side,
    /// [`Error::InvalidExpiry`] when an expiry is not in the future of `created`, or a malformed
    /// grant.
    pub fn new(
        id: LinkId,
        local: Party,
        peer: Party,
        carrier: Carrier,
        created: u64,
        expires: Option<u64>,
        offer: Offer,
    ) -> Result<Self> {
        let draft = Self {
            id,
            local,
            peer,
            carrier,
            created,
            expires,
            offer,
        };
        draft.validate()?;
        Ok(draft)
    }

    /// The two parties in canonical order.
    #[must_use]
    pub fn parties(&self) -> [Party; 2] {
        if self.local <= self.peer {
            [self.local.clone(), self.peer.clone()]
        } else {
            [self.peer.clone(), self.local.clone()]
        }
    }

    /// The draft's invariants.
    ///
    /// # Errors
    /// As [`LinkDraft::new`].
    pub fn validate(&self) -> Result<()> {
        if self.local == self.peer {
            return Err(Error::InvalidParty {
                reason: "a link joins two different parties",
            });
        }
        if self.offer.side != self.local {
            return Err(Error::InvalidOffer {
                reason: "the offer is made by the proposing side",
            });
        }
        if self.expires.is_some_and(|expires| expires <= self.created) {
            return Err(Error::InvalidExpiry {
                reason: "a link expires after it was created",
            });
        }
        self.offer.validate(self.created, self.carrier)
    }
}
