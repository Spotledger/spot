//! The receiver (ADR-058 rules 4, 9 and 14): verification, idempotence, order, parking.
//!
//! [`Inbox::receive_from`] never runs business logic and emits no document event. It
//!
//! 1. authenticates: the transport-bound peer is the link's peer, the message names this link,
//!    and its signature verifies against the key pinned in the link;
//! 2. answers a repeat of a known id with the earlier outcome (a lost receipt is repaired, the
//!    message is not applied twice);
//! 3. checks the grant (topic, direction, schema range, field list, expiry, size, canonical
//!    JSON); a refusal returns a rejection code, is audited and quarantined, and is never
//!    applied;
//! 4. orders by `(conversation, seq)`: in order is stored, ahead of a gap is parked, behind the
//!    cursor is refused;
//! 5. in one atomic commit stores the entries, advances the cursor and appends the delivery
//!    notices of `notify` topics.
//!
//! A *message* topic is applied by being stored. A *record* topic is stored `PendingApply`; the
//! host's apply path (`spot-replica-apply`) takes it through [`Inbox::pending_records`] and
//! reports [`Inbox::record_applied`] or [`Inbox::record_refused`].

use std::collections::BTreeSet;
use std::sync::Mutex;

use spot_replica_apply::ReceivedRecord;
use spot_sync_protocol::SourceId;
use spot_tenant_link::{
    AuditSink, Direction, ExchangePayload, Grant, IdentitySigner, Link, LinkEventKind, LinkId,
    LinkState, LinkStore, Party, RegionOracle, Topic, TopicKind,
};

use crate::audit::{self, Fact};
use crate::canonical;
use crate::envelope::Envelope;
use crate::error::{Error, Result};
use crate::ids::MessageId;
use crate::message::{Message, TOMBSTONE_EXPIRES, TOMBSTONE_PAYLOAD};
use crate::notice::NoticeDraft;
use crate::state::{Receipt, ReceiptOutcome, RejectCode};
use crate::store::{
    ConversationView, EntryStatus, InboxCommit, InboxEntry, InboxStore, LinkUsage, ParkedEntry,
    SeenKind,
};
use crate::transport::TransportParty;

const COMMIT_ATTEMPTS: usize = 4;
const MAX_LIFETIME_MS: u64 = 365 * 86_400_000;

/// A refused input, handed to the quarantine so an operator can see it.
#[derive(Clone, Debug)]
pub struct QuarantinedInput {
    /// The link it arrived on.
    pub link: LinkId,
    /// The message exactly as received (its `Debug` never prints the payload).
    pub message: Message,
    /// Why it was refused.
    pub code: RejectCode,
    /// Unix milliseconds.
    pub at: u64,
}

/// Where refused inputs go. `hold` is idempotent on the message id: holding the same message
/// twice leaves one item. If `hold` fails, the receive fails and nothing is acknowledged, so the
/// sender keeps the message.
///
/// `held_count` is what a caller bounds a partner by, so it is the quarantine's own answer and
/// never a count kept beside it: an input an operator released or deleted stops occupying a slot
/// as soon as it is gone, and a process that restarts over a full quarantine sees it full.
pub trait InputQuarantine: Send + Sync {
    /// Holds one refused input.
    ///
    /// # Errors
    /// [`Error::QuarantineFailed`] when it could not be held.
    fn hold(&self, input: QuarantinedInput) -> Result<()>;

    /// How many inputs this quarantine holds for `link`, counted at call time.
    ///
    /// # Errors
    /// [`Error::QuarantineCountUnavailable`] when the quarantine cannot count what it holds. The
    /// default answer is exactly that, for an implementer that predates the count: a caller
    /// bounds a partner by this number, so an unknown count must fail closed instead of reading
    /// as "nothing held".
    fn held_count(&self, _link: &LinkId) -> Result<usize> {
        Err(Error::QuarantineCountUnavailable)
    }

    /// Whether this quarantine already holds `input` for `link`, at the same scope the count
    /// takes.
    ///
    /// A re-hold of an input that is already there takes no second slot, so a caller can answer
    /// "held" for it even when the link is at its bound (see `LinkLimits::hold`). The default is
    /// [`Ok(false)`](Ok): an implementer that cannot answer is treated as **not** holding the
    /// input, so the bound still applies to it and the caller stays fail closed. Answering `true`
    /// without checking would let a redelivery past a bound the quarantine is really at.
    ///
    /// # Errors
    /// [`Error::QuarantineFailed`] when it could not look.
    fn holds(&self, _link: &LinkId, _input: &MessageId) -> Result<bool> {
        Ok(false)
    }
}

/// An in-memory [`InputQuarantine`].
#[derive(Default)]
pub struct MemoryQuarantine {
    held: Mutex<Vec<QuarantinedInput>>,
}

impl MemoryQuarantine {
    /// An empty quarantine.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// What is held, oldest first.
    #[must_use]
    pub fn held(&self) -> Vec<QuarantinedInput> {
        self.held.lock().map(|h| h.clone()).unwrap_or_default()
    }

    /// Releases one held input, as an operator who examined and deleted it would: it stops
    /// counting against its link's bound at once. Answers the item when it was held.
    pub fn release(&self, id: &MessageId) -> Option<QuarantinedInput> {
        let mut held = self.held.lock().ok()?;
        let at = held.iter().position(|input| input.message.id == *id)?;
        Some(held.remove(at))
    }

    /// How many inputs are held for `link` right now.
    #[must_use]
    pub fn count(&self, link: &LinkId) -> usize {
        self.held
            .lock()
            .map(|held| held.iter().filter(|input| input.link == *link).count())
            .unwrap_or(0)
    }
}

impl InputQuarantine for MemoryQuarantine {
    fn hold(&self, input: QuarantinedInput) -> Result<()> {
        let mut held = self.held.lock().map_err(|_| Error::QuarantineFailed)?;
        if !held.iter().any(|h| h.message.id == input.message.id) {
            held.push(input);
        }
        Ok(())
    }

    fn held_count(&self, link: &LinkId) -> Result<usize> {
        Ok(self.count(link))
    }

    fn holds(&self, link: &LinkId, input: &MessageId) -> Result<bool> {
        let held = self.held.lock().map_err(|_| Error::QuarantineFailed)?;
        Ok(held
            .iter()
            .any(|h| h.link == *link && h.message.id == *input))
    }
}

/// Receiver limits.
///
/// The three parking caps are **per link**, so a sender that opens many conversations cannot
/// push one link past its budget and cannot spend another link's budget either.
#[derive(Clone, Copy, Debug)]
pub struct InboxConfig {
    /// The furthest ahead of the cursor a message may be and still be parked.
    pub max_gap: u64,
    /// The most messages parked per conversation.
    pub max_parked: usize,
    /// The most messages parked for one link, across all its conversations.
    pub max_parked_per_link: usize,
    /// The most conversations one link may hold.
    pub max_conversations_per_link: usize,
    /// The most payload bytes one link may hold parked.
    pub max_parked_bytes_per_link: usize,
    /// How far ahead of the receiver's clock a `sent_at` may be.
    pub max_skew_ms: u64,
}

impl Default for InboxConfig {
    fn default() -> Self {
        Self {
            max_gap: 1_024,
            max_parked: 4_096,
            max_parked_per_link: 8_192,
            max_conversations_per_link: 1_024,
            max_parked_bytes_per_link: 16 * 1024 * 1024,
            max_skew_ms: 300_000,
        }
    }
}

/// What became of one received message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReceiveStatus {
    /// Stored in order (a message topic) or a slot filled.
    Applied,
    /// Stored in order; a record waiting for the apply path.
    PendingApply,
    /// Held behind a gap.
    Parked,
    /// Already known; the earlier outcome is repeated in the receipt.
    Duplicate,
    /// Refused with a code.
    Rejected(RejectCode),
    /// Not taken (limits); the sender keeps it and sends again. No receipt.
    Busy,
}

/// The outcome of a receive and the receipts to send back, including receipts for parked
/// messages this arrival released.
#[derive(Clone, Debug)]
pub struct ReceiveOutcome {
    /// What became of the message itself.
    pub status: ReceiveStatus,
    /// Receipts to return to the sender, signed by this side.
    pub receipts: Vec<Receipt>,
}

/// A record the apply path has yet to take.
#[derive(Clone, Debug)]
pub struct PendingRecord {
    /// The link it came on.
    pub link: LinkId,
    /// The stored entry.
    pub entry: InboxEntry,
}

/// The receiver side of a link.
pub struct Inbox<'a> {
    store: &'a dyn InboxStore,
    audit: &'a dyn AuditSink,
    quarantine: &'a dyn InputQuarantine,
    signer: &'a dyn IdentitySigner,
    /// Where the apply path re-reads a link's current state; `None` when the host did not wire
    /// one, in which case the caller's snapshot is not re-checked.
    links: Option<&'a dyn LinkStore>,
    /// Where a grant that requires a peer region is judged; `None` refuses every such grant
    /// (ADR-058 rule 6, fail closed).
    region: Option<&'a dyn RegionOracle>,
    config: InboxConfig,
}

/// Everything one receive plans, before any side effect.
struct Plan {
    commit: InboxCommit,
    receipts: Vec<Receipt>,
    rejections: Vec<(Message, RejectCode)>,
    accepted: Vec<MessageId>,
}

impl<'a> Inbox<'a> {
    /// An inbox over `store`, answering as `signer`.
    #[must_use]
    pub fn new(
        store: &'a dyn InboxStore,
        audit: &'a dyn AuditSink,
        quarantine: &'a dyn InputQuarantine,
        signer: &'a dyn IdentitySigner,
    ) -> Self {
        Self {
            store,
            audit,
            quarantine,
            signer,
            links: None,
            region: None,
            config: InboxConfig::default(),
        }
    }

    /// Replaces the limits.
    #[must_use]
    pub fn with_config(mut self, config: InboxConfig) -> Self {
        self.config = config;
        self
    }

    /// Where the apply path re-reads a link before it marks a record applied: a link
    /// revoked while the record sat in flight is refused instead of being recorded as applied.
    #[must_use]
    pub fn with_links(mut self, links: &'a dyn LinkStore) -> Self {
        self.links = Some(links);
        self
    }

    /// The oracle that places the peer, for a grant that requires the peer to sit in a region
    /// (ADR-058 rule 6). Without one, a message on such a grant is refused as not granted; an
    /// oracle that does not know the peer refuses too. The oracle is asked about the receiving
    /// side's own view of the peer, never about a region carried in the message.
    #[must_use]
    pub fn with_region_oracle(mut self, oracle: &'a dyn RegionOracle) -> Self {
        self.region = Some(oracle);
        self
    }

    /// The oracle set by [`Inbox::with_region_oracle`], so a gate in front of this inbox judges a
    /// grant with the same answer.
    #[must_use]
    pub fn region_oracle(&self) -> Option<&'a dyn RegionOracle> {
        self.region
    }

    /// The grant that lets `link` receive `topic` at `now`, with the peer-region requirement
    /// judged by this inbox's oracle (or refused when there is none).
    ///
    /// # Errors
    /// Whatever [`Link::allows`] or [`Link::allows_with_region`] refuses with.
    pub fn receive_grant<'l>(
        &self,
        link: &'l Link,
        topic: &Topic,
        now: u64,
    ) -> std::result::Result<&'l Grant, spot_tenant_link::Error> {
        match self.region {
            Some(oracle) => link.allows_with_region(topic, Direction::Receive, now, oracle),
            None => link.allows(topic, Direction::Receive, now),
        }
    }

    /// Receives one message delivered by the carrier, from the party the connection
    /// authenticated.
    ///
    /// This is the entry point a carrier adapter uses: [`TransportParty`] can only be built from
    /// an identity the adapter verified itself, so a party read out of the message cannot be
    /// passed here. A message signed by another party than the connection is still refused by
    /// [`Inbox::authenticate`].
    ///
    /// # Errors
    /// As [`Inbox::receive_from`]'s inner call.
    ///
    /// The bare-`Party` call does not compile:
    ///
    /// ```compile_fail
    /// use spot_link_exchange::{Inbox, Message};
    /// use spot_tenant_link::{Link, Party};
    ///
    /// fn delivers(inbox: &Inbox<'_>, link: &Link, party: &Party, message: &Message) {
    ///     // A `&Party` is not a `TransportParty`: who authenticated this is not the caller's
    ///     // word.
    ///     let _ = inbox.receive_from(link, party, message, 0);
    /// }
    /// ```
    ///
    /// Nor does the old bare entry point exist any more:
    ///
    /// ```compile_fail
    /// use spot_link_exchange::{Inbox, Message};
    /// use spot_tenant_link::{Link, Party};
    ///
    /// fn delivers(inbox: &Inbox<'_>, link: &Link, party: &Party, message: &Message) {
    ///     let _ = inbox.receive(link, party, message, 0);
    /// }
    /// ```
    pub fn receive_from(
        &self,
        link: &Link,
        transport: &TransportParty,
        message: &Message,
        now: u64,
    ) -> Result<ReceiveOutcome> {
        self.receive(link, transport.party(), message, now)
    }

    /// Receives one message delivered by the carrier.
    ///
    /// `transport_peer` is the party the transport authenticated (mTLS identity or pinned
    /// fingerprint of the connection), never a value read from the message.
    ///
    /// Private on purpose: the only way in is [`Inbox::receive_from`], which cannot be
    /// handed a party read out of the message.
    ///
    /// # Errors
    /// [`Error::LinkMismatch`], [`Error::IdentityMismatch`], [`Error::UnknownKey`] or
    /// [`Error::BadSignature`] for an input that cannot be attributed to the peer (audited, no
    /// receipt: a forger gets nothing back); an audit, quarantine or store failure.
    fn receive(
        &self,
        link: &Link,
        transport_peer: &Party,
        message: &Message,
        now: u64,
    ) -> Result<ReceiveOutcome> {
        if let Err(error) = self.authenticate(link, transport_peer, message) {
            audit::write(
                self.audit,
                "system",
                &Fact {
                    link: link.id,
                    kind: LinkEventKind::Refused,
                    at: now,
                    revision: link.version,
                    detail: error.code(),
                    action: "record a refused delivery",
                },
            )?;
            return Err(error);
        }
        if let Some(kind) = self.store.seen(&link.id, &message.id)? {
            return self.on_seen(message, kind, now);
        }
        for _ in 0..COMMIT_ATTEMPTS {
            let attempt = match self.validate(link, message, now) {
                Ok(_) => self.sequence(link, message, now),
                Err(code) => self.refuse(link, message, code, now),
            };
            match attempt {
                Err(Error::Conflict { .. }) => {}
                other => return other,
            }
        }
        Ok(ReceiveOutcome {
            status: ReceiveStatus::Busy,
            receipts: Vec::new(),
        })
    }

    /// Refuses every parked message that expired while waiting, and returns the receipts to
    /// send. Run it from the host's periodic job.
    ///
    /// # Errors
    /// An audit, quarantine or store failure.
    pub fn sweep_expired(&self, now: u64) -> Result<Vec<Receipt>> {
        let mut receipts = Vec::new();
        for (link, parked) in self.store.parked()? {
            let message = parked.message;
            if message.tombstone || message.expires_at > now {
                continue;
            }
            let view = self.store.conversation(&link, &message.conversation)?;
            let mut plan = Plan::new(InboxCommit::new(
                link,
                message.conversation.clone(),
                view.next_seq,
            ));
            plan.commit.unpark.push(message.id);
            plan.reject(self, &message, RejectCode::OutOfOrderExpired, now);
            match self.finish(plan, now) {
                Ok(outcome) => receipts.extend(outcome),
                Err(Error::Conflict { .. }) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(receipts)
    }

    /// Records waiting for the apply path, in order within each conversation.
    ///
    /// # Errors
    /// A store failure.
    pub fn pending_records(&self, limit: usize) -> Result<Vec<PendingRecord>> {
        Ok(self
            .store
            .pending_records(limit)?
            .into_iter()
            .map(|(link, entry)| PendingRecord { link, entry })
            .collect())
    }

    /// The record the apply path of `spot-replica-apply` takes, from a pending entry.
    ///
    /// # Errors
    /// [`Error::WrongKind`] for a non-record entry, [`Error::InvalidPayload`] for a bad image.
    pub fn received_record(&self, entry: &InboxEntry, source: &SourceId) -> Result<ReceivedRecord> {
        let envelope = Envelope::decode(&entry.message.payload)?;
        envelope.into_received(
            source,
            entry.message.seq,
            &entry.message.topic,
            &entry.message.conversation,
        )
    }

    /// The apply path applied a record: marks it, appends its delivery notice (if the topic
    /// notifies) in the same transaction, and returns the `Applied` receipt. `None` when the
    /// record was no longer pending.
    ///
    /// With a [`LinkStore`] wired through [`Inbox::with_links`], the link is re-read here and the
    /// record is refused, and **not** marked, when the link is no longer active: a link revoked
    /// while the record sat in flight never produces an applied row. The refusal leaves
    /// the entry pending, which is a state the retention sweep closes.
    ///
    /// # Errors
    /// [`Error::NotFound`], [`Error::LinkMismatch`], [`spot_tenant_link::Error::NotActive`] when
    /// the link was revoked or expired meanwhile, an audit or store failure.
    pub fn record_applied(&self, link: &Link, id: &MessageId, now: u64) -> Result<Option<Receipt>> {
        let entry = self.pending_entry(link, id)?;
        self.check_active(link, now)?;
        let notify = link
            .grant(&entry.message.topic)
            .is_some_and(|grant| grant.notify);
        let notice = if notify {
            Some(NoticeDraft {
                link: link.id,
                topic: entry.message.topic.clone(),
                message_id: entry.message.id,
                conversation: entry.message.conversation.clone(),
                source: link.peer().cloned().ok_or(Error::IdentityMismatch)?,
                received_at: now,
            })
        } else {
            None
        };
        self.audit_fact(
            link,
            now,
            LinkEventKind::Allowed,
            "applied",
            "record an applied record",
        )?;
        let done = self
            .store
            .finish_record(id, &link.id, EntryStatus::Applied, notice)?;
        Ok(done.map(|entry| self.receipt(&entry.message, ReceiptOutcome::Applied, now)))
    }

    /// The apply path refused a record: marks it failed and returns the `Rejected` receipt.
    ///
    /// # Errors
    /// [`Error::NotFound`], [`Error::LinkMismatch`], an audit, quarantine or store failure.
    pub fn record_refused(&self, link: &Link, id: &MessageId, now: u64) -> Result<Option<Receipt>> {
        let entry = self.pending_entry(link, id)?;
        let code = RejectCode::PayloadInvalid;
        self.audit_fact(
            link,
            now,
            LinkEventKind::Refused,
            code.code(),
            "record a refused record",
        )?;
        self.quarantine.hold(QuarantinedInput {
            link: link.id,
            message: entry.message.clone(),
            code,
            at: now,
        })?;
        let done = self
            .store
            .finish_record(id, &link.id, EntryStatus::Failed(code), None)?;
        Ok(done.map(|entry| self.receipt(&entry.message, ReceiptOutcome::Rejected(code), now)))
    }

    fn pending_entry(&self, link: &Link, id: &MessageId) -> Result<InboxEntry> {
        self.store.entry(&link.id, id)?.ok_or(Error::NotFound)
    }

    /// Re-reads the link's current state and refuses when it is no longer active. A
    /// host that wired no [`LinkStore`] is answered on its own snapshot, as before.
    fn check_active(&self, link: &Link, now: u64) -> Result<()> {
        let Some(links) = self.links else {
            return Ok(());
        };
        let current = links.get(&link.id)?;
        if current.state == LinkState::Active {
            return Ok(());
        }
        let error = Error::Link(spot_tenant_link::Error::NotActive {
            link: link.id.to_string(),
            state: current.state,
        });
        self.audit_fact(
            link,
            now,
            LinkEventKind::Refused,
            error.code(),
            "record an applied record",
        )?;
        Err(error)
    }

    fn authenticate(&self, link: &Link, transport_peer: &Party, message: &Message) -> Result<()> {
        if message.link != link.id {
            return Err(Error::LinkMismatch);
        }
        let peer = link.peer().ok_or(Error::IdentityMismatch)?;
        if peer != transport_peer || peer != &message.sender {
            return Err(Error::IdentityMismatch);
        }
        let index = link.party_index(peer).ok_or(Error::IdentityMismatch)?;
        let key = link
            .peer_keys
            .get(index)
            .and_then(Option::as_ref)
            .ok_or(Error::UnknownKey)?;
        message.verify(key)
    }

    fn on_seen(&self, message: &Message, kind: SeenKind, now: u64) -> Result<ReceiveOutcome> {
        let outcome = match kind {
            SeenKind::Stored(EntryStatus::Applied) => ReceiptOutcome::Applied,
            SeenKind::Stored(EntryStatus::PendingApply) | SeenKind::Parked => {
                ReceiptOutcome::Acknowledged
            }
            SeenKind::Stored(EntryStatus::Failed(code)) | SeenKind::Rejected(code) => {
                ReceiptOutcome::Rejected(code)
            }
        };
        Ok(ReceiveOutcome {
            status: ReceiveStatus::Duplicate,
            receipts: vec![self.receipt(message, outcome, now)],
        })
    }

    /// Everything about a message that depends on the grant and the message alone.
    fn validate(
        &self,
        link: &Link,
        m: &Message,
        now: u64,
    ) -> std::result::Result<Grant, RejectCode> {
        let grant = self
            .receive_grant(link, &m.topic, now)
            .map_err(|_| RejectCode::NotGranted)?
            .clone();
        let lifetime = m.expires_at.saturating_sub(m.sent_at);
        let sane = m.seq > 0
            && m.expires_at > m.sent_at
            && m.sent_at <= now.saturating_add(self.config.max_skew_ms);
        if !sane {
            return Err(RejectCode::PayloadInvalid);
        }
        if m.replaces.is_some() && !m.tombstone {
            return Err(RejectCode::PayloadInvalid);
        }
        if m.tombstone {
            return if m.payload == TOMBSTONE_PAYLOAD && m.expires_at == TOMBSTONE_EXPIRES {
                Ok(grant)
            } else {
                Err(RejectCode::PayloadInvalid)
            };
        }
        if !grant.schema_range.contains(m.schema_version) {
            return Err(RejectCode::VersionUnsupported);
        }
        if lifetime > MAX_LIFETIME_MS || !canonical::is_canonical(&m.payload) {
            return Err(RejectCode::PayloadInvalid);
        }
        if now >= m.expires_at {
            return Err(RejectCode::OutOfOrderExpired);
        }
        let within = match grant.kind {
            TopicKind::Message => serde_json::from_str::<ExchangePayload>(&m.payload)
                .is_ok_and(|payload| payload.iter().all(|(name, _)| grant.fields.includes(name))),
            TopicKind::Record => Envelope::decode(&m.payload)
                .is_ok_and(|e| e.within(&grant.fields) && e.validate().is_ok()),
        };
        if within {
            Ok(grant)
        } else {
            Err(RejectCode::PayloadInvalid)
        }
    }

    /// Whether `m` would push its link past one of the per-link caps when parked. Only parking
    /// is capped: a message that arrives in order is stored and costs the link nothing.
    fn over_capacity(&self, link: &Link, m: &Message, view: &ConversationView) -> Result<bool> {
        let usage: LinkUsage = self.store.usage(&link.id)?;
        let fresh = view.next_seq <= 1 && view.parked.is_empty();
        let bytes = usage
            .parked_bytes
            .saturating_add(u64::try_from(m.payload.len()).unwrap_or(u64::MAX));
        Ok(usage.parked >= self.config.max_parked_per_link as u64
            || bytes > self.config.max_parked_bytes_per_link as u64
            || (fresh && usage.conversations >= self.config.max_conversations_per_link as u64))
    }

    /// Refuses a message the link has no room for: a receipt with the stable code, an audit row
    /// and a quarantine item (idempotent on the message id), and **nothing stored** — no entry,
    /// no parking slot and no `seen` row, so the message is not answered as known later on.
    fn refuse_unstored(
        &self,
        link: &Link,
        m: &Message,
        code: RejectCode,
        now: u64,
    ) -> Result<ReceiveOutcome> {
        let mut plan = Plan::new(InboxCommit::new(link.id, m.conversation.clone(), 1));
        plan.reject(self, m, code, now);
        let receipts = self.finish_unrecorded(plan, now)?;
        Ok(ReceiveOutcome {
            status: ReceiveStatus::Rejected(code),
            receipts,
        })
    }

    fn refuse(
        &self,
        link: &Link,
        m: &Message,
        code: RejectCode,
        now: u64,
    ) -> Result<ReceiveOutcome> {
        let view = self.store.conversation(&link.id, &m.conversation)?;
        let mut plan = Plan::new(InboxCommit::new(
            link.id,
            m.conversation.clone(),
            view.next_seq,
        ));
        plan.reject(self, m, code, now);
        let receipts = self.finish(plan, now)?;
        Ok(ReceiveOutcome {
            status: ReceiveStatus::Rejected(code),
            receipts,
        })
    }

    fn sequence(&self, link: &Link, m: &Message, now: u64) -> Result<ReceiveOutcome> {
        let view = self.store.conversation(&link.id, &m.conversation)?;
        let next = view.next_seq;
        let mut plan = Plan::new(InboxCommit::new(link.id, m.conversation.clone(), next));
        if m.seq < next {
            if m.tombstone {
                plan.receipts
                    .push(self.receipt(m, ReceiptOutcome::Applied, now));
                if let Some(original) = self.replaced_outcome(link, m, now)? {
                    plan.receipts.push(original);
                }
                let receipts = self.finish(plan, now)?;
                return Ok(ReceiveOutcome {
                    status: ReceiveStatus::Applied,
                    receipts,
                });
            }
            return self.refuse_with(plan, m, RejectCode::OutOfOrderExpired, now);
        }
        if view.parked.iter().any(|p| p.message.seq == m.seq) {
            if m.tombstone {
                // The slot's original is still parked here (the sender expired it before this
                // side did). Refusing the tombstone would record it as seen-and-rejected for
                // good, and the slot could then never be filled once the parked original
                // expires. Take nothing, send no receipt: the sender resends it later.
                return Ok(ReceiveOutcome {
                    status: ReceiveStatus::Busy,
                    receipts: Vec::new(),
                });
            }
            return self.refuse_with(plan, m, RejectCode::PayloadInvalid, now);
        }
        if m.seq > next {
            let ahead = m.seq - next;
            if ahead > self.config.max_gap || view.parked.len() >= self.config.max_parked {
                return Ok(ReceiveOutcome {
                    status: ReceiveStatus::Busy,
                    receipts: Vec::new(),
                });
            }
            if self.over_capacity(link, m, &view)? {
                return self.refuse_unstored(link, m, RejectCode::CapacityExceeded, now);
            }
            plan.commit.park.push(ParkedEntry {
                message: m.clone(),
                parked_at: now,
            });
            plan.receipts
                .push(self.receipt(m, ReceiptOutcome::Acknowledged, now));
            plan.accepted.push(m.id);
            let receipts = self.finish(plan, now)?;
            return Ok(ReceiveOutcome {
                status: ReceiveStatus::Parked,
                receipts,
            });
        }
        let status = self.drain(link, m, &view.parked, now, &mut plan);
        let receipts = self.finish(plan, now)?;
        Ok(ReceiveOutcome { status, receipts })
    }

    /// What became of the message a tombstone replaces, as a receipt for that message: this is
    /// how a sender that expired a message learns it was applied after all.
    fn replaced_outcome(
        &self,
        link: &Link,
        tombstone: &Message,
        now: u64,
    ) -> Result<Option<Receipt>> {
        let Some(original) = tombstone.replaces else {
            return Ok(None);
        };
        let Some(kind) = self.store.seen(&link.id, &original)? else {
            return Ok(None);
        };
        let outcome = match kind {
            SeenKind::Stored(EntryStatus::Applied) => ReceiptOutcome::Applied,
            SeenKind::Stored(EntryStatus::PendingApply) | SeenKind::Parked => {
                ReceiptOutcome::Acknowledged
            }
            SeenKind::Stored(EntryStatus::Failed(code)) | SeenKind::Rejected(code) => {
                ReceiptOutcome::Rejected(code)
            }
        };
        Ok(Some(Receipt::sign(
            self.signer,
            tombstone.link,
            original,
            tombstone.conversation.clone(),
            tombstone.seq,
            outcome,
            now,
        )))
    }

    /// Takes `first` (at the cursor) and every parked message that becomes contiguous.
    fn drain(
        &self,
        link: &Link,
        first: &Message,
        parked: &[ParkedEntry],
        now: u64,
        plan: &mut Plan,
    ) -> ReceiveStatus {
        let mut status = ReceiveStatus::Applied;
        let mut current = Some((first.clone(), false));
        let mut cursor = plan.commit.expected_next;
        while let Some((m, from_parked)) = current.take() {
            let is_first = m.id == first.id;
            if from_parked {
                plan.commit.unpark.push(m.id);
            }
            match self.validate(link, &m, now) {
                Ok(grant) => {
                    let result = self.take(link, &m, &grant, now, plan);
                    if is_first {
                        status = result;
                    }
                    cursor += 1;
                }
                Err(code) => {
                    plan.reject(self, &m, code, now);
                    if is_first {
                        status = ReceiveStatus::Rejected(code);
                    }
                    break;
                }
            }
            current = parked
                .iter()
                .find(|p| p.message.seq == cursor)
                .map(|p| (p.message.clone(), true));
        }
        plan.commit.next_seq = cursor;
        status
    }

    fn take(
        &self,
        link: &Link,
        m: &Message,
        grant: &Grant,
        now: u64,
        plan: &mut Plan,
    ) -> ReceiveStatus {
        if m.tombstone {
            plan.receipts
                .push(self.receipt(m, ReceiptOutcome::Applied, now));
            plan.accepted.push(m.id);
            return ReceiveStatus::Applied;
        }
        let (status, outcome, entry_status) = match grant.kind {
            TopicKind::Message => (
                ReceiveStatus::Applied,
                ReceiptOutcome::Applied,
                EntryStatus::Applied,
            ),
            TopicKind::Record => (
                ReceiveStatus::PendingApply,
                ReceiptOutcome::Acknowledged,
                EntryStatus::PendingApply,
            ),
        };
        plan.commit.store.push(InboxEntry {
            message: m.clone(),
            received_at: now,
            status: entry_status,
        });
        if grant.notify && grant.kind == TopicKind::Message {
            plan.commit.notices.push(NoticeDraft {
                link: link.id,
                topic: m.topic.clone(),
                message_id: m.id,
                conversation: m.conversation.clone(),
                source: link.peer().cloned().unwrap_or_else(|| m.sender.clone()),
                received_at: now,
            });
        }
        plan.receipts.push(self.receipt(m, outcome, now));
        plan.accepted.push(m.id);
        status
    }

    fn refuse_with(
        &self,
        mut plan: Plan,
        m: &Message,
        code: RejectCode,
        now: u64,
    ) -> Result<ReceiveOutcome> {
        plan.reject(self, m, code, now);
        let receipts = self.finish(plan, now)?;
        Ok(ReceiveOutcome {
            status: ReceiveStatus::Rejected(code),
            receipts,
        })
    }

    /// Audit, then quarantine, then the atomic commit. A failure at any step stops the later
    /// ones, so nothing is stored that was not audited and nothing is acknowledged that was not
    /// held.
    fn finish(&self, plan: Plan, now: u64) -> Result<Vec<Receipt>> {
        self.side_effects(&plan, now)?;
        self.store.commit(plan.commit)?;
        Ok(plan.receipts)
    }

    /// As [`Inbox::finish`] for a rejection that must not be recorded against the id (the id
    /// belongs to another link).
    fn finish_unrecorded(&self, mut plan: Plan, now: u64) -> Result<Vec<Receipt>> {
        plan.commit.rejected.clear();
        self.side_effects(&plan, now)?;
        Ok(plan.receipts)
    }

    fn side_effects(&self, plan: &Plan, now: u64) -> Result<()> {
        let link = plan.commit.link;
        for _ in &plan.accepted {
            self.audit_fact(
                &link,
                now,
                LinkEventKind::Allowed,
                "received",
                "record a received message",
            )?;
        }
        let mut done = BTreeSet::new();
        for (message, code) in &plan.rejections {
            self.audit_fact(
                &link,
                now,
                LinkEventKind::Refused,
                code.code(),
                "record a rejected message",
            )?;
            if done.insert(message.id) {
                self.quarantine.hold(QuarantinedInput {
                    link,
                    message: message.clone(),
                    code: *code,
                    at: now,
                })?;
            }
        }
        Ok(())
    }

    fn audit_fact(
        &self,
        link: &impl LinkRef,
        now: u64,
        kind: LinkEventKind,
        detail: &'static str,
        action: &'static str,
    ) -> Result<()> {
        audit::write(
            self.audit,
            "system",
            &Fact {
                link: link.link_id(),
                kind,
                at: now,
                revision: link.revision(),
                detail,
                action,
            },
        )
    }

    fn receipt(&self, m: &Message, outcome: ReceiptOutcome, now: u64) -> Receipt {
        Receipt::sign(
            self.signer,
            m.link,
            m.id,
            m.conversation.clone(),
            m.seq,
            outcome,
            now,
        )
    }
}

impl Plan {
    fn new(commit: InboxCommit) -> Self {
        Self {
            commit,
            receipts: Vec::new(),
            rejections: Vec::new(),
            accepted: Vec::new(),
        }
    }

    fn reject(&mut self, inbox: &Inbox<'_>, m: &Message, code: RejectCode, now: u64) {
        self.commit.rejected.push((m.id, self.commit.link, code));
        self.rejections.push((m.clone(), code));
        self.receipts
            .push(inbox.receipt(m, ReceiptOutcome::Rejected(code), now));
    }
}

/// What the audit helper needs of a link reference.
trait LinkRef {
    fn link_id(&self) -> LinkId;
    fn revision(&self) -> u64;
}

impl LinkRef for Link {
    fn link_id(&self) -> LinkId {
        self.id
    }
    fn revision(&self) -> u64 {
        self.version
    }
}

impl LinkRef for LinkId {
    fn link_id(&self) -> LinkId {
        *self
    }
    fn revision(&self) -> u64 {
        0
    }
}
