//! The receiving gate: conformance of every incoming message, before the inbox takes it.
//!
//! [`ConformantInbox`] wraps `spot_link_exchange::Inbox::receive`. The inbox checks the link, the
//! grant and the order of a stream; this gate adds the contract (ADR-080 section 5.6): the message
//! must be a known one, sent by the role the link's peer plays, allowed after the previous
//! message of this conversation, and its payload must fit the field list.
//!
//! A message that fails is **refused before the inbox sees it**: the sender gets a signed
//! rejection receipt carrying a reason code, and the message is held in the input quarantine for
//! a person. Refusing first is what lets the sender learn of it; once the inbox has stored a
//! message it is applied for good. A message ahead of a gap in its stream is not judged yet: the
//! gate takes nothing and sends no receipt, and the sender's resend delivers it after the gap
//! closes (the contract order can only be judged against the messages before it).
//!
//! Nothing here trusts the unauthenticated: a message that does not verify against the link goes
//! straight to the inbox, which audits and drops it, so a forger gets no receipt and no
//! quarantine entry.
//!
//! # The record is caught up before a message is judged
//!
//! The inbox stores a message and appends its delivery notice in one commit; the conversation
//! record is a second store, so the two cannot commit together and a crash between them leaves
//! the record one message behind. Judging an in-order message against that stale record refuses
//! a valid follow-up (and the refusal is remembered), so the gate first records every notice the
//! inbox already holds, from the notice feed the host gives it ([`ConformantInbox::with_ledger`]).
//! With no feed the gate judges what the record says, as before.

use spot_link_exchange::{
    Error as ExchangeError, Inbox, InboxStore, InputQuarantine, MAX_PAYLOAD_BYTES, Message,
    NoticeFeed, QuarantinedInput, Receipt, ReceiptOutcome, ReceiveOutcome, ReceiveStatus,
    TransportParty, is_canonical,
};
use spot_tenant_link::{IdentitySigner, Link, Party};

use crate::audit::Refusal;
use crate::error::{ConversationError, Violation, codes};
use crate::ledger::{record_received, record_sent, repair};
use crate::order::{check_entry, check_message};
use crate::record::WaitKind;
use crate::runtime::Runtime;
use crate::wire::{Decoded, decode};

/// Why a receive failed outright (as opposed to refusing the message with a reason).
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ReceiveError {
    /// The link exchange failed: an input that cannot be attributed to the peer, or a store,
    /// audit or quarantine failure. Nothing was acknowledged; the sender keeps the message.
    #[error("the link exchange failed ({code})")]
    Exchange {
        /// The link exchange's stable code.
        code: &'static str,
    },
    /// The conversation store failed.
    #[error("the conversation record failed ({code})")]
    Conversation {
        /// This crate's stable code.
        code: &'static str,
    },
}

impl ReceiveError {
    /// The stable code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Exchange { code } | Self::Conversation { code } => code,
        }
    }
}

impl From<ExchangeError> for ReceiveError {
    fn from(error: ExchangeError) -> Self {
        Self::Exchange { code: error.code() }
    }
}

impl From<ConversationError> for ReceiveError {
    fn from(error: ConversationError) -> Self {
        Self::Conversation { code: error.code() }
    }
}

/// The inbox with the contract in front of it.
pub struct ConformantInbox<'a> {
    inbox: &'a Inbox<'a>,
    inbox_store: &'a dyn InboxStore,
    quarantine: &'a dyn InputQuarantine,
    signer: &'a dyn IdentitySigner,
    runtime: &'a Runtime,
    ledger: Option<&'a dyn NoticeFeed>,
}

impl<'a> ConformantInbox<'a> {
    /// A gate over `inbox`, quarantining refusals into `quarantine` and signing rejection
    /// receipts as `signer` (the same identity the inbox answers as).
    #[must_use]
    pub fn new(
        inbox: &'a Inbox<'a>,
        inbox_store: &'a dyn InboxStore,
        quarantine: &'a dyn InputQuarantine,
        signer: &'a dyn IdentitySigner,
        runtime: &'a Runtime,
    ) -> Self {
        Self {
            inbox,
            inbox_store,
            quarantine,
            signer,
            runtime,
            ledger: None,
        }
    }

    /// The same gate with the delivery notice feed, so it records a predecessor the crash window
    /// left unrecorded before it judges anything.
    #[must_use]
    pub fn with_ledger(mut self, notes: &'a dyn NoticeFeed) -> Self {
        self.ledger = Some(notes);
        self
    }

    /// Records the notices the inbox already holds that the conversation record does not: the
    /// gap a crash between the inbox commit and the record write leaves.
    fn catch_up(&self) -> Result<(), ReceiveError> {
        if let Some(notes) = self.ledger {
            repair(self.runtime, notes, self.inbox_store)?;
        }
        Ok(())
    }

    /// Writes the one row a refusal leaves: its reason code and ids, never a payload.
    fn audit(
        &self,
        link: &Link,
        message: &Message,
        code: &'static str,
        now_ms: u64,
    ) -> Result<(), ReceiveError> {
        self.runtime.audit.refused(&Refusal {
            link: link.id,
            conversation: message.conversation.clone(),
            message_id: message.id,
            code,
            at_ms: now_ms,
        })?;
        Ok(())
    }

    /// Receives one message delivered by the carrier.
    ///
    /// `transport` is the party the carrier authenticated for the connection, never a value read
    /// from the message: only a carrier adapter can build one.
    ///
    /// # Errors
    /// [`ReceiveError`] when the link exchange or the conversation store fails.
    pub fn receive_from(
        &self,
        link: &Link,
        transport: &TransportParty,
        message: &Message,
        now_ms: u64,
    ) -> Result<ReceiveOutcome, ReceiveError> {
        if message.tombstone || !authenticated(link, transport.party(), message) {
            return Ok(self.inbox.receive_from(link, transport, message, now_ms)?);
        }
        if let Some(code) = self.runtime.store.rejection(&link.id, &message.id)? {
            return Ok(self.rejection(link, message, code, now_ms));
        }
        if self.inbox_store.seen(&link.id, &message.id)?.is_some() {
            let outcome = self.inbox.receive_from(link, transport, message, now_ms)?;
            self.record_if_applied(link, message, &outcome, now_ms)?;
            return Ok(outcome);
        }
        let view = self
            .inbox_store
            .conversation(&link.id, &message.conversation)?;
        if message.seq > view.next_seq {
            return Ok(ReceiveOutcome {
                status: ReceiveStatus::Busy,
                receipts: Vec::new(),
            });
        }
        if message.seq < view.next_seq {
            return Ok(self.inbox.receive_from(link, transport, message, now_ms)?);
        }
        // The inbox's own cheap preconditions come first: a message that fails one is parsed by
        // nobody here, and goes down the inbox's bounded reject, audit and quarantine path.
        if !within_inbox_limits(self.inbox, link, message, now_ms) {
            return Ok(self.inbox.receive_from(link, transport, message, now_ms)?);
        }
        // The record is written after the inbox commits, so it can be one message behind: catch
        // it up before judging, or a valid follow-up is refused against a stale history.
        self.catch_up()?;
        // A crash between the send and the record write leaves a reply to a message the record
        // never wrote down; record it before the reply is judged.
        record_sent(self.runtime, &link.id, &message.conversation, now_ms)?;
        // A partner may not make this tenant hold conversations without limit. Checked after the
        // catch-up, so a conversation that already exists but was not recorded yet is not counted
        // as a new one.
        if self.would_open(link, message)? {
            return self.refuse_bound(link, message, codes::LINK_CONVERSATION_LIMIT, now_ms);
        }
        match self.judge(link, message) {
            Ok(_) => {
                let outcome = self.inbox.receive_from(link, transport, message, now_ms)?;
                self.record_if_applied(link, message, &outcome, now_ms)?;
                Ok(outcome)
            }
            Err(violation) => self.refuse(link, message, &violation, now_ms),
        }
    }

    /// The contract's verdict on an in-order message from the authenticated peer.
    fn judge(&self, link: &Link, message: &Message) -> Result<Decoded, Violation> {
        let decoded = decode(&message.payload)?;
        let header = &decoded.header;
        let record = self
            .runtime
            .store
            .load(&link.id, &message.conversation)
            .map_err(|_| Violation::ContractUnknown)?;
        let contract = match &record {
            Some(record) => {
                if record.contract != header.contract || record.contract_version != header.version {
                    return Err(Violation::ContractUnknown);
                }
                self.runtime
                    .book
                    .get(&record.contract, record.contract_version)
            }
            None => self.runtime.book.get(&header.contract, header.version),
        }
        .ok_or(Violation::ContractUnknown)?;
        let my_role = self
            .runtime
            .book
            .role(&contract.id)
            .ok_or(Violation::ContractUnknown)?;
        let peer_role = self
            .runtime
            .peers
            .peer_role(&link.id, &contract.id)
            .ok_or(Violation::WrongPeer)?;
        let def = contract
            .message(&header.message)
            .ok_or(Violation::MessageUnknown)?;
        if def.from != peer_role {
            return Err(Violation::WrongPeer);
        }
        if def.topic != message.topic.as_str() {
            return Err(Violation::MessageUnknown);
        }
        match &record {
            Some(record) => {
                check_message(
                    contract,
                    &peer_role,
                    my_role,
                    &header.message,
                    message.schema_version,
                    &decoded.payload,
                    &record.order_state(),
                )?;
            }
            None => {
                check_entry(contract, &peer_role, &header.message)?;
                let counts = std::collections::BTreeMap::new();
                check_message(
                    contract,
                    &peer_role,
                    my_role,
                    &header.message,
                    message.schema_version,
                    &decoded.payload,
                    &crate::order::OrderState {
                        last: None,
                        counts: &counts,
                        ended: false,
                    },
                )?;
            }
        }
        Ok(decoded)
    }

    /// Whether this message would open a conversation the link's bound no longer allows.
    fn would_open(&self, link: &Link, message: &Message) -> Result<bool, ReceiveError> {
        if self
            .runtime
            .store
            .load(&link.id, &message.conversation)?
            .is_some()
        {
            return Ok(false);
        }
        let open = self.runtime.store.open_on(&link.id)?;
        Ok(!self.runtime.limits.may_open(open))
    }

    /// Holds a refused input, or writes down that the link's quarantine is full and drops it.
    ///
    /// How much the link already holds is the quarantine's own count, taken by
    /// [`LinkLimits::hold`], so an input an operator released frees its slot and a restarted
    /// process sees a full quarantine as full. A quarantine that cannot count is treated as
    /// full (fail closed): the input is dropped and the drop audited, which still leaves the
    /// sender with the refusal receipt this refusal already decided on.
    fn hold(
        &self,
        link: &Link,
        message: &Message,
        code: spot_link_exchange::RejectCode,
        now_ms: u64,
    ) -> Result<(), ReceiveError> {
        let input = QuarantinedInput {
            link: link.id,
            message: message.clone(),
            code,
            at: now_ms,
        };
        match self.runtime.limits.hold(self.quarantine, input) {
            Ok(true) => Ok(()),
            // Bounded: the input is not held, and the drop is a refusal of its own.
            Ok(false) => self.audit(link, message, codes::LINK_QUARANTINE_LIMIT, now_ms),
            Err(error) => Err(ReceiveError::from(error)),
        }
    }

    /// Refuses a message a per-link bound stops, whatever the contract would have said about it.
    ///
    /// The refusal is remembered like any other, so a redelivery gets the same answer even after
    /// the bound clears: the partner sends a new message (a new id) rather than retrying this one.
    fn refuse_bound(
        &self,
        link: &Link,
        message: &Message,
        code: &'static str,
        now_ms: u64,
    ) -> Result<ReceiveOutcome, ReceiveError> {
        let reject = spot_link_exchange::RejectCode::PayloadInvalid;
        self.audit(link, message, code, now_ms)?;
        self.hold(link, message, reject, now_ms)?;
        self.runtime
            .store
            .note_rejection(link.id, message.id, reject)?;
        Ok(self.rejection(link, message, reject, now_ms))
    }

    /// Quarantines the message, audits the refusal, remembers the verdict and answers with a
    /// signed rejection.
    fn refuse(
        &self,
        link: &Link,
        message: &Message,
        violation: &Violation,
        now_ms: u64,
    ) -> Result<ReceiveOutcome, ReceiveError> {
        let code = violation.reject_code();
        // Audited first: a refusal that is not written down is not a decision anyone can review,
        // so the trail failing stops the refusal before anything is stored.
        self.audit(link, message, violation.code(), now_ms)?;
        self.hold(link, message, code, now_ms)?;
        self.runtime
            .store
            .note_rejection(link.id, message.id, code)?;
        self.mark_blocked(link, message, violation, now_ms)?;
        Ok(self.rejection(link, message, code, now_ms))
    }

    /// Raises the `needs attention` marker when the refused message is one a request is waiting
    /// for, because then the failure blocks its path. A refusal that blocks nothing leaves the
    /// request alone.
    fn mark_blocked(
        &self,
        link: &Link,
        message: &Message,
        violation: &Violation,
        now_ms: u64,
    ) -> Result<(), ReceiveError> {
        let Ok(decoded) = decode(&message.payload) else {
            return Ok(());
        };
        let Some(record) = self.runtime.store.load(&link.id, &message.conversation)? else {
            return Ok(());
        };
        let blocks = record.waits.iter().any(|wait| {
            matches!(&wait.kind, WaitKind::Message { message, .. } if *message == decoded.header.message)
        });
        if blocks {
            crate::record::update(
                &*self.runtime.store,
                &link.id,
                &message.conversation,
                |record| {
                    record.mark_attention(violation.code(), now_ms);
                    Ok(())
                },
            )?;
        }
        Ok(())
    }

    fn rejection(
        &self,
        link: &Link,
        message: &Message,
        code: spot_link_exchange::RejectCode,
        now_ms: u64,
    ) -> ReceiveOutcome {
        let receipt = Receipt::sign(
            self.signer,
            link.id,
            message.id,
            message.conversation.clone(),
            message.seq,
            ReceiptOutcome::Rejected(code),
            now_ms,
        );
        ReceiveOutcome {
            status: ReceiveStatus::Rejected(code),
            receipts: vec![receipt],
        }
    }

    /// Records the message in its conversation when the inbox took it.
    fn record_if_applied(
        &self,
        link: &Link,
        message: &Message,
        outcome: &ReceiveOutcome,
        now_ms: u64,
    ) -> Result<(), ReceiveError> {
        let taken = match outcome.status {
            ReceiveStatus::Applied => true,
            ReceiveStatus::Duplicate => outcome.receipts.iter().any(|receipt| {
                receipt.message == message.id && receipt.outcome == ReceiptOutcome::Applied
            }),
            _ => false,
        };
        if !taken {
            return Ok(());
        }
        if let Ok(decoded) = decode(&message.payload) {
            record_received(
                self.runtime,
                &link.id,
                &message.conversation,
                &decoded,
                message.id,
                now_ms,
            )?;
        }
        Ok(())
    }
}

/// Whether the message passes what the inbox checks before it reads the payload: size,
/// canonical form, an active receive grant that covers the message's schema version, and an
/// expiry after its send.
///
/// The inbox also bounds the message's lifetime and its clock skew, from its own config, so this
/// pre-check leaves those to it rather than guessing a second number: the two should be one
/// shared pre-check (see the crate's report, `## Open questions`).
fn within_inbox_limits(inbox: &Inbox<'_>, link: &Link, message: &Message, now_ms: u64) -> bool {
    message.payload.len() <= MAX_PAYLOAD_BYTES
        && is_canonical(&message.payload)
        // The inbox's own grant check, so a grant that requires a peer region is judged by the
        // inbox's oracle here as it is there (and refused when it has none).
        && inbox
            .receive_grant(link, &message.topic, now_ms)
            .is_ok_and(|grant| grant.schema_range.contains(message.schema_version))
        && message.expires_at > message.sent_at
        && now_ms < message.expires_at
}

/// Whether the message verifies as the link's peer's: the transport's identity, the message's
/// sender and the key pinned in the link all agree.
fn authenticated(link: &Link, transport_peer: &Party, message: &Message) -> bool {
    if message.link != link.id {
        return false;
    }
    let Some(peer) = link.peer() else {
        return false;
    };
    if peer != transport_peer || peer != &message.sender {
        return false;
    }
    let Some(index) = link.party_index(peer) else {
        return false;
    };
    link.peer_keys
        .get(index)
        .and_then(Option::as_ref)
        .is_some_and(|key| message.verify(key).is_ok())
}
