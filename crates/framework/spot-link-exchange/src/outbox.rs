//! The sender (ADR-058 rule 13): an outbox state machine per message.
//!
//! `Queued -> Sent -> Acknowledged -> Applied | Rejected(code) | Expired`.
//!
//! * A message is signed once, at enqueue, and never changes; a resend is the same bytes, so the
//!   receiver deduplicates on the id.
//! * [`Outbox::next_batch`] hands out only what the link allows right now (`Link::allows`, the
//!   grant's expiry and direction, the peer-region requirement). A revoked or expired link, or a
//!   topic no longer granted, holds the message in `Queued`/`Sent`; it is never sent.
//! * The field list is applied at enqueue, before signing: a field outside the grant is dropped
//!   and only its name is reported.
//! * When a message ends without being applied (it expired, or the receiver rejected it) the
//!   outbox signs a tombstone for its slot, so later messages in the conversation are not held
//!   behind a sequence number that will never arrive.

use std::collections::BTreeMap;

use spot_tenant_link::{
    AuditSink, Direction, Grant, IdentitySigner, Link, LinkEventKind, Party, RegionOracle, Topic,
    TopicKind,
};

use crate::audit::{self, Fact};
use crate::canonical;
use crate::envelope::Envelope;
use crate::error::{Error, Result};
use crate::ids::{Conversation, IdSource, MessageId};
use crate::message::{Message, MessageParts, TOMBSTONE_EXPIRES, TOMBSTONE_PAYLOAD};
use crate::retention::Lifetimes;
use crate::state::{MessageState, Receipt, ReceiptOutcome};
use crate::store::{EventKind, Filler, OutboxEntry, OutboxStore, ProducerEvent};
use crate::transport::TransportParty;

const ENQUEUE_ATTEMPTS: usize = 4;

/// Outbox tuning.
#[derive(Clone, Copy, Debug)]
pub struct OutboxConfig {
    /// A sent message with no receipt is handed to the carrier again after this long.
    pub resend_after_ms: u64,
    /// A message the peer acknowledged but has not yet confirmed as applied is handed over again
    /// after this long, so a lost `Applied` receipt is repaired by the peer's repeated answer.
    pub unapplied_resend_after_ms: u64,
}

impl Default for OutboxConfig {
    fn default() -> Self {
        Self {
            resend_after_ms: 60_000,
            unapplied_resend_after_ms: 600_000,
        }
    }
}

/// Where and when to enqueue a message.
#[derive(Clone, Copy)]
pub struct Submit<'a> {
    /// The link, as the local store holds it.
    pub link: &'a Link,
    /// The conversation.
    pub conversation: &'a Conversation,
    /// The topic.
    pub topic: &'a Topic,
    /// The topic schema version being sent.
    pub schema_version: u32,
    /// Unix milliseconds.
    pub now: u64,
    /// `system`, `user:<id>` or `agent:<id>`.
    pub actor: &'a str,
    /// The peer-region oracle; required when the grant requires a peer region.
    pub oracle: Option<&'a dyn RegionOracle>,
}

/// What an enqueue returns.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Enqueued {
    /// The message id.
    pub id: MessageId,
    /// The sequence number in its conversation.
    pub seq: u64,
    /// When it expires.
    pub expires_at: u64,
    /// The names (never the values) of fields the grant's field list removed.
    pub withheld: Vec<String>,
}

/// What a receipt did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReceiptEffect {
    /// The message moved to this state.
    Moved(MessageState),
    /// The tombstone of a slot was confirmed.
    FillerConfirmed,
    /// Nothing changed: a stale, repeated or out-of-order receipt.
    Ignored,
}

/// The sender side of a link.
pub struct Outbox<'a> {
    store: &'a dyn OutboxStore,
    audit: &'a dyn AuditSink,
    signer: &'a dyn IdentitySigner,
    ids: &'a dyn IdSource,
    lifetimes: Lifetimes,
    config: OutboxConfig,
}

impl<'a> Outbox<'a> {
    /// An outbox over `store`, signing as `signer`.
    #[must_use]
    pub fn new(
        store: &'a dyn OutboxStore,
        audit: &'a dyn AuditSink,
        signer: &'a dyn IdentitySigner,
        ids: &'a dyn IdSource,
        lifetimes: Lifetimes,
    ) -> Self {
        Self {
            store,
            audit,
            signer,
            ids,
            lifetimes,
            config: OutboxConfig::default(),
        }
    }

    /// Replaces the tuning.
    #[must_use]
    pub fn with_config(mut self, config: OutboxConfig) -> Self {
        self.config = config;
        self
    }

    /// Enqueues a message-topic payload. Fields the grant does not list are dropped.
    ///
    /// # Errors
    /// The link's refusal (revoked, expired, not granted, wrong direction, region), a wrong
    /// topic kind, an unsupported schema version, an invalid payload, an audit failure or a
    /// store failure. A refusal is audited.
    pub fn enqueue_message(
        &self,
        request: &Submit<'_>,
        payload: &spot_tenant_link::ExchangePayload,
    ) -> Result<Enqueued> {
        let grant = self.authorize(request, TopicKind::Message)?;
        let filtered = grant.fields.filter(payload);
        let withheld = filtered
            .withheld()
            .iter()
            .map(|name| name.as_str().to_owned())
            .collect();
        let text = serde_json::to_string(filtered.values()).map_err(|_| Error::InvalidPayload {
            reason: "the payload cannot be encoded",
        })?;
        self.push(request, canonical::canonicalize(&text)?, withheld)
    }

    /// Enqueues a record version. Fields the grant does not list are dropped from the image.
    ///
    /// # Errors
    /// As [`Outbox::enqueue_message`].
    pub fn enqueue_record(&self, request: &Submit<'_>, envelope: Envelope) -> Result<Enqueued> {
        let grant = self.authorize(request, TopicKind::Record)?;
        let (envelope, withheld) = envelope.filtered(&grant.fields);
        self.push(request, envelope.encode()?, withheld)
    }

    /// The messages to hand to the carrier now, oldest sequence first, at most `limit`.
    ///
    /// Expired messages are expired first. A topic the link does not allow right now is skipped
    /// (and the refusal audited once per call); its messages wait.
    ///
    /// # Errors
    /// An audit or store failure.
    pub fn next_batch(
        &self,
        link: &Link,
        now: u64,
        limit: usize,
        actor: &str,
        oracle: Option<&dyn RegionOracle>,
    ) -> Result<Vec<Message>> {
        self.expire_due(now)?;
        let mut allowed: BTreeMap<Topic, bool> = BTreeMap::new();
        let mut batch = Vec::new();
        for mut entry in self.store.live(Some(&link.id))? {
            if batch.len() >= limit {
                break;
            }
            let sends_original = matches!(
                entry.state,
                MessageState::Queued | MessageState::Sent | MessageState::Acknowledged
            );
            let interval = if entry.state == MessageState::Acknowledged {
                self.config.unapplied_resend_after_ms
            } else {
                self.config.resend_after_ms
            };
            let last = if sends_original {
                entry.last_sent_at
            } else {
                entry.filler.as_ref().and_then(|f| f.last_sent_at)
            };
            if !sends_original && entry.filler.as_ref().is_none_or(|f| f.acknowledged) {
                continue;
            }
            if last.is_some_and(|at| at.saturating_add(interval) > now) {
                continue;
            }
            let topic = entry.message.topic.clone();
            let permitted = match allowed.get(&topic) {
                Some(permitted) => *permitted,
                None => {
                    let result = self.check_send(link, &topic, now, oracle);
                    if let Err(error) = &result {
                        audit::write(
                            self.audit,
                            actor,
                            &Fact {
                                link: link.id,
                                kind: LinkEventKind::Refused,
                                at: now,
                                revision: link.version,
                                detail: error.code(),
                                action: "record a held send",
                            },
                        )?;
                    }
                    allowed.insert(topic, result.is_ok());
                    result.is_ok()
                }
            };
            if !permitted {
                continue;
            }
            let outgoing = if sends_original {
                if entry.state == MessageState::Queued {
                    entry.state = MessageState::Sent;
                    entry.state_at = now;
                }
                entry.last_sent_at = Some(now);
                entry.message.clone()
            } else if let Some(filler) = entry.filler.as_mut() {
                filler.last_sent_at = Some(now);
                filler.message.clone()
            } else {
                continue;
            };
            entry.attempts = entry.attempts.saturating_add(1);
            match self.store.update(entry) {
                Ok(()) => batch.push(outgoing),
                Err(Error::Conflict { .. }) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(batch)
    }

    /// Expires every `Queued` or `Sent` message whose lifetime ended, on every link, and returns
    /// the events produced. `Expired` is visible on the entry and as an event for the producer,
    /// written in the same transaction as the state.
    ///
    /// # Errors
    /// An audit or store failure.
    pub fn expire_due(&self, now: u64) -> Result<Vec<ProducerEvent>> {
        let mut events = Vec::new();
        for mut entry in self.store.live(None)? {
            let due = matches!(entry.state, MessageState::Queued | MessageState::Sent)
                && entry.message.expires_at <= now;
            if !due {
                continue;
            }
            audit::write(
                self.audit,
                "system",
                &Fact {
                    link: entry.message.link,
                    kind: LinkEventKind::Refused,
                    at: now,
                    revision: 0,
                    detail: "expired",
                    action: "record an expired message",
                },
            )?;
            entry.state = MessageState::Expired;
            entry.state_at = now;
            entry.filler = Some(self.filler_for(&entry.message, now)?);
            let event = event_of(&entry.message, EventKind::Expired, now);
            // One call, one transaction: the state and the event land together or not at all
            //.
            match self.store.settle(entry, Some(event.clone())) {
                Ok(()) => events.push(event),
                Err(Error::Conflict { .. }) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(events)
    }

    /// Applies a receipt from the party the connection authenticated.
    ///
    /// This is the entry point a carrier adapter uses: [`TransportParty`] can only be built from
    /// an identity the adapter verified itself, so a sender read out of the receipt cannot be
    /// passed here. The receipt must still name this link and verify against the pinned key.
    ///
    /// # Errors
    /// As [`Outbox::on_receipt_from`]: the same checks, on the party the connection authenticated.
    pub fn on_receipt_from(
        &self,
        link: &Link,
        transport: &TransportParty,
        receipt: &Receipt,
        now: u64,
    ) -> Result<ReceiptEffect> {
        self.on_receipt(link, transport.party(), receipt, now)
    }

    /// Applies a receipt from the peer.
    ///
    /// The receipt must come over the authenticated transport from the link's peer
    /// (`transport_peer`), name this link, and verify against the key pinned in the link.
    ///
    /// Private on purpose: the only way in is [`Outbox::on_receipt_from`], which cannot
    /// be handed a party read out of the receipt.
    ///
    /// # Errors
    /// [`Error::IdentityMismatch`], [`Error::LinkMismatch`], [`Error::UnknownKey`] or
    /// [`Error::BadSignature`] (each audited), [`Error::NotFound`] for a message this side never
    /// sent, an audit failure or a store failure.
    fn on_receipt(
        &self,
        link: &Link,
        transport_peer: &Party,
        receipt: &Receipt,
        now: u64,
    ) -> Result<ReceiptEffect> {
        if let Err(error) = self.authenticate(link, transport_peer, receipt) {
            audit::write(
                self.audit,
                "system",
                &Fact {
                    link: link.id,
                    kind: LinkEventKind::Refused,
                    at: now,
                    revision: link.version,
                    detail: error.code(),
                    action: "record a refused receipt",
                },
            )?;
            return Err(error);
        }
        let mut entry = self.store.find(&receipt.message)?.ok_or(Error::NotFound)?;
        let belongs = entry.message.link == link.id
            && entry.message.conversation == receipt.conversation
            && entry.message.seq == receipt.seq;
        if !belongs {
            audit::write(
                self.audit,
                "system",
                &Fact {
                    link: link.id,
                    kind: LinkEventKind::Refused,
                    at: now,
                    revision: link.version,
                    detail: Error::LinkMismatch.code(),
                    action: "record a refused receipt",
                },
            )?;
            return Err(Error::LinkMismatch);
        }
        if entry
            .filler
            .as_ref()
            .is_some_and(|f| f.message.id == receipt.message)
        {
            return self.on_filler_receipt(entry, receipt, now);
        }
        let Some(next) = next_state(entry.state, receipt.outcome) else {
            return Ok(ReceiptEffect::Ignored);
        };
        audit::write(
            self.audit,
            "system",
            &Fact {
                link: link.id,
                kind: LinkEventKind::Allowed,
                at: now,
                revision: link.version,
                detail: "receipt",
                action: "record a receipt",
            },
        )?;
        entry.state = next;
        entry.state_at = now;
        if matches!(next, MessageState::Rejected(_)) && entry.filler.is_none() {
            entry.filler = Some(self.filler_for(&entry.message, now)?);
        }
        let event = match next {
            MessageState::Applied => Some(EventKind::Applied),
            MessageState::Rejected(code) => Some(EventKind::Rejected(code)),
            _ => None,
        }
        .map(|kind| event_of(&entry.message, kind, now));
        self.store.settle(entry, event)?;
        Ok(ReceiptEffect::Moved(next))
    }

    /// The current state of one message.
    ///
    /// # Errors
    /// [`Error::NotFound`] or a store failure.
    pub fn state(&self, id: &MessageId) -> Result<MessageState> {
        Ok(self.store.find(id)?.ok_or(Error::NotFound)?.state)
    }

    /// Up to `limit` events for producers after position `after`.
    ///
    /// # Errors
    /// A store failure.
    pub fn events_from(&self, after: u64, limit: usize) -> Result<Vec<(u64, ProducerEvent)>> {
        self.store.events_from(after, limit)
    }

    fn on_filler_receipt(
        &self,
        mut entry: OutboxEntry,
        receipt: &Receipt,
        now: u64,
    ) -> Result<ReceiptEffect> {
        match receipt.outcome {
            ReceiptOutcome::Acknowledged | ReceiptOutcome::Applied => {
                let Some(filler) = entry.filler.as_mut() else {
                    return Ok(ReceiptEffect::Ignored);
                };
                if filler.acknowledged {
                    return Ok(ReceiptEffect::Ignored);
                }
                filler.acknowledged = true;
                entry.state_at = entry.state_at.max(now);
                self.store.update(entry)?;
                Ok(ReceiptEffect::FillerConfirmed)
            }
            ReceiptOutcome::Rejected(_) => Ok(ReceiptEffect::Ignored),
        }
    }

    fn authenticate(&self, link: &Link, transport_peer: &Party, receipt: &Receipt) -> Result<()> {
        if receipt.link != link.id {
            return Err(Error::LinkMismatch);
        }
        let peer = link.peer().ok_or(Error::IdentityMismatch)?;
        if peer != transport_peer || peer != &receipt.signer {
            return Err(Error::IdentityMismatch);
        }
        let index = link.party_index(peer).ok_or(Error::IdentityMismatch)?;
        let key = link
            .peer_keys
            .get(index)
            .and_then(Option::as_ref)
            .ok_or(Error::UnknownKey)?;
        receipt.verify(key)
    }

    fn check_send(
        &self,
        link: &Link,
        topic: &Topic,
        now: u64,
        oracle: Option<&dyn RegionOracle>,
    ) -> std::result::Result<Grant, spot_tenant_link::Error> {
        let grant = match oracle {
            Some(oracle) => link.allows_with_region(topic, Direction::Send, now, oracle)?,
            None => {
                let grant = link.allows(topic, Direction::Send, now)?;
                if let Some(required) = &grant.require_peer_region {
                    return Err(spot_tenant_link::Error::PeerRegionMismatch {
                        required: required.to_string(),
                        found: "unknown".to_owned(),
                    });
                }
                grant
            }
        };
        Ok(grant.clone())
    }

    fn authorize(&self, request: &Submit<'_>, kind: TopicKind) -> Result<Grant> {
        let link = request.link;
        if link.local() != &self.signer.party() {
            return Err(Error::IdentityMismatch);
        }
        let refuse = |error: &Error| {
            audit::write(
                self.audit,
                request.actor,
                &Fact {
                    link: link.id,
                    kind: LinkEventKind::Refused,
                    at: request.now,
                    revision: link.version,
                    detail: error.code(),
                    action: "record a refused send",
                },
            )
        };
        let grant = match self.check_send(link, request.topic, request.now, request.oracle) {
            Ok(grant) => grant,
            Err(error) => {
                let error = Error::from(error);
                refuse(&error)?;
                return Err(error);
            }
        };
        let outcome = if grant.kind == kind {
            if grant.schema_range.contains(request.schema_version) {
                None
            } else {
                Some(Error::SchemaUnsupported)
            }
        } else {
            Some(Error::WrongKind {
                expected: grant.kind.as_str(),
            })
        };
        if let Some(error) = outcome {
            refuse(&error)?;
            return Err(error);
        }
        Ok(grant)
    }

    fn push(
        &self,
        request: &Submit<'_>,
        payload: String,
        withheld: Vec<String>,
    ) -> Result<Enqueued> {
        let link = request.link;
        let expires_at = self.lifetimes.expires_at(request.topic, request.now)?;
        audit::write(
            self.audit,
            request.actor,
            &Fact {
                link: link.id,
                kind: LinkEventKind::Allowed,
                at: request.now,
                revision: link.version,
                detail: "enqueued",
                action: "record an enqueued message",
            },
        )?;
        for _ in 0..ENQUEUE_ATTEMPTS {
            let seq = self
                .store
                .last_seq(&link.id, request.conversation)?
                .saturating_add(1);
            let message = Message::sign(
                MessageParts {
                    id: self.ids.next_id(request.now),
                    link: link.id,
                    conversation: request.conversation.clone(),
                    seq,
                    topic: request.topic.clone(),
                    schema_version: request.schema_version,
                    sent_at: request.now,
                    expires_at,
                    tombstone: false,
                    replaces: None,
                    payload: payload.clone(),
                },
                self.signer,
            )?;
            let id = message.id;
            let entry = OutboxEntry {
                message,
                state: MessageState::Queued,
                state_at: request.now,
                attempts: 0,
                last_sent_at: None,
                filler: None,
                version: 0,
            };
            match self.store.insert(entry) {
                Ok(()) => {
                    return Ok(Enqueued {
                        id,
                        seq,
                        expires_at,
                        withheld,
                    });
                }
                Err(Error::Conflict { .. }) => {}
                Err(error) => return Err(error),
            }
        }
        Err(Error::Busy)
    }

    fn filler_for(&self, original: &Message, now: u64) -> Result<Filler> {
        let message = Message::sign(
            MessageParts {
                id: self.ids.next_id(now),
                link: original.link,
                conversation: original.conversation.clone(),
                seq: original.seq,
                topic: original.topic.clone(),
                schema_version: original.schema_version,
                sent_at: now.max(original.sent_at.saturating_add(1)),
                expires_at: TOMBSTONE_EXPIRES,
                tombstone: true,
                replaces: Some(original.id),
                payload: TOMBSTONE_PAYLOAD.to_owned(),
            },
            self.signer,
        )?;
        Ok(Filler {
            message,
            acknowledged: false,
            last_sent_at: None,
        })
    }
}

fn next_state(current: MessageState, outcome: ReceiptOutcome) -> Option<MessageState> {
    use MessageState::{Acknowledged, Applied, Expired, Queued, Rejected, Sent};
    match (current, outcome) {
        (Queued | Sent, ReceiptOutcome::Acknowledged) => Some(Acknowledged),
        (Queued | Sent | Acknowledged | Expired, ReceiptOutcome::Applied) => Some(Applied),
        (Queued | Sent | Acknowledged, ReceiptOutcome::Rejected(code)) => Some(Rejected(code)),
        _ => None,
    }
}

fn event_of(message: &Message, kind: EventKind, at: u64) -> ProducerEvent {
    ProducerEvent {
        message: message.id,
        link: message.link,
        conversation: message.conversation.clone(),
        seq: message.seq,
        topic: message.topic.clone(),
        kind,
        at,
    }
}
