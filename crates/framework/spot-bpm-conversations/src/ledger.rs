//! Keeping the conversation record in step with what the link exchange delivered.
//!
//! The inbox stores a message and appends its delivery notice in one atomic commit; the
//! conversation record is a second store, so the two cannot commit together. [`record_received`]
//! closes the gap from one side: it is idempotent on the message id, the gate calls it right after
//! the inbox accepted a message, and [`repair`] calls it for every notice a crash left
//! unrecorded. A restart therefore never loses an arrival and never counts one twice.

use std::sync::Arc;

use spot_bpm_contract::ProcessContract;
use spot_link_exchange::{Conversation, InboxStore, MessageId, NoticeFeed};
use spot_tenant_link::LinkId;

use crate::error::{ConversationError, Result};
use crate::record::{ConversationRecord, update};
use crate::runtime::Runtime;
use crate::wire::{Decoded, decode};

/// The notice consumer name the ledger repair acknowledges under.
pub const REPAIR_CONSUMER: &str = concat!("bpm", ".conversations.ledger");

/// The most notices one repair pass reads.
const REPAIR_BATCH: usize = 256;

/// Loads the conversation's record, creating it on first contact.
///
/// # Errors
/// [`ConversationError::Store`] on a backend failure.
pub fn ensure_record(
    runtime: &Runtime,
    link: &LinkId,
    conversation: &Conversation,
    contract: &ProcessContract,
    role: &str,
    peer_role: &str,
) -> Result<ConversationRecord> {
    if let Some(record) = runtime.store.load(link, conversation)? {
        return Ok(record);
    }
    let fresh = ConversationRecord::new(*link, conversation.clone(), contract, role, peer_role);
    match runtime.store.create(fresh) {
        Ok(record) => Ok(record),
        Err(ConversationError::Conflict) => runtime
            .store
            .load(link, conversation)?
            .ok_or(ConversationError::Store),
        Err(error) => Err(error),
    }
}

/// Records a received message in its conversation: the first message of a conversation creates
/// the record. Idempotent on `id`.
///
/// Returns `false` when the message names a contract, version or role this tenant cannot place
/// (the gate refuses those before they reach the inbox; a message that got here some other way
/// is left for the quarantine, never recorded).
///
/// # Errors
/// [`ConversationError::TooLong`] when the history is full, or a store failure.
pub fn record_received(
    runtime: &Runtime,
    link: &LinkId,
    conversation: &Conversation,
    decoded: &Decoded,
    id: MessageId,
    at_ms: u64,
) -> Result<bool> {
    let record = match runtime.store.load(link, conversation)? {
        Some(record) => record,
        None => {
            let Some(contract) = runtime
                .book
                .get(&decoded.header.contract, decoded.header.version)
            else {
                return Ok(false);
            };
            let (Some(role), Some(def)) = (
                runtime.book.role(&contract.id),
                contract.message(&decoded.header.message),
            ) else {
                return Ok(false);
            };
            let peer_role = runtime
                .peers
                .peer_role(link, &contract.id)
                .unwrap_or_else(|| def.from.clone());
            ensure_record(runtime, link, conversation, contract, role, &peer_role)?
        }
    };
    if record.contract != decoded.header.contract
        || record.contract_version != decoded.header.version
    {
        return Ok(false);
    }
    let Some(contract) = runtime
        .book
        .get(&record.contract, record.contract_version)
        .map(Arc::clone)
    else {
        return Ok(false);
    };
    update(&*runtime.store, link, conversation, |record| {
        record.append_received(
            &contract,
            &decoded.header.message,
            id,
            at_ms,
            decoded.payload.clone(),
        )
    })?;
    Ok(true)
}

/// Records a message this side already sent, when the outbox holds it and the conversation record
/// does not: the crash window between `sender.send` succeeding and `append_sent`.
///
/// Without it the record has no Sent event for a message the peer has already answered: the reply
/// is judged against a history that never sent anything, refused as out of order and pinned, and a
/// replay's own order check fails the same way. A send step repairs the window itself; the gate and
/// [`repair`] call this too, so a reply that arrives first is judged against the true history.
///
/// Returns how many events it recorded. Idempotent, and only a message the outbox knows can be
/// recorded, so a decision whose send never happened is left alone.
///
/// # Errors
/// A store or sender failure.
pub fn record_sent(
    runtime: &Runtime,
    link: &LinkId,
    conversation: &Conversation,
    at_ms: u64,
) -> Result<usize> {
    let Some(record) = runtime.store.load(link, conversation)? else {
        return Ok(0);
    };
    if record.intents.is_empty() {
        return Ok(0);
    }
    let Some(sender) = runtime.peers.sender(link) else {
        return Ok(0);
    };
    let Some(contract) = runtime
        .book
        .get(&record.contract, record.contract_version)
        .cloned()
    else {
        return Ok(0);
    };
    let mut missing = Vec::new();
    for intent in record.intents.values() {
        if record.has_event(&intent.id) {
            continue;
        }
        if sender.state(&intent.id)?.is_some() {
            missing.push(intent.clone());
        }
    }
    if missing.is_empty() {
        return Ok(0);
    }
    update(&*runtime.store, link, conversation, |record| {
        let mut recorded = 0;
        for intent in &missing {
            if record.has_event(&intent.id) {
                continue;
            }
            record.append_sent(&contract, &intent.message, intent.id, at_ms)?;
            recorded += 1;
        }
        Ok(recorded)
    })
    .map(|(recorded, _)| recorded)
}

/// Records every notice the ledger has not seen, from the notice feed and the inbox, and returns
/// how many messages it placed in a conversation.
///
/// Call it at startup: it repairs a crash that stored a message in the inbox but stopped before
/// recording it in the conversation.
///
/// # Errors
/// A store failure.
pub fn repair(runtime: &Runtime, feed: &dyn NoticeFeed, inbox: &dyn InboxStore) -> Result<usize> {
    let mut recorded = 0;
    loop {
        let after = feed
            .acked(REPAIR_CONSUMER)
            .map_err(|_| ConversationError::Store)?;
        let notices = feed
            .read_from(after, REPAIR_BATCH)
            .map_err(|_| ConversationError::Store)?;
        let Some(last) = notices.last().map(|notice| notice.cursor) else {
            return Ok(recorded);
        };
        for notice in &notices {
            let entry = inbox
                .entry(&notice.link, &notice.message_id)
                .map_err(|_| ConversationError::Store)?;
            let Some(entry) = entry else {
                continue;
            };
            if entry.message.tombstone {
                continue;
            }
            // A payload that is not a contract message was never recorded by the gate either.
            if let Ok(decoded) = decode(&entry.message.payload)
                && record_received(
                    runtime,
                    &notice.link,
                    &notice.conversation,
                    &decoded,
                    notice.message_id,
                    notice.received_at,
                )?
            {
                recorded += 1;
            }
            // A message this side sent may be in the outbox without being in the record.
            record_sent(
                runtime,
                &notice.link,
                &notice.conversation,
                notice.received_at,
            )?;
        }
        feed.ack(REPAIR_CONSUMER, last)
            .map_err(|_| ConversationError::Store)?;
    }
}
