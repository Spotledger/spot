//! The operations around a stored link: revoke, expire and every use.
//!
//! [`LinkService`] is the one place a stored link changes after the handshake. Each mutation
//! writes its audit entry **before** the store update, so a persisted change never lacks its
//! audit entry; when the store then fails, a `change_failed` entry records that the audited
//! change did not land.
//!
//! Every sender and receiver calls [`LinkService::authorize`]; it runs [`Link::allows`] (or
//! [`Link::allows_with_region`]) and audits both the allowed and the refused use, so the trail
//! answers "what crossed, and what was refused".

use crate::audit::{AuditSink, LinkEvent, LinkEventKind, record};
use crate::error::{Error, Result};
use crate::grant::{Direction, ExchangePayload, Filtered, Grant};
use crate::ids::{LinkId, Topic};
use crate::model::{Link, LinkState, Party, RevokeReason};
use crate::nonce::NonceLog;
use crate::region::RegionOracle;
use crate::store::LinkStore;
use crate::wire::{REVOKE_ACTIVE_REVISION, RevokeMessage};

/// One sender-side call: which link, topic and direction, when, by whom, with which payload and
/// (when a grant requires it) against which region oracle.
pub struct SendRequest<'a> {
    /// The link.
    pub link: &'a LinkId,
    /// The topic.
    pub topic: &'a Topic,
    /// The direction, read from this side.
    pub direction: Direction,
    /// Unix milliseconds.
    pub now: u64,
    /// The acting principal.
    pub actor: &'a str,
    /// The payload the app wants to send.
    pub payload: &'a ExchangePayload,
    /// The peer-region oracle, required when the grant carries a requirement.
    pub oracle: Option<&'a dyn RegionOracle>,
}

/// Link operations over one tenant's store and audit trail.
pub struct LinkService<'a> {
    store: &'a dyn LinkStore,
    audit: &'a dyn AuditSink,
}

impl<'a> LinkService<'a> {
    /// A service over a store and an audit trail.
    #[must_use]
    pub fn new(store: &'a dyn LinkStore, audit: &'a dyn AuditSink) -> Self {
        Self { store, audit }
    }

    /// Stores a link finished by a handshake. The handshake already wrote its state changes to
    /// the local trail, so this adds none.
    ///
    /// # Errors
    /// [`Error::StoreFailed`] or a validation refusal from the store.
    pub fn register(&self, link: Link) -> Result<Link> {
        self.store.insert(link)
    }

    /// Revokes a link from this side; effective for new exchanges the moment the store answers.
    ///
    /// The revoking party is always the link's local side (`Link::local`, which a store keeps
    /// equal to the tenant it belongs to): there is no parameter to name another party. A
    /// revocation by the peer arrives as a signed message through
    /// [`LinkService::receive_revoke`]. `actor` is the audit actor of the authenticated caller;
    /// a host takes it from its authenticated principal, as `spot-tenant-link-pg`'s
    /// `LinkAdmin::revoke` does.
    ///
    /// # Errors
    /// [`Error::NotFound`], [`Error::InvalidTransition`], [`Error::AuditFailed`] (the store was
    /// not changed) or [`Error::StoreFailed`].
    pub fn revoke(&self, id: &LinkId, reason: RevokeReason, actor: &str, at: u64) -> Result<Link> {
        let link = self.store.get(id)?;
        let by = link.local.clone();
        let next = link.revoked_by(&by, reason, at)?;
        let event = LinkEvent {
            link: *id,
            kind: LinkEventKind::Revoked,
            actor: actor.to_owned(),
            at,
            revision: link.version,
            detail: Some(reason.as_str()),
        };
        self.commit(next, link.version, event)
    }

    /// Applies a revocation that arrived from the peer.
    ///
    /// Everything is checked **before** the nonce is claimed and before any state changes: the
    /// signature over the whole message, that the link is one this side holds, that the signer
    /// is the key pinned for the peer on that link, that the message was signed for an active
    /// link ([`REVOKE_ACTIVE_REVISION`]) and that the link is active. Only then is the nonce
    /// claimed, so an outsider cannot fill the nonce log with messages that prove nothing, and
    /// a replay of a genuine revoke is refused. The recorded revoking party is the link's peer.
    ///
    /// A message that fails the signature check leaves no trace (it is anonymous); one that
    /// names a link of this side and then fails is audited as a refusal.
    ///
    /// # Errors
    /// [`Error::BadSignature`] or [`Error::WrongStep`] (a version this crate does not know),
    /// [`Error::NotFound`], [`Error::WrongSigner`], [`Error::OutOfOrder`] (stale or unknown
    /// revision), [`Error::NotActive`], [`Error::Replayed`], [`Error::AuditFailed`],
    /// [`Error::VersionConflict`] or [`Error::StoreFailed`]. Nothing in an error carries a key
    /// or a nonce.
    pub fn receive_revoke(
        &self,
        message: &RevokeMessage,
        nonces: &dyn NonceLog,
        at: u64,
    ) -> Result<Link> {
        message.verify()?;
        let link = self.store.get(&message.link)?;
        // Until the signer is the pinned peer key the message proves nothing about this link,
        // so a refusal here leaves no audit row: anyone who knows a link id could otherwise
        // flood the chain.
        let peer = pinned_peer(&link, message)?;
        match self.apply_remote_revoke(link.clone(), &peer, message, nonces, at) {
            Ok(stored) => Ok(stored),
            Err(error) => {
                let event = LinkEvent {
                    link: link.id,
                    kind: LinkEventKind::Refused,
                    actor: "system".to_owned(),
                    at,
                    revision: link.version,
                    detail: Some(error.code()),
                };
                // A failure to record the refusal does not hide the refusal.
                let _ = record(self.audit, &event, "record a refused remote revoke");
                Err(error)
            }
        }
    }

    /// Checks everything that needs no write, claims the nonce, then commits. A commit that
    /// loses a version race is retried on a fresh read (at most [`REVOKE_ATTEMPTS`] commits);
    /// any other failure releases the nonce, so a genuine revoke is never burned by a store or
    /// audit failure and a resend can still apply.
    fn apply_remote_revoke(
        &self,
        mut link: Link,
        peer: &Party,
        message: &RevokeMessage,
        nonces: &dyn NonceLog,
        at: u64,
    ) -> Result<Link> {
        check_revoke_state(&link, message)?;
        link.revoked_by(peer, message.reason, at)?;
        if !nonces.record(&link.id, &message.nonce)? {
            return Err(Error::Replayed {
                revision: message.revision,
            });
        }
        let mut attempt = 1;
        loop {
            let outcome = check_revoke_state(&link, message)
                .and_then(|()| link.revoked_by(peer, message.reason, at))
                .and_then(|next| {
                    let event = LinkEvent {
                        link: link.id,
                        kind: LinkEventKind::Revoked,
                        actor: "system".to_owned(),
                        at,
                        revision: link.version,
                        detail: Some(message.reason.as_str()),
                    };
                    self.commit(next, link.version, event)
                });
            match outcome {
                Ok(stored) => return Ok(stored),
                Err(Error::VersionConflict { .. }) if attempt < REVOKE_ATTEMPTS => {
                    attempt += 1;
                    match self.store.get(&link.id) {
                        Ok(fresh) => link = fresh,
                        Err(error) => {
                            let _ = nonces.release(&link.id, &message.nonce);
                            return Err(error);
                        }
                    }
                }
                Err(error) => {
                    // The revoke did not land: give the nonce back so a resend is not
                    // refused as a replay. A failure to release does not hide the error.
                    let _ = nonces.release(&link.id, &message.nonce);
                    return Err(error);
                }
            }
        }
    }

    /// Moves an expired link to `Expired`.
    ///
    /// # Errors
    /// [`Error::NotFound`], [`Error::InvalidTransition`], [`Error::InvalidExpiry`],
    /// [`Error::AuditFailed`] or [`Error::StoreFailed`].
    pub fn expire(&self, id: &LinkId, at: u64) -> Result<Link> {
        let link = self.store.get(id)?;
        let next = link.expired_at(at)?;
        let event = LinkEvent {
            link: *id,
            kind: LinkEventKind::Expired,
            actor: "system".to_owned(),
            at,
            revision: link.version,
            detail: None,
        };
        self.commit(next, link.version, event)
    }

    /// The single check every sender and receiver calls: may this side exchange `topic` in
    /// `direction` at `now`, and if so under which grant?
    ///
    /// `oracle` is required when a grant carries a peer-region requirement; without it, such a
    /// grant is refused rather than assumed to pass.
    ///
    /// # Errors
    /// Everything [`Link::allows`] refuses, plus [`Error::RegionRefused`],
    /// [`Error::PeerRegionMismatch`], [`Error::AuditFailed`] and [`Error::StoreFailed`].
    pub fn authorize(
        &self,
        id: &LinkId,
        topic: &Topic,
        direction: Direction,
        now: u64,
        actor: &str,
        oracle: Option<&dyn RegionOracle>,
    ) -> Result<Grant> {
        let link = self.store.get(id)?;
        // Without an oracle `allows` refuses a grant that carries a region requirement.
        let result = match oracle {
            Some(oracle) => link.allows_with_region(topic, direction, now, oracle),
            None => link.allows(topic, direction, now),
        };
        match result {
            Ok(grant) => {
                let event = LinkEvent {
                    link: *id,
                    kind: LinkEventKind::Allowed,
                    actor: actor.to_owned(),
                    at: now,
                    revision: link.version,
                    detail: Some(direction.as_str()),
                };
                record(self.audit, &event, "record a link use")?;
                Ok(grant.clone())
            }
            Err(error) => {
                let event = LinkEvent {
                    link: *id,
                    kind: LinkEventKind::Refused,
                    actor: actor.to_owned(),
                    at: now,
                    revision: link.version,
                    detail: Some(error.code()),
                };
                record(self.audit, &event, "record a refused link use")?;
                Err(error)
            }
        }
    }

    /// The sender-side check: authorize the use and return the payload filtered to the grant's
    /// field list. Nothing the grant does not name is in the result (ADR-058 rule 6).
    ///
    /// # Errors
    /// As [`LinkService::authorize`].
    pub fn prepare_send(&self, request: &SendRequest<'_>) -> Result<Filtered> {
        let grant = self.authorize(
            request.link,
            request.topic,
            request.direction,
            request.now,
            request.actor,
            request.oracle,
        )?;
        Ok(grant.fields.filter(request.payload))
    }

    fn commit(&self, next: Link, expected: u64, event: LinkEvent) -> Result<Link> {
        record(self.audit, &event, "record a link change")?;
        match self.store.update(next, expected) {
            Ok(stored) => Ok(stored),
            Err(error) => {
                let failed = LinkEvent {
                    link: event.link,
                    kind: LinkEventKind::ChangeFailed,
                    actor: "system".to_owned(),
                    at: event.at,
                    revision: event.revision,
                    detail: Some(error.code()),
                };
                // A failure to record the failure does not hide the store error.
                let _ = record(self.audit, &failed, "record a failed link change");
                Err(error)
            }
        }
    }
}

/// How many times a remote revoke commits against a fresh read when it loses a version race.
const REVOKE_ATTEMPTS: u32 = 3;

/// The peer of `link`, when `message` is signed by the key pinned for it on that link.
fn pinned_peer(link: &Link, message: &RevokeMessage) -> Result<Party> {
    let peer = link.peer().cloned().ok_or(Error::InvalidParty {
        reason: "the link's local side is not one of its parties",
    })?;
    let index = link.party_index(&peer).ok_or(Error::InvalidParty {
        reason: "the peer is one of the link's parties",
    })?;
    if link.peer_keys[index] != Some(message.signer) {
        return Err(Error::WrongSigner {
            expected: "the key pinned for the peer on this link".to_owned(),
            found: "another key".to_owned(),
        });
    }
    Ok(peer)
}

/// The revision and state a remote revoke may end: an active link, signed for revision 4.
fn check_revoke_state(link: &Link, message: &RevokeMessage) -> Result<()> {
    if message.revision != REVOKE_ACTIVE_REVISION {
        return Err(Error::OutOfOrder {
            expected: REVOKE_ACTIVE_REVISION,
            found: message.revision,
        });
    }
    if link.state != LinkState::Active {
        return Err(Error::NotActive {
            link: link.id.to_string(),
            state: link.state,
        });
    }
    Ok(())
}
