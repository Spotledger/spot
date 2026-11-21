//! The handshake as a message-passing state machine.
//!
//! ```text
//! proposer                                   peer
//!   propose        (revision 0, consent A)  ->
//!                  <-  accept (revision 1, offer B, consent B) | reject
//!   key_exchange   (revision 2)             ->
//!                  <-  key_exchange (revision 3)
//!   activate       (revision 4, grants digest) ->
//! ```
//!
//! Every message is signed by its sender's identity key and names the digest of the message
//! before it, so the five messages form one chain: a message replayed at a later step, a message
//! with a byte changed, or a message signed by an unexpected key is refused ([`Error::Replayed`],
//! [`Error::TranscriptMismatch`], [`Error::WrongSigner`]). A nonce is recorded once per link in
//! the durable [`NonceLog`], so a replay is refused even after a restart.
//!
//! Both sides consent: the proposal carries the proposer's [`Consent`] and the acceptance the
//! peer's, and they must come from different parties and different actors. Activation also needs
//! both identity keys, at least one agreed grant, and the same [`grants_digest`] on both sides.
//!
//! The state machine never touches the network or a clock: a caller passes each instant, carries
//! messages itself and stores the finished [`Link`]. It writes an audit entry for every state
//! change and every refusal, on the side that observes it.
//!
//! # What a message proves
//!
//! A signature proves that its sender holds a private key, not that the key belongs to the party
//! the message names: anyone who can reach the other tenant could send the first message with a
//! key of their own. The first key a party sends is therefore checked against the peer keys the
//! caller already trusts ([`PeerKeyDirectory`]); a different key is refused, and a party with no
//! pinned key needs an administrator's explicit [`Trust::FirstUseConfirmed`] fingerprint. Later
//! messages keep using the key the link recorded.
//!
//! Every consent is re-checked when it is used (shape, [`ConsentPolicy`] age and the
//! [`StepUpGate`]), and every nonce goes through a [`NonceLog`] before the send or receive acts,
//! so a rebuilt handshake cannot be made to accept a captured message.

use std::collections::BTreeSet;

use crate::audit::{AuditSink, LinkEvent, LinkEventKind, record};
use crate::consent::{Consent, ConsentPolicy, StepUpGate, require_distinct};
use crate::error::{Error, Result};
use crate::grant::{Offer, grants_digest, grants_match};
use crate::ids::{Digest, LinkId, Nonce};
use crate::keys::{IdentitySigner, PeerKey};
use crate::model::{Link, LinkDraft, LinkState, Party, Revocation, RevokeReason};
use crate::nonce::{NonceClaim, NonceLog};
use crate::peers::{PeerKeyDirectory, Trust, check_peer_key};
use crate::wire::{
    AcceptBody, ActivateBody, HandshakeBody, HandshakeMessage, KeyExchangeBody, ProposalBody,
    RejectBody, RejectReason,
};

/// The ports and the policy one handshake runs under, besides its signer and audit sink.
///
/// All five are the caller's: the trusted peer keys, how a first-use key may be confirmed, the
/// maximum age of a used consent, the tenant's step-up gate and the durable nonce log.
pub struct HandshakePolicy<'a> {
    /// Where this side's pinned peer identity keys come from.
    pub keys: &'a dyn PeerKeyDirectory,
    /// How a peer with no pinned key may be trusted, if at all.
    pub trust: Trust,
    /// How old a consent may be when it is used.
    pub consent: ConsentPolicy,
    /// The tenant's step-up gate, applied again to every consent that is used.
    pub gate: &'a dyn StepUpGate,
    /// The durable record of the nonces this link's handshake has used.
    pub nonces: &'a dyn NonceLog,
}

/// The state a handshake is rebuilt from after a restart.
///
/// The stored [`Link`] carries the parties, consents and keys; the transcript tail (the digest of
/// the last message and the next revision) is the host's to persist beside it, because it is the
/// chain the signatures cover. The offer fields are the draft's for a proposal and the recorded
/// offers after an acceptance. The nonces are **not** here: they live in the [`NonceLog`].
pub struct Resume {
    /// The link as it was stored.
    pub link: Link,
    /// This side's offer.
    pub local_offer: Offer,
    /// The peer's offer, once received.
    pub remote_offer: Option<Offer>,
    /// The digest of the last message exchanged.
    pub head: Digest,
    /// The revision this side expects next.
    pub next_revision: u64,
}

/// What a received message did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Received {
    /// The peer accepted; this side sends its identity key next.
    Accepted,
    /// The peer published its identity key.
    KeyExchanged,
    /// The peer activated the link.
    Activated,
    /// The peer rejected the link.
    Rejected,
}

/// One side's handshake.
///
/// It borrows the identity signer, the audit sink and the policy's ports for its lifetime;
/// no secret and no private key is reachable through it. Build both sides with
/// [`Handshake::propose`] and [`Handshake::from_proposal`], rebuild one after a restart with
/// [`Handshake::resume`], then alternate `receive` with `accept`, `key_exchange`, `activate` or
/// `reject`.
pub struct Handshake<'a> {
    signer: &'a dyn IdentitySigner,
    audit: &'a dyn AuditSink,
    policy: &'a HandshakePolicy<'a>,
    link: Link,
    local: Party,
    peer: Party,
    proposer: Party,
    local_offer: Offer,
    remote_offer: Option<Offer>,
    next_revision: u64,
    head: Option<Digest>,
    /// A cache of the nonces this process has seen; the durable record is [`NonceLog`].
    used: BTreeSet<Nonce>,
}

impl<'a> Handshake<'a> {
    /// Starts a link: builds revision 0 from `draft` and the proposer's `consent`.
    ///
    /// The consent is checked as a **use**, not only as a record: its shape, its age against
    /// `policy.consent` and its step-up proof through `policy.gate`. A refusal is audited on this
    /// side before it is returned.
    ///
    /// # Errors
    /// A malformed draft, a consent from another party, a stale or weak consent, an instant
    /// before the draft was created, a nonce already recorded for this link, or an audit failure.
    pub fn propose(
        signer: &'a dyn IdentitySigner,
        audit: &'a dyn AuditSink,
        policy: &'a HandshakePolicy<'a>,
        draft: LinkDraft,
        consent: Consent,
        at: u64,
        nonce: Nonce,
    ) -> Result<(Self, HandshakeMessage)> {
        draft.validate()?;
        let refuse = |error| refusal(audit, draft.id, at, 0, error);
        if consent.party != draft.local {
            return Err(refuse(Error::ConsentInvalid {
                reason: "the proposing side consents for itself",
            }));
        }
        if consent.at < draft.created {
            return Err(refuse(Error::ConsentInvalid {
                reason: "a consent is dated after the draft it approves",
            }));
        }
        consent
            .check_use(at, policy.consent, policy.gate)
            .map_err(refuse)?;
        if signer.party() != draft.local {
            return Err(Error::WrongSigner {
                expected: draft.local.canonical(),
                found: signer.party().canonical(),
            });
        }
        let parties = draft.parties();
        let local = draft.local.clone();
        let peer = draft.peer.clone();
        let index = if parties[0] == local { 0 } else { 1 };
        let mut peer_keys = [None, None];
        peer_keys[index] = Some(signer.public_key());
        let link = Link {
            id: draft.id,
            parties,
            local: local.clone(),
            carrier: draft.carrier,
            state: LinkState::Proposed,
            created: draft.created,
            peer_keys,
            grants: Vec::new(),
            consents: vec![consent.clone()],
            revocation: None,
            expires: draft.expires,
            version: 0,
        };
        link.validate()?;
        if !policy.nonces.record(&link.id, &nonce)? {
            return Err(refuse(Error::Replayed { revision: 0 }));
        }
        let event = LinkEvent {
            link: link.id,
            kind: LinkEventKind::Proposed,
            actor: consent.actor.clone(),
            at: consent.at,
            revision: 0,
            detail: None,
        };
        record(audit, &event, "propose a link")?;
        let message = HandshakeMessage::signed(
            signer,
            0,
            None,
            nonce,
            HandshakeBody::Propose(ProposalBody {
                draft: draft.clone(),
                consent,
            }),
        );
        let mut handshake = Self {
            signer,
            audit,
            policy,
            link,
            local,
            peer,
            proposer: draft.local,
            local_offer: draft.offer,
            remote_offer: None,
            next_revision: 1,
            head: Some(message.digest()),
            used: BTreeSet::new(),
        };
        handshake.used.insert(nonce);
        Ok((handshake, message))
    }

    /// The invited side's view of a proposal: verifies message 0, pins the proposer's key and
    /// records it.
    ///
    /// The key in the message is checked against `policy.keys` for the proposing party; a
    /// different pin, or a first-use key the caller did not confirm, is refused and audited.
    ///
    /// # Errors
    /// A bad signature, a replayed or out-of-order message, a malformed draft, a consent from
    /// another party, a stale or weak consent, a peer key that is not trusted, or an audit
    /// failure.
    pub fn from_proposal(
        signer: &'a dyn IdentitySigner,
        audit: &'a dyn AuditSink,
        policy: &'a HandshakePolicy<'a>,
        message: &HandshakeMessage,
        at: u64,
    ) -> Result<Self> {
        Self::from_proposal_claimed(signer, audit, policy, message, at, None)
    }

    /// [`from_proposal`](Self::from_proposal) for a message whose nonce the caller already claimed
    /// (`claim`), so the nonce is recorded through [`NonceLog::record_claimed`] instead of a
    /// second `record` that would read the caller's own claim as a replay. `None` records
    /// normally.
    ///
    /// # Errors
    /// As [`from_proposal`](Self::from_proposal); a claim that is spent, released or for another
    /// nonce or link is refused as a replay.
    pub fn from_proposal_claimed(
        signer: &'a dyn IdentitySigner,
        audit: &'a dyn AuditSink,
        policy: &'a HandshakePolicy<'a>,
        message: &HandshakeMessage,
        at: u64,
        claim: Option<&NonceClaim>,
    ) -> Result<Self> {
        message.verify()?;
        if message.revision != 0 {
            return Err(Error::OutOfOrder {
                expected: 0,
                found: message.revision,
            });
        }
        if message.previous.is_some() {
            return Err(Error::TranscriptMismatch);
        }
        let HandshakeBody::Propose(body) = &message.body else {
            return Err(Error::WrongStep {
                expected: "propose",
                found: message.body.name(),
            });
        };
        body.draft.validate()?;
        let refuse = |error| refusal(audit, body.draft.id, at, 0, error);
        if body.draft.peer != signer.party() {
            return Err(Error::WrongSigner {
                expected: body.draft.peer.canonical(),
                found: signer.party().canonical(),
            });
        }
        if body.consent.party != body.draft.local {
            return Err(refuse(Error::ConsentInvalid {
                reason: "the proposing side consents for itself",
            }));
        }
        if body.consent.at < body.draft.created {
            return Err(refuse(Error::ConsentInvalid {
                reason: "a consent is dated after the draft it approves",
            }));
        }
        body.consent
            .check_use(at, policy.consent, policy.gate)
            .map_err(refuse)?;
        // The proposer's key is the message's signer. Pin it before it can vouch for itself.
        let peer_key = message.signer;
        trust_peer_key(policy, &body.draft.local, peer_key).map_err(refuse)?;
        let parties = body.draft.parties();
        let local = body.draft.peer.clone();
        let peer = body.draft.local.clone();
        let index = if parties[0] == peer { 0 } else { 1 };
        let mut peer_keys = [None, None];
        peer_keys[index] = Some(peer_key);
        let link = Link {
            id: body.draft.id,
            parties,
            local: local.clone(),
            carrier: body.draft.carrier,
            state: LinkState::Proposed,
            created: body.draft.created,
            peer_keys,
            grants: Vec::new(),
            consents: vec![body.consent.clone()],
            revocation: None,
            expires: body.draft.expires,
            version: 0,
        };
        link.validate()?;
        if !record_nonce(policy.nonces, &link.id, &message.nonce, claim)? {
            return Err(refuse(Error::Replayed {
                revision: message.revision,
            }));
        }
        let event = LinkEvent {
            link: link.id,
            kind: LinkEventKind::Proposed,
            actor: body.consent.actor.clone(),
            at,
            revision: 0,
            detail: None,
        };
        record(audit, &event, "receive a link proposal")?;
        let mut handshake = Self {
            signer,
            audit,
            policy,
            link,
            local,
            peer: peer.clone(),
            proposer: peer,
            local_offer: body.draft.offer.clone(),
            remote_offer: Some(body.draft.offer.clone()),
            next_revision: 1,
            head: Some(message.digest()),
            used: BTreeSet::new(),
        };
        handshake.used.insert(message.nonce);
        Ok(handshake)
    }

    /// The invited side accepts: revision 1, carrying this side's offer and consent.
    ///
    /// The agreed grants are the intersection of the two offers; a topic only one side listed
    /// does not cross, and a shared topic both sides describe differently is an error.
    ///
    /// Both consents are checked as uses at `at`: the acceptance's own, and the proposal's that
    /// this link already carries. A stale, weak or non-administrator consent is refused and
    /// audited.
    ///
    /// # Errors
    /// Not this side's turn, a malformed offer, a consent from another party, a stale or weak
    /// consent, consents that are not distinct, offers that disagree, a nonce already recorded
    /// for this link, or an audit failure.
    pub fn accept(
        &mut self,
        offer: Offer,
        consent: Consent,
        at: u64,
        nonce: Nonce,
    ) -> Result<HandshakeMessage> {
        self.require_local_turn("accept")?;
        if self.next_revision != 1 {
            return Err(Error::WrongStep {
                expected: "accept or reject",
                found: "accept",
            });
        }
        if offer.side != self.local {
            return Err(Error::InvalidOffer {
                reason: "an offer is made by the side that sends it",
            });
        }
        offer.validate(self.link.created, self.link.carrier)?;
        if consent.party != self.local {
            return Err(refusal(
                self.audit,
                self.link.id,
                at,
                1,
                Error::ConsentInvalid {
                    reason: "the accepting side consents for itself",
                },
            ));
        }
        if consent.at < self.link.created {
            return Err(refusal(
                self.audit,
                self.link.id,
                at,
                1,
                Error::ConsentInvalid {
                    reason: "a consent is dated after the draft it approves",
                },
            ));
        }
        consent
            .check_use(at, self.policy.consent, self.policy.gate)
            .map_err(|error| refusal(self.audit, self.link.id, at, 1, error))?;
        let Some(proposal_consent) = self.link.consents.first() else {
            return Err(Error::ConsentRequired {
                link: self.link.id.to_string(),
            });
        };
        // The proposal's consent is used again here: it approved the terms this side is now
        // accepting, so it must still be fresh and strong.
        proposal_consent
            .check_use(at, self.policy.consent, self.policy.gate)
            .map_err(|error| refusal(self.audit, self.link.id, at, 1, error))?;
        require_distinct(proposal_consent, &consent)
            .map_err(|error| refusal(self.audit, self.link.id, at, 1, error))?;
        let Some(remote) = self.remote_offer.as_ref() else {
            return Err(Error::WrongStep {
                expected: "propose",
                found: "accept",
            });
        };
        let grants = grants_match(&offer, remote)?;
        if !self.policy.nonces.record(&self.link.id, &nonce)? {
            return Err(refusal(
                self.audit,
                self.link.id,
                at,
                1,
                Error::Replayed { revision: 1 },
            ));
        }
        let event = LinkEvent {
            link: self.link.id,
            kind: LinkEventKind::Accepted,
            actor: consent.actor.clone(),
            at: consent.at,
            revision: 1,
            detail: None,
        };
        record(self.audit, &event, "accept a link")?;
        self.link.grants = grants;
        self.link.consents.push(consent.clone());
        self.link.state = LinkState::AcceptedByPeer;
        self.local_offer = offer.clone();
        self.used.insert(nonce);
        let message = HandshakeMessage::signed(
            self.signer,
            1,
            self.head,
            nonce,
            HandshakeBody::Accept(AcceptBody {
                link: self.link.id,
                offer,
                consent,
            }),
        );
        self.advance(&message);
        Ok(message)
    }

    /// Publishes this side's identity key (revision 2 for the proposer, 3 for the peer).
    ///
    /// # Errors
    /// Not this side's turn, the link is not accepted yet, or an audit failure.
    pub fn key_exchange(&mut self, at: u64, nonce: Nonce) -> Result<HandshakeMessage> {
        self.require_local_turn("key_exchange")?;
        if !matches!(self.next_revision, 2 | 3) {
            return Err(Error::WrongStep {
                expected: "key_exchange or reject",
                found: "key_exchange",
            });
        }
        if self.link.state != LinkState::AcceptedByPeer {
            return Err(Error::InvalidTransition {
                link: self.link.id.to_string(),
                from: self.link.state,
                to: LinkState::Active,
            });
        }
        let index = self
            .link
            .party_index(&self.local)
            .ok_or(Error::InvalidParty {
                reason: "the local side is one of the link's parties",
            })?;
        let key = self.signer.public_key();
        if !self.policy.nonces.record(&self.link.id, &nonce)? {
            return Err(refusal(
                self.audit,
                self.link.id,
                at,
                self.next_revision,
                Error::Replayed {
                    revision: self.next_revision,
                },
            ));
        }
        let event = LinkEvent {
            link: self.link.id,
            kind: LinkEventKind::KeyExchanged,
            actor: "system".to_owned(),
            at,
            revision: self.next_revision,
            detail: None,
        };
        record(self.audit, &event, "exchange a link identity key")?;
        self.link.peer_keys[index] = Some(key);
        self.used.insert(nonce);
        let message = HandshakeMessage::signed(
            self.signer,
            self.next_revision,
            self.head,
            nonce,
            HandshakeBody::KeyExchange(KeyExchangeBody {
                link: self.link.id,
                key,
            }),
        );
        self.advance(&message);
        Ok(message)
    }

    /// The proposer activates: the final message, revision 4.
    ///
    /// # Errors
    /// Not the proposer's turn, missing consents, missing keys, no agreed grant, or an audit
    /// failure.
    pub fn activate(&mut self, at: u64, nonce: Nonce) -> Result<HandshakeMessage> {
        self.require_local_turn("activate")?;
        if self.next_revision != 4 {
            return Err(Error::WrongStep {
                expected: "activate or reject",
                found: "activate",
            });
        }
        self.require_ready_to_activate()?;
        let grants = grants_digest(&self.link.grants, &self.local, &self.link.parties);
        if !self.policy.nonces.record(&self.link.id, &nonce)? {
            return Err(refusal(
                self.audit,
                self.link.id,
                at,
                4,
                Error::Replayed { revision: 4 },
            ));
        }
        let event = LinkEvent {
            link: self.link.id,
            kind: LinkEventKind::Activated,
            actor: "system".to_owned(),
            at,
            revision: 4,
            detail: None,
        };
        record(self.audit, &event, "activate a link")?;
        self.link.state = LinkState::Active;
        self.used.insert(nonce);
        let message = HandshakeMessage::signed(
            self.signer,
            4,
            self.head,
            nonce,
            HandshakeBody::Activate(ActivateBody {
                link: self.link.id,
                grants,
            }),
        );
        self.advance(&message);
        Ok(message)
    }

    /// Rejects the link at this side's turn (revision 1, or a later turn after acceptance).
    ///
    /// # Errors
    /// Not this side's turn, a finished link, or an audit failure.
    pub fn reject(
        &mut self,
        reason: RejectReason,
        at: u64,
        nonce: Nonce,
    ) -> Result<HandshakeMessage> {
        self.require_local_turn("reject")?;
        if !(1..=4).contains(&self.next_revision) {
            return Err(Error::WrongStep {
                expected: "a link still in handshake",
                found: "reject",
            });
        }
        if !self.policy.nonces.record(&self.link.id, &nonce)? {
            return Err(refusal(
                self.audit,
                self.link.id,
                at,
                self.next_revision,
                Error::Replayed {
                    revision: self.next_revision,
                },
            ));
        }
        let event = LinkEvent {
            link: self.link.id,
            kind: LinkEventKind::Rejected,
            actor: "system".to_owned(),
            at,
            revision: self.next_revision,
            detail: Some(reason.as_str()),
        };
        record(self.audit, &event, "reject a link")?;
        self.link.state = LinkState::Revoked;
        self.link.revocation = Some(Revocation {
            by: self.local.clone(),
            reason: RevokeReason::TermsEnded,
            at,
        });
        self.used.insert(nonce);
        let message = HandshakeMessage::signed(
            self.signer,
            self.next_revision,
            self.head,
            nonce,
            HandshakeBody::Reject(RejectBody {
                link: self.link.id,
                reason,
            }),
        );
        self.advance(&message);
        Ok(message)
    }

    /// Receives the peer's next message and applies it.
    ///
    /// # Errors
    /// A bad signature, a message from the wrong key, a replay or an out-of-order message, a
    /// broken transcript, a message for another link, an unexpected step, a consent problem, an
    /// offer conflict, a grants digest mismatch, or an audit failure. Every refusal is itself
    /// audited on this side.
    pub fn receive(&mut self, message: &HandshakeMessage, at: u64) -> Result<Received> {
        self.receive_claimed(message, at, None)
    }

    /// [`receive`](Self::receive) for a message whose nonce the caller already claimed (`claim`),
    /// so the nonce is recorded through [`NonceLog::record_claimed`] instead of a second `record`
    /// that would read the caller's own claim as a replay. `None` records normally. A claim that
    /// is spent, released or for another nonce or link is refused as a replay.
    ///
    /// # Errors
    /// As [`receive`](Self::receive).
    pub fn receive_claimed(
        &mut self,
        message: &HandshakeMessage,
        at: u64,
        claim: Option<&NonceClaim>,
    ) -> Result<Received> {
        match self.receive_inner(message, at, claim) {
            Ok(received) => Ok(received),
            Err(error) => {
                let event = LinkEvent {
                    link: self.link.id,
                    kind: LinkEventKind::Refused,
                    actor: "system".to_owned(),
                    at,
                    revision: message.revision,
                    detail: Some(error.code()),
                };
                record(self.audit, &event, "refuse a handshake message")?;
                Err(error)
            }
        }
    }

    fn receive_inner(
        &mut self,
        message: &HandshakeMessage,
        at: u64,
        claim: Option<&NonceClaim>,
    ) -> Result<Received> {
        message.verify()?;
        if message.revision < self.next_revision {
            return Err(Error::Replayed {
                revision: message.revision,
            });
        }
        if message.revision > self.next_revision {
            return Err(Error::OutOfOrder {
                expected: self.next_revision,
                found: message.revision,
            });
        }
        if message.previous != self.head {
            return Err(Error::TranscriptMismatch);
        }
        if self.used.contains(&message.nonce) {
            return Err(Error::Replayed {
                revision: message.revision,
            });
        }
        let sender = if message.revision.is_multiple_of(2) {
            &self.proposer
        } else {
            &self.peer
        };
        let signer_party = message.signer;
        let sender_index = self.link.party_index(sender).ok_or(Error::InvalidParty {
            reason: "the expected sender is one of the link's parties",
        })?;
        if let Some(expected) = self.link.peer_keys[sender_index]
            && expected != signer_party
        {
            return Err(Error::WrongSigner {
                expected: expected.fingerprint(),
                found: signer_party.fingerprint(),
            });
        }
        if message.body.link() != Some(&self.link.id) {
            return Err(Error::WrongLink {
                expected: self.link.id.to_string(),
                found: message
                    .body
                    .link()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "none".to_owned()),
            });
        }
        // The nonce is claimed before anything is applied. A message whose nonce the log
        // already holds is a replay, even when this process has never seen it.
        if !record_nonce(self.policy.nonces, &self.link.id, &message.nonce, claim)? {
            return Err(Error::Replayed {
                revision: message.revision,
            });
        }
        let received = match (&message.body, message.revision) {
            (HandshakeBody::Accept(body), 1) => {
                if self.local != self.proposer {
                    return Err(Error::WrongStep {
                        expected: "key_exchange",
                        found: "accept",
                    });
                }
                if body.offer.side != *sender {
                    return Err(Error::InvalidOffer {
                        reason: "an offer is made by the side that sends it",
                    });
                }
                body.offer.validate(self.link.created, self.link.carrier)?;
                if body.consent.party != *sender {
                    return Err(Error::ConsentInvalid {
                        reason: "a consent names the side that sent it",
                    });
                }
                if body.consent.at < self.link.created {
                    return Err(Error::ConsentInvalid {
                        reason: "a consent is dated after the draft it approves",
                    });
                }
                body.consent
                    .check_use(at, self.policy.consent, self.policy.gate)?;
                if let Some(first) = self.link.consents.first() {
                    require_distinct(first, &body.consent)?;
                } else {
                    return Err(Error::ConsentRequired {
                        link: self.link.id.to_string(),
                    });
                }
                // The invitee's key arrives with its acceptance and is recorded here for the
                // first time. It must be the pinned key, or one the administrator confirmed.
                // A key already on the link was pinned when it was first recorded, and the
                // check above has already compared the signer with it.
                if self.link.peer_keys[sender_index].is_none() {
                    trust_peer_key(self.policy, sender, signer_party)?;
                }
                let grants = grants_match(&self.local_offer, &body.offer)?;
                let event = LinkEvent {
                    link: self.link.id,
                    kind: LinkEventKind::Accepted,
                    actor: "system".to_owned(),
                    at,
                    revision: 1,
                    detail: None,
                };
                record(self.audit, &event, "receive a link acceptance")?;
                self.link.grants = grants;
                self.link.consents.push(body.consent.clone());
                self.link.state = LinkState::AcceptedByPeer;
                self.link.peer_keys[sender_index] = Some(signer_party);
                self.remote_offer = Some(body.offer.clone());
                Received::Accepted
            }
            (HandshakeBody::KeyExchange(body), 2 | 3) => {
                if body.key != signer_party {
                    return Err(Error::WrongSigner {
                        expected: body.key.fingerprint(),
                        found: signer_party.fingerprint(),
                    });
                }
                let index = self.link.party_index(sender).ok_or(Error::InvalidParty {
                    reason: "the sender is one of the link's parties",
                })?;
                let event = LinkEvent {
                    link: self.link.id,
                    kind: LinkEventKind::KeyExchanged,
                    actor: "system".to_owned(),
                    at,
                    revision: message.revision,
                    detail: None,
                };
                record(self.audit, &event, "receive a link identity key")?;
                self.link.peer_keys[index] = Some(body.key);
                Received::KeyExchanged
            }
            (HandshakeBody::Activate(body), 4) => {
                if self.local == self.proposer {
                    return Err(Error::WrongStep {
                        expected: "key_exchange",
                        found: "activate",
                    });
                }
                self.require_ready_to_activate()?;
                let digest = grants_digest(&self.link.grants, &self.local, &self.link.parties);
                if digest != body.grants {
                    return Err(Error::TranscriptMismatch);
                }
                let event = LinkEvent {
                    link: self.link.id,
                    kind: LinkEventKind::Activated,
                    actor: "system".to_owned(),
                    at,
                    revision: 4,
                    detail: None,
                };
                record(self.audit, &event, "receive a link activation")?;
                self.link.state = LinkState::Active;
                Received::Activated
            }
            (HandshakeBody::Reject(body), 1..=4) => {
                let event = LinkEvent {
                    link: self.link.id,
                    kind: LinkEventKind::Rejected,
                    actor: "system".to_owned(),
                    at,
                    revision: message.revision,
                    detail: Some(body.reason.as_str()),
                };
                record(self.audit, &event, "receive a link rejection")?;
                self.link.state = LinkState::Revoked;
                self.link.revocation = Some(Revocation {
                    by: sender.clone(),
                    reason: RevokeReason::TermsEnded,
                    at,
                });
                Received::Rejected
            }
            (body, revision) => {
                return Err(Error::WrongStep {
                    expected: expected_step(revision),
                    found: body.name(),
                });
            }
        };
        self.used.insert(message.nonce);
        self.head = Some(message.digest());
        self.next_revision = self.next_revision.saturating_add(1);
        Ok(received)
    }

    /// Rebuilds one side's handshake after a restart, from the stored link and the transcript tail.
    ///
    /// Used nonces are **not** restored from the link: they are read from the policy's
    /// [`NonceLog`], which is what keeps a captured message from being applied twice after a
    /// restart. A host that rebuilds with a fresh log loses that protection, so the log must be
    /// as durable as the link.
    ///
    /// # Errors
    /// A link whose local side is not `signer`'s party, a malformed link or offer, a link with no
    /// consent to name its proposer, or a revision outside a running handshake.
    pub fn resume(
        signer: &'a dyn IdentitySigner,
        audit: &'a dyn AuditSink,
        policy: &'a HandshakePolicy<'a>,
        resume: Resume,
    ) -> Result<Self> {
        let Resume {
            link,
            local_offer,
            remote_offer,
            head,
            next_revision,
        } = resume;
        link.validate()?;
        if signer.party() != link.local {
            return Err(Error::WrongSigner {
                expected: link.local.canonical(),
                found: signer.party().canonical(),
            });
        }
        if !(1..=4).contains(&next_revision) {
            return Err(Error::WrongStep {
                expected: "a link still in handshake",
                found: "resume",
            });
        }
        let proposer = link
            .consents
            .first()
            .map(|consent| consent.party.clone())
            .ok_or_else(|| Error::ConsentRequired {
                link: link.id.to_string(),
            })?;
        let peer = link.peer().cloned().ok_or(Error::InvalidParty {
            reason: "the local side is one of the link's parties",
        })?;
        local_offer.validate(link.created, link.carrier)?;
        if local_offer.side != link.local {
            return Err(Error::InvalidOffer {
                reason: "an offer is made by the side that sends it",
            });
        }
        if let Some(remote) = &remote_offer {
            remote.validate(link.created, link.carrier)?;
            if remote.side != peer {
                return Err(Error::InvalidOffer {
                    reason: "an offer is made by the side that sends it",
                });
            }
        }
        let local = link.local.clone();
        Ok(Self {
            signer,
            audit,
            policy,
            link,
            local,
            peer,
            proposer,
            local_offer,
            remote_offer,
            next_revision,
            head: Some(head),
            used: BTreeSet::new(),
        })
    }

    /// The link as it stands. Its version is 0 until a store assigns one.
    #[must_use]
    pub fn link(&self) -> &Link {
        &self.link
    }

    /// The finished (or refused) link, to store.
    #[must_use]
    pub fn into_link(self) -> Link {
        self.link
    }

    /// The next revision either side will send or receive.
    #[must_use]
    pub fn next_revision(&self) -> u64 {
        self.next_revision
    }

    /// This side.
    #[must_use]
    pub fn local(&self) -> &Party {
        &self.local
    }

    /// The other side.
    #[must_use]
    pub fn peer(&self) -> &Party {
        &self.peer
    }

    fn require_local_turn(&self, step: &'static str) -> Result<()> {
        let local_is_proposer = self.local == self.proposer;
        let local_turn = self.next_revision.is_multiple_of(2) == local_is_proposer;
        if !local_turn {
            return Err(Error::WrongStep {
                expected: expected_step(self.next_revision),
                found: step,
            });
        }
        Ok(())
    }

    fn require_ready_to_activate(&self) -> Result<()> {
        if self.link.state != LinkState::AcceptedByPeer {
            return Err(Error::InvalidTransition {
                link: self.link.id.to_string(),
                from: self.link.state,
                to: LinkState::Active,
            });
        }
        if self.link.consents.len() != 2 {
            return Err(Error::ConsentRequired {
                link: self.link.id.to_string(),
            });
        }
        require_distinct(&self.link.consents[0], &self.link.consents[1])?;
        if self.link.peer_keys.iter().any(Option::is_none) {
            return Err(Error::InvalidGrant {
                reason: "both identity keys are exchanged before activation",
            });
        }
        if self.link.peer_keys[0] == self.link.peer_keys[1] {
            return Err(Error::InvalidGrant {
                reason: "the two parties sign with different identity keys",
            });
        }
        if self.link.grants.is_empty() {
            return Err(Error::InvalidGrant {
                reason: "a link activates with at least one agreed topic",
            });
        }
        Ok(())
    }

    fn advance(&mut self, message: &HandshakeMessage) {
        self.head = Some(message.digest());
        self.next_revision = self.next_revision.saturating_add(1);
    }
}

fn expected_step(revision: u64) -> &'static str {
    match revision {
        0 => "propose",
        1 => "accept or reject",
        2 | 3 => "key_exchange or reject",
        4 => "activate or reject",
        _ => "nothing; the handshake is complete",
    }
}

/// Records a refused step on this side and returns the refusal.
///
/// The refusal is written to the trail before it is returned, so a refused handshake step never
/// lacks its audit entry. When the trail itself cannot be written, that failure is returned
/// instead: the step was not merely refused, its refusal could not be recorded.
fn refusal(audit: &dyn AuditSink, link: LinkId, at: u64, revision: u64, error: Error) -> Error {
    let event = LinkEvent {
        link,
        kind: LinkEventKind::Refused,
        actor: "system".to_owned(),
        at,
        revision,
        detail: Some(error.code()),
    };
    match record(audit, &event, "refuse a link handshake step") {
        Ok(()) => error,
        Err(audit_error) => audit_error,
    }
}

/// Checks the first identity key a party sends against the pins this side already holds.
///
/// See [`check_peer_key`]; later messages are checked against the key the link recorded.
fn trust_peer_key(policy: &HandshakePolicy<'_>, party: &Party, key: PeerKey) -> Result<()> {
    check_peer_key(policy.keys, &policy.trust, party, key)
}

/// Records a received message's nonce: through the caller's claim when there is one, else
/// normally. A claim never widens the normal path: it is accepted once, for its own nonce and link.
fn record_nonce(
    log: &dyn NonceLog,
    link: &LinkId,
    nonce: &Nonce,
    claim: Option<&NonceClaim>,
) -> Result<bool> {
    match claim {
        Some(claim) => log.record_claimed(link, nonce, claim),
        None => log.record(link, nonce),
    }
}
