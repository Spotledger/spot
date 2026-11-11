//! What a request shows of its conversation: [`ConversationView`].
//!
//! The view is for the screen of the request: which contract and peer the conversation runs on,
//! the last message each way, the delivery state of every message, and whatever is stuck. A peer
//! that cannot be reached is a *condition* (an amber line saying the message is waiting for the
//! peer's server), never an error: the request is not lost and nothing needs both sides online.

use spot_bpm_contract::OutcomeKind;
use spot_link_exchange::{Conversation, MessageId, MessageState};
use spot_quarantine::{Condition, Severity};
use spot_tenant_link::LinkId;

use crate::error::Result;
use crate::record::{Attention, ConversationRecord, Direction, Ending};
use crate::runtime::Runtime;

/// How long a message may sit undelivered before it is a stuck condition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewThresholds {
    /// Milliseconds a queued, sent or unapplied message may wait.
    pub undelivered_ms: u64,
}

impl Default for ViewThresholds {
    fn default() -> Self {
        Self {
            undelivered_ms: 3_600_000,
        }
    }
}

/// One message of the conversation as the request shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageView {
    /// The contract message's name.
    pub message: String,
    /// The link message's id.
    pub id: MessageId,
    /// Which way it went.
    pub direction: Direction,
    /// `received`, or the delivery state of a sent message (`queued`, `sent`, `acknowledged`,
    /// `applied`, `rejected`, `expired`).
    pub delivery: &'static str,
}

/// The request-visible state of one conversation.
#[derive(Debug, Clone, PartialEq)]
pub struct ConversationView {
    /// The conversation.
    pub conversation: Conversation,
    /// The link it travels on.
    pub link: LinkId,
    /// The contract's id.
    pub contract: String,
    /// The contract version it runs on.
    pub contract_version: u32,
    /// The role this side plays.
    pub role: String,
    /// The role of the peer.
    pub peer: String,
    /// The request the conversation belongs to.
    pub case: Option<String>,
    /// The name of the last message sent.
    pub last_sent: Option<String>,
    /// The name of the last message received.
    pub last_received: Option<String>,
    /// Every message, in the order this side saw them.
    pub messages: Vec<MessageView>,
    /// How the conversation ended, when it did.
    pub ending: Option<Ending>,
    /// Why a person has to look at the request, when they do.
    pub needs_attention: Option<Attention>,
    /// What is stuck, in plain language.
    pub conditions: Vec<Condition>,
}

fn condition(
    code: &str,
    severity: Severity,
    record: &ConversationRecord,
    sentence: String,
    action: &str,
) -> Condition {
    Condition {
        code: code.to_owned(),
        severity,
        subject: format!("conversation {}", record.conversation),
        sentence,
        action: action.to_owned(),
    }
}

/// Builds the view of a record at `now_ms`.
///
/// # Errors
/// A failure of the sender's store while reading delivery states.
pub fn build(
    runtime: &Runtime,
    record: &ConversationRecord,
    now_ms: u64,
    thresholds: &ViewThresholds,
) -> Result<ConversationView> {
    let sender = runtime.peers.sender(&record.link);
    let mut messages = Vec::with_capacity(record.events.len());
    let mut conditions = Vec::new();
    for event in &record.events {
        let delivery = match event.direction {
            Direction::Received => "received",
            Direction::Sent => {
                let state = match &sender {
                    Some(sender) => sender.state(&event.id)?,
                    None => None,
                };
                let age = now_ms.saturating_sub(event.at_ms);
                let stuck = age > thresholds.undelivered_ms;
                match state {
                    Some(MessageState::Queued | MessageState::Sent) if stuck => {
                        conditions.push(condition(
                            "PEER_UNREACHABLE",
                            Severity::Amber,
                            record,
                            format!(
                                "Waiting for the {}'s server: {} has not been delivered yet.",
                                record.peer_role, event.message
                            ),
                            "Nothing is lost; the message is sent again when the peer is reachable.",
                        ));
                    }
                    Some(MessageState::Acknowledged) if stuck => {
                        conditions.push(condition(
                            "PEER_NOT_APPLIED",
                            Severity::Amber,
                            record,
                            format!(
                                "The {} holds {} but has not applied it yet.",
                                record.peer_role, event.message
                            ),
                            "Wait, or ask the peer to look at their queue.",
                        ));
                    }
                    Some(MessageState::Rejected(code)) => {
                        conditions.push(condition(
                            "MESSAGE_REJECTED",
                            Severity::Red,
                            record,
                            format!(
                                "The {} rejected {} ({}).",
                                record.peer_role,
                                event.message,
                                code.code()
                            ),
                            "Fix the cause and send it again, or take the request's error path.",
                        ));
                    }
                    Some(MessageState::Expired) => {
                        conditions.push(condition(
                            "MESSAGE_EXPIRED",
                            Severity::Red,
                            record,
                            format!(
                                "{} expired before the {} confirmed it.",
                                event.message, record.peer_role
                            ),
                            "Check the link, then send the message again.",
                        ));
                    }
                    _ => {}
                }
                state.map_or("queued", MessageState::as_str)
            }
        };
        messages.push(MessageView {
            message: event.message.clone(),
            id: event.id,
            direction: event.direction,
            delivery,
        });
    }
    if let Some(attention) = &record.attention {
        conditions.push(condition(
            "NEEDS_ATTENTION",
            Severity::Red,
            record,
            format!(
                "The conversation with the {} cannot continue on its own ({}).",
                record.peer_role, attention.code
            ),
            "Open the request and decide how to continue.",
        ));
    }
    if let Some(ending) = &record.ending
        && ending.kind == OutcomeKind::NeedsAttention
    {
        conditions.push(condition(
            "NEEDS_ATTENTION",
            Severity::Red,
            record,
            format!(
                "The conversation ended in `{}`, which needs a person to decide.",
                ending.outcome
            ),
            "Open the request and decide how to continue.",
        ));
    }
    Ok(ConversationView {
        conversation: record.conversation.clone(),
        link: record.link,
        contract: record.contract.clone(),
        contract_version: record.contract_version,
        role: record.role.clone(),
        peer: record.peer_role.clone(),
        case: record.case.clone(),
        last_sent: record.last_sent().map(|event| event.message.clone()),
        last_received: record.last_received().map(|event| event.message.clone()),
        messages,
        ending: record.ending.clone(),
        needs_attention: record.attention.clone(),
        conditions,
    })
}
