//! The administration operations behind the management API, as service functions.
//!
//! Each operation:
//!
//! 1. checks the **`link.manage` right** through [`LinkPermissions`]; a denial is itself audited
//!    (`tenant_link.refused`, detail `tenant_link_pg.permission_denied`) and nothing else runs;
//! 2. re-checks what the type system cannot: the proof of every consent goes through the
//!    [`StepUpGate`] again here (a [`Consent`]'s fields are public, so one built by hand has not
//!    necessarily been through the gate), the consenting administrator must be the caller, and
//!    the link must belong to this tenant;
//! 3. writes the audit record **before** the change, and when the change then fails writes a
//!    `change_failed` record, exactly as `spot-tenant-link`'s `LinkService` does; every record
//!    also leaves a reference in the tenant's schema ([`RefRecordingSink`]).
//!
//! The operation ids the management API registers are in [`Operation::id`]
//! (`link.propose`, `link.accept`, `link.reject`, `link.grant`, `link.revoke`, `link.list`,
//! `link.show`, `link.audit`). They are listed in the PR for the lead to add to the catalogue;
//! this crate does not edit `spot-mgmt-ops`.
//!
//! What these functions do **not** do is the cryptography: signing and verifying handshake
//! messages, exchanging keys and activating a link are `spot_tenant_link::Handshake`'s, driven by
//! the transport. The finished link is stored with [`LinkStore::update`]; an operation here never
//! moves a link to `Active`.

use spot_audit_chain::{AuditRecord, Payload, PayloadValue, Subject};
use spot_permissions::Principal;
use spot_tenant_link::consent::require_distinct;
use spot_tenant_link::{
    AuditSink, Consent, Error, Grant, Link, LinkDraft, LinkEvent, LinkEventKind, LinkId,
    LinkService, LinkState, LinkStore, Nonce, NonceLog, Offer, Party, REVOKE_ACTIVE_REVISION,
    RejectReason, Revocation, RevokeMessage, RevokeReason, StepUpGate, grant_allowed, grants_match,
    record_event,
};

use crate::audit_refs::{AuditRef, RefRecordingSink};
use crate::error::{OpError, PERMISSION_DENIED};
use crate::permissions::{LinkPermissions, Operation};
use crate::store::PgLinkStore;

/// The audit kind of a grant change. `spot-tenant-link` has no event for it, so it carries this
/// crate's prefix.
pub const GRANTED_KIND: &str = "tenant_link_pg.granted";

/// Who is asking.
#[derive(Clone, Debug)]
pub struct Caller {
    principal: Principal,
}

impl Caller {
    /// A caller from the permission engine's principal.
    #[must_use]
    pub fn new(principal: Principal) -> Self {
        Self { principal }
    }

    /// The principal.
    #[must_use]
    pub fn principal(&self) -> &Principal {
        &self.principal
    }

    /// The audit actor: `system` or `user:<name>`; `None` for a name the audit chain would not
    /// accept, which the operations treat as a denial.
    #[must_use]
    pub fn actor(&self) -> Option<String> {
        if self.principal.is_system {
            return Some("system".to_owned());
        }
        let user = &self.principal.user;
        (!user.is_empty() && user.len() <= 200 && !user.contains(char::is_whitespace))
            .then(|| format!("user:{user}"))
    }
}

/// The operations over one tenant's link store.
pub struct LinkAdmin<'a> {
    store: &'a PgLinkStore,
    sink: RefRecordingSink<'a>,
    gate: &'a dyn StepUpGate,
    permissions: &'a dyn LinkPermissions,
}

impl<'a> LinkAdmin<'a> {
    /// Operations over `store`, writing to the host's audit `chain`, checking consent proofs
    /// through `gate` and the right through `permissions`.
    #[must_use]
    pub fn new(
        store: &'a PgLinkStore,
        chain: &'a dyn AuditSink,
        gate: &'a dyn StepUpGate,
        permissions: &'a dyn LinkPermissions,
    ) -> Self {
        Self {
            store,
            sink: RefRecordingSink::new(chain, store),
            gate,
            permissions,
        }
    }

    /// Checks the right and returns the audit actor.
    fn authorize(
        &self,
        caller: &Caller,
        operation: Operation,
        link: Option<&LinkId>,
        now: u64,
    ) -> Result<String, OpError> {
        let actor = caller.actor();
        let allowed = actor.is_some() && self.permissions.may(caller.principal(), operation, link);
        match (allowed, actor) {
            (true, Some(actor)) => Ok(actor),
            (_, actor) => {
                match (actor, link) {
                    (Some(actor), Some(link)) => {
                        let event = LinkEvent {
                            link: *link,
                            kind: LinkEventKind::Refused,
                            actor,
                            at: now,
                            revision: 0,
                            detail: Some(PERMISSION_DENIED),
                        };
                        // A failure to record the denial does not turn it into an approval.
                        let _ = record_event(&self.sink, &event, "record a denied link operation");
                    }
                    (Some(actor), None) => {
                        // A call that names no link (a `list`): the denial carries the
                        // operation, the caller and the time, and no link id at all.
                        if let Ok(record) = denied_record(operation, &actor, now) {
                            let _ = self.sink.append_unscoped(record);
                        }
                    }
                    (None, _) => {}
                }
                Err(OpError::Denied {
                    operation: operation.name(),
                })
            }
        }
    }

    /// Checks a consent the caller supplies: it is for `party`, from the caller, in the past,
    /// well formed, and its step-up proof passes the gate **now**.
    fn confirm_consent(
        &self,
        consent: &Consent,
        party: &Party,
        actor: &str,
        now: u64,
    ) -> Result<(), Error> {
        if consent.party != *party {
            return Err(Error::ConsentInvalid {
                reason: "a side consents for itself",
            });
        }
        if consent.actor != actor {
            return Err(Error::ConsentInvalid {
                reason: "the consenting administrator is the caller",
            });
        }
        consent.check_shape(now)?;
        self.gate.confirm(&consent.actor, &consent.proof, now)
    }

    /// Writes the audit record, then stores; a failed store leaves a `change_failed` record.
    fn commit(&self, next: Link, expected: u64, event: LinkEvent) -> Result<Link, OpError> {
        record_event(&self.sink, &event, "record a link change")?;
        match self.store.update(next, expected) {
            Ok(stored) => Ok(stored),
            Err(error) => {
                self.record_failure(&event, &error);
                Err(error.into())
            }
        }
    }

    fn record_failure(&self, event: &LinkEvent, error: &Error) {
        let failed = LinkEvent {
            link: event.link,
            kind: LinkEventKind::ChangeFailed,
            actor: "system".to_owned(),
            at: event.at,
            revision: event.revision,
            detail: Some(error.code()),
        };
        // A failure to record the failure does not hide the store error.
        let _ = record_event(&self.sink, &failed, "record a failed link change");
    }

    /// Starts a link from this tenant: stores it as `Proposed` with the proposer's consent and
    /// offer. The transport then runs `Handshake::propose` for the signed message.
    ///
    /// # Errors
    /// [`OpError::Denied`], a malformed draft, a draft that is not this tenant's, a consent that
    /// is not the caller's or whose proof the gate refuses, a live link already joining the
    /// pair, an audit failure or a store failure.
    pub fn propose(
        &self,
        caller: &Caller,
        draft: LinkDraft,
        consent: Consent,
        now: u64,
    ) -> Result<Link, OpError> {
        let actor = self.authorize(caller, Operation::Propose, Some(&draft.id), now)?;
        draft.validate()?;
        if draft.local != self.store.local_party() {
            return Err(Error::InvalidParty {
                reason: "a tenant proposes links from itself",
            }
            .into());
        }
        if consent.at < draft.created {
            return Err(Error::ConsentInvalid {
                reason: "a consent is dated after the draft it approves",
            }
            .into());
        }
        self.confirm_consent(&consent, &draft.local, &actor, now)?;
        let link = Link {
            id: draft.id,
            parties: draft.parties(),
            local: draft.local.clone(),
            carrier: draft.carrier,
            state: LinkState::Proposed,
            created: draft.created,
            peer_keys: [None, None],
            grants: Vec::new(),
            consents: vec![consent],
            revocation: None,
            expires: draft.expires,
            version: 0,
        };
        link.validate()?;
        let event = LinkEvent {
            link: link.id,
            kind: LinkEventKind::Proposed,
            actor,
            at: now,
            revision: 0,
            detail: None,
        };
        record_event(&self.sink, &event, "propose a link")?;
        match self
            .store
            .run(self.store.insert_with_offer(link, &draft.offer))
        {
            Ok(stored) => Ok(stored),
            Err(error) => {
                self.record_failure(&event, &error);
                Err(error.into())
            }
        }
    }

    /// Accepts a proposal received from the peer: checks `offer` against the peer's, keeps the
    /// intersection as the agreed grants, adds this side's consent and moves the link to
    /// `AcceptedByPeer`.
    ///
    /// # Errors
    /// [`OpError::Denied`], a link that is not a proposal from the peer, a malformed offer, a
    /// consent that is not the caller's, not distinct from the proposer's or refused by the
    /// gate, offers that disagree, a version conflict, an audit failure or a store failure.
    pub fn accept(
        &self,
        caller: &Caller,
        id: &LinkId,
        offer: Offer,
        consent: Consent,
        now: u64,
    ) -> Result<Link, OpError> {
        let actor = self.authorize(caller, Operation::Accept, Some(id), now)?;
        let link = self.store.get(id)?;
        if link.state != LinkState::Proposed {
            return Err(Error::InvalidTransition {
                link: id.to_string(),
                from: link.state,
                to: LinkState::AcceptedByPeer,
            }
            .into());
        }
        let proposal = link
            .consents
            .first()
            .ok_or_else(|| Error::ConsentRequired {
                link: id.to_string(),
            })?;
        if proposal.party == link.local {
            return Err(Error::WrongStep {
                expected: "a proposal from the peer",
                found: "accept",
            }
            .into());
        }
        if offer.side != link.local {
            return Err(Error::InvalidOffer {
                reason: "an offer is made by the side that sends it",
            }
            .into());
        }
        offer.validate(link.created, link.carrier)?;
        self.confirm_consent(&consent, &link.local, &actor, now)?;
        require_distinct(proposal, &consent)?;
        let (_, remote) = self.store.run(self.store.offers(id))?;
        let remote = remote.ok_or(Error::InvalidOffer {
            reason: "the peer's offer is not on record",
        })?;
        let grants = grants_match(&offer, &remote)?;
        let mut next = link.clone();
        next.grants = grants;
        next.consents.push(consent);
        next.state = LinkState::AcceptedByPeer;
        let event = LinkEvent {
            link: *id,
            kind: LinkEventKind::Accepted,
            actor,
            at: now,
            revision: link.version,
            detail: None,
        };
        // The audit record is written first (a failure stops here, with nothing stored). The
        // offer, the grants, the consent and the state change then land in one transaction that
        // checks the version: a conflict leaves no offer row behind.
        record_event(&self.sink, &event, "record a link change")?;
        match self
            .store
            .run(self.store.update_with_offer(next, link.version, &offer))
        {
            Ok(stored) => Ok(stored),
            Err(error) => {
                self.record_failure(&event, &error);
                Err(error.into())
            }
        }
    }

    /// Rejects a proposal, or withdraws one this side made, while the link is still in
    /// handshake. The link becomes `Revoked`.
    ///
    /// # Errors
    /// [`OpError::Denied`], a link that is no longer in handshake, a version conflict, an audit
    /// failure or a store failure.
    pub fn reject(
        &self,
        caller: &Caller,
        id: &LinkId,
        reason: RejectReason,
        now: u64,
    ) -> Result<Link, OpError> {
        let actor = self.authorize(caller, Operation::Reject, Some(id), now)?;
        let link = self.store.get(id)?;
        if !matches!(link.state, LinkState::Proposed | LinkState::AcceptedByPeer) {
            return Err(Error::InvalidTransition {
                link: id.to_string(),
                from: link.state,
                to: LinkState::Revoked,
            }
            .into());
        }
        let mut next = link.clone();
        next.state = LinkState::Revoked;
        next.revocation = Some(Revocation {
            by: link.local.clone(),
            reason: RevokeReason::TermsEnded,
            at: now,
        });
        let event = LinkEvent {
            link: *id,
            kind: LinkEventKind::Rejected,
            actor,
            at: now,
            revision: link.version,
            detail: Some(reason.as_str()),
        };
        self.commit(next, link.version, event)
    }

    /// Sets this side's grant for a topic on an active link, adding the topic or replacing its
    /// grant. The grant may not be wider than **either** side's offer in any dimension (topic,
    /// kind, direction, schema range, fields, expiry, region, notice); a wider grant is refused
    /// with the dimension named, never trimmed.
    ///
    /// The change is this side's alone: the peer enforces its own copy of the link, so widening
    /// here cannot open anything the peer's copy does not allow.
    ///
    /// # Errors
    /// [`OpError::Denied`], a link that is not active, a grant outside the offers, offers not on
    /// record, a version conflict, an audit failure or a store failure.
    pub fn grant(
        &self,
        caller: &Caller,
        id: &LinkId,
        grant: Grant,
        now: u64,
    ) -> Result<Link, OpError> {
        let actor = self.authorize(caller, Operation::Grant, Some(id), now)?;
        let link = self.store.get(id)?;
        if link.state != LinkState::Active {
            return Err(Error::NotActive {
                link: id.to_string(),
                state: link.state,
            }
            .into());
        }
        grant.validate(link.created)?;
        let (local, remote) = self.store.run(self.store.offers(id))?;
        let (Some(local), Some(remote)) = (local, remote) else {
            return Err(Error::InvalidOffer {
                reason: "both sides' offers are on record",
            }
            .into());
        };
        grant_allowed(&grant, &local, &remote)?;
        let mut next = link.clone();
        match next
            .grants
            .iter_mut()
            .find(|current| current.topic == grant.topic)
        {
            Some(slot) => *slot = grant.clone(),
            None => next.grants.push(grant.clone()),
        }
        next.validate()?;
        let record = granted_record(id, &actor, now, link.version, grant.topic.as_str())?;
        use spot_tenant_link::AuditSink as _;
        self.sink.append(record).map_err(|_| Error::AuditFailed {
            action: "record a grant change",
        })?;
        match self.store.update(next, link.version) {
            Ok(stored) => Ok(stored),
            Err(error) => {
                let event = LinkEvent {
                    link: *id,
                    kind: LinkEventKind::ChangeFailed,
                    actor: "system".to_owned(),
                    at: now,
                    revision: link.version,
                    detail: Some(error.code()),
                };
                self.record_failure(&event, &error);
                Err(error.into())
            }
        }
    }

    /// Revokes a link from this tenant's side. Effective for new exchanges the moment this
    /// returns: the row is committed and the cache hook has been told.
    ///
    /// The revoking party is this tenant and the audit actor is the authenticated `caller`;
    /// neither can be named by the call. A revocation by the peer arrives through
    /// [`receive_revoke`](Self::receive_revoke).
    ///
    /// # Errors
    /// [`OpError::Denied`], a link already revoked or expired, a version conflict, an audit
    /// failure or a store failure.
    pub fn revoke(
        &self,
        caller: &Caller,
        id: &LinkId,
        reason: RevokeReason,
        now: u64,
    ) -> Result<Link, OpError> {
        let actor = self.authorize(caller, Operation::Revoke, Some(id), now)?;
        let service = LinkService::new(self.store, &self.sink);
        Ok(service.revoke(id, reason, &actor, now)?)
    }

    /// Applies a revocation that arrived from the peer as a signed message.
    ///
    /// There is no caller to authenticate: the signature is the authentication. It must verify,
    /// name a link this tenant holds, be made by the key pinned for the peer on that link, be
    /// for an active link and carry a fresh nonce; each of those is checked before the nonce is
    /// claimed, and the nonce is claimed before the link changes (see
    /// [`LinkService::receive_revoke`]). Effective for new exchanges the moment this returns.
    ///
    /// # Errors
    /// [`Error::BadSignature`], [`Error::NotFound`], [`Error::WrongSigner`],
    /// [`Error::OutOfOrder`], [`Error::NotActive`], [`Error::Replayed`], a version conflict, an
    /// audit failure or a store failure.
    pub fn receive_revoke(&self, message: &RevokeMessage, now: u64) -> Result<Link, OpError> {
        let service = LinkService::new(self.store, &self.sink);
        let nonces = StoreNonces {
            store: self.store,
            at: now,
        };
        Ok(service.receive_revoke(message, &nonces, now)?)
    }

    /// The tenant's links, ordered by id.
    ///
    /// # Errors
    /// [`OpError::Denied`] or a store failure.
    pub fn list(&self, caller: &Caller, now: u64) -> Result<Vec<Link>, OpError> {
        self.authorize_read(caller, Operation::List, None, now)?;
        Ok(self.store.list_for(&self.store.local_party())?)
    }

    /// One link.
    ///
    /// # Errors
    /// [`OpError::Denied`], [`Error::NotFound`] or a store failure.
    pub fn show(&self, caller: &Caller, id: &LinkId, now: u64) -> Result<Link, OpError> {
        self.authorize_read(caller, Operation::Show, Some(id), now)?;
        Ok(self.store.get(id)?)
    }

    /// The audit references of one link, oldest first.
    ///
    /// # Errors
    /// [`OpError::Denied`], [`Error::NotFound`] or a store failure.
    pub fn show_audit(
        &self,
        caller: &Caller,
        id: &LinkId,
        now: u64,
    ) -> Result<Vec<AuditRef>, OpError> {
        self.authorize_read(caller, Operation::ShowAudit, Some(id), now)?;
        self.store.get(id)?;
        Ok(self.store.run(self.store.audit_refs(id))?)
    }

    fn authorize_read(
        &self,
        caller: &Caller,
        operation: Operation,
        link: Option<&LinkId>,
        now: u64,
    ) -> Result<(), OpError> {
        self.authorize(caller, operation, link, now).map(|_| ())
    }
}

/// The nonce log of a revoke: it claims in the store's nonce table, and only ever from
/// [`LinkService::receive_revoke`], which has verified the message first. It is private so no
/// caller can claim a nonce for a message nobody checked.
struct StoreNonces<'a> {
    store: &'a PgLinkStore,
    at: u64,
}

impl NonceLog for StoreNonces<'_> {
    fn record(&self, link: &LinkId, nonce: &Nonce) -> Result<bool, Error> {
        match self.store.run(
            self.store
                .claim_nonce(link, nonce, self.at, REVOKE_ACTIVE_REVISION),
        ) {
            Ok(()) => Ok(true),
            Err(Error::Replayed { .. }) => Ok(false),
            Err(error) => Err(error),
        }
    }

    fn release(&self, link: &LinkId, nonce: &Nonce) -> Result<(), Error> {
        self.store.run(self.store.release_nonce(link, nonce))
    }
}

/// The audit record of a refused call that names no link (a denied `list`): kind
/// [`PERMISSION_DENIED`], the operation, the caller and the time, and no link id, field or value.
/// The tenant keeps a reference to it ([`PgLinkStore::denied_audit_refs`]).
fn denied_record(operation: Operation, actor: &str, at: u64) -> Result<AuditRecord, Error> {
    let at = i64::try_from(at).map_err(|_| Error::InvalidId {
        reason: "the audit instant is out of range",
    })?;
    let mut payload = Payload::new();
    payload.insert("event".to_owned(), PayloadValue::Text("denied".to_owned()));
    payload.insert("operation".to_owned(), PayloadValue::Text(operation.id()));
    payload.insert(
        "detail".to_owned(),
        PayloadValue::Text(PERMISSION_DENIED.to_owned()),
    );
    Ok(AuditRecord {
        kind: PERMISSION_DENIED.to_owned(),
        actor: actor.to_owned(),
        on_behalf_of: None,
        subject: None,
        at,
        payload,
        masked: true,
    })
}

/// The audit record of a grant change: link, event, revision and the topic name. No field name,
/// field value or key.
fn granted_record(
    link: &LinkId,
    actor: &str,
    at: u64,
    revision: u64,
    topic: &str,
) -> Result<AuditRecord, Error> {
    let at = i64::try_from(at).map_err(|_| Error::InvalidId {
        reason: "the audit instant is out of range",
    })?;
    let mut payload = Payload::new();
    payload.insert("link".to_owned(), PayloadValue::Text(link.to_string()));
    payload.insert("event".to_owned(), PayloadValue::Text("granted".to_owned()));
    payload.insert(
        "revision".to_owned(),
        PayloadValue::Int(i64::try_from(revision).unwrap_or(i64::MAX)),
    );
    payload.insert("topic".to_owned(), PayloadValue::Text(topic.to_owned()));
    Ok(AuditRecord {
        kind: GRANTED_KIND.to_owned(),
        actor: actor.to_owned(),
        on_behalf_of: None,
        subject: Some(Subject {
            doctype: "tenant_link".to_owned(),
            id: link.to_string(),
        }),
        at,
        payload,
        masked: true,
    })
}
