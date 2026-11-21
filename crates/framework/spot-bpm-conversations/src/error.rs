//! Typed errors with stable `bpm_conversations.*` codes, and the [`Violation`]s a message can be
//! refused for.
//!
//! Callers match on [`ConversationError::code`] and [`Violation::code`], never on a message. No
//! message or reason here ever carries a payload value, a secret or text a peer chose: a
//! rejection reason names the rule that was broken and, at most, fields the *contract* defines.

use spot_link_exchange::RejectCode;

/// The stable codes this crate raises. Each has one entry in `bpm_conversations.toml`.
pub mod codes {
    /// The contract (or the version a message names) is not one this tenant holds.
    pub const CONTRACT_UNKNOWN: &str = "bpm_conversations.contract_unknown";
    /// The message is not one the contract defines.
    pub const MESSAGE_UNKNOWN: &str = "bpm_conversations.message_unknown";
    /// The message does not travel between these roles in this direction.
    pub const WRONG_DIRECTION: &str = "bpm_conversations.wrong_direction";
    /// The contract does not allow this message after the previous one.
    pub const OUT_OF_ORDER: &str = "bpm_conversations.out_of_order";
    /// The step the message takes has already been taken as often as the contract allows.
    pub const STEP_EXHAUSTED: &str = "bpm_conversations.step_exhausted";
    /// The conversation has already ended in an outcome.
    pub const CONVERSATION_ENDED: &str = "bpm_conversations.conversation_ended";
    /// The message's schema version is not the one the contract defines.
    pub const SCHEMA_UNSUPPORTED: &str = "bpm_conversations.schema_unsupported";
    /// The payload does not fit the contract message's field list.
    pub const PAYLOAD_INVALID: &str = "bpm_conversations.payload_invalid";
    /// The message does not say which contract message it is.
    pub const HEADER_INVALID: &str = "bpm_conversations.header_invalid";
    /// The sender is not the role the link's peer plays in this contract.
    pub const WRONG_PEER: &str = "bpm_conversations.wrong_peer";
    /// The message cannot start a conversation.
    pub const NOT_ENTRY: &str = "bpm_conversations.not_entry";
    /// The send node has no payload to send.
    pub const PAYLOAD_MISSING: &str = "bpm_conversations.payload_missing";
    /// A send or wait node's config is not usable.
    pub const CONFIG_INVALID: &str = "bpm_conversations.config_invalid";
    /// The request has no conversation for this contract yet.
    pub const NO_CONVERSATION: &str = "bpm_conversations.no_conversation";
    /// No link reaches the role the message is addressed to.
    pub const NO_PEER: &str = "bpm_conversations.no_peer";
    /// The link's grant does not let a field of the message cross.
    pub const FIELD_NOT_GRANTED: &str = "bpm_conversations.field_not_granted";
    /// The link exchange refused to queue the message.
    pub const SEND_FAILED: &str = "bpm_conversations.send_failed";
    /// The receiver rejected the message.
    pub const REJECTED: &str = "bpm_conversations.rejected";
    /// The wait for a message ran out of time.
    pub const WAIT_TIMEOUT: &str = "bpm_conversations.wait_timeout";
    /// The message expired before the receiver confirmed it.
    pub const DELIVERY_EXPIRED: &str = "bpm_conversations.delivery_expired";
    /// The peer cancelled the conversation.
    pub const CONVERSATION_CANCELLED: &str = "bpm_conversations.conversation_cancelled";
    /// The Case id derived for a message already belongs to a Case another message started.
    pub const CASE_COLLISION: &str = "bpm_conversations.case_collision";
    /// The gate's audit trail could not be written.
    pub const AUDIT_FAILED: &str = "bpm_conversations.audit_failed";
    /// The link already holds as many open conversations as it may.
    pub const LINK_CONVERSATION_LIMIT: &str = "bpm_conversations.link_conversation_limit";
    /// The link already holds as many inputs in the quarantine as it may.
    pub const LINK_QUARANTINE_LIMIT: &str = "bpm_conversations.link_quarantine_limit";
    /// The conversation store failed.
    pub const STORE_FAILED: &str = "bpm_conversations.store_failed";
    /// The conversation changed under a writer that did not retry.
    pub const CONFLICT: &str = "bpm_conversations.conflict";
    /// The conversation holds more history than a request may carry.
    pub const TOO_LONG: &str = "bpm_conversations.too_long";
    /// A waiting node could not be completed.
    pub const RESOLVE_FAILED: &str = "bpm_conversations.resolve_failed";
}

/// Why a message may not be sent or taken in: a broken contract rule.
///
/// The reason a violation reports names the rule, never a value; the one field list it can carry
/// holds names the contract itself defines (a name the peer invented is counted, not echoed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Violation {
    /// The contract or its version is not held.
    ContractUnknown,
    /// The message is not defined by the contract.
    MessageUnknown,
    /// The message does not travel between these roles in this direction.
    WrongDirection,
    /// The contract does not allow the message after the previous one.
    OutOfOrder,
    /// The step was already taken as often as allowed.
    StepExhausted,
    /// The conversation already ended.
    ConversationEnded,
    /// The schema version differs from the contract's.
    SchemaUnsupported,
    /// The payload does not fit the field list.
    PayloadInvalid {
        /// Contract-defined fields that are wrong (missing, mistyped or too long), sorted.
        fields: Vec<String>,
        /// How many fields the payload carries that the contract does not define.
        unknown: usize,
    },
    /// The message names no contract message.
    HeaderInvalid,
    /// The sender is not the role the peer plays.
    WrongPeer,
    /// The message cannot start a conversation.
    NotEntry,
}

impl Violation {
    /// The stable code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::ContractUnknown => codes::CONTRACT_UNKNOWN,
            Self::MessageUnknown => codes::MESSAGE_UNKNOWN,
            Self::WrongDirection => codes::WRONG_DIRECTION,
            Self::OutOfOrder => codes::OUT_OF_ORDER,
            Self::StepExhausted => codes::STEP_EXHAUSTED,
            Self::ConversationEnded => codes::CONVERSATION_ENDED,
            Self::SchemaUnsupported => codes::SCHEMA_UNSUPPORTED,
            Self::PayloadInvalid { .. } => codes::PAYLOAD_INVALID,
            Self::HeaderInvalid => codes::HEADER_INVALID,
            Self::WrongPeer => codes::WRONG_PEER,
            Self::NotEntry => codes::NOT_ENTRY,
        }
    }

    /// The rejection code the sender is told, in the link exchange's vocabulary.
    #[must_use]
    pub fn reject_code(&self) -> RejectCode {
        match self {
            Self::ContractUnknown | Self::SchemaUnsupported => RejectCode::VersionUnsupported,
            Self::OutOfOrder | Self::StepExhausted | Self::ConversationEnded => {
                RejectCode::OutOfOrderExpired
            }
            _ => RejectCode::PayloadInvalid,
        }
    }

    /// A short plain-language reason that names no value.
    #[must_use]
    pub fn reason(&self) -> String {
        match self {
            Self::ContractUnknown => "the contract or its version is not held here".to_owned(),
            Self::MessageUnknown => "the contract does not define this message".to_owned(),
            Self::WrongDirection => {
                "the message does not travel between these roles in this direction".to_owned()
            }
            Self::OutOfOrder => {
                "the contract does not allow this message after the previous one".to_owned()
            }
            Self::StepExhausted => "the step was already taken as often as allowed".to_owned(),
            Self::ConversationEnded => "the conversation has already ended".to_owned(),
            Self::SchemaUnsupported => {
                "the schema version is not the one the contract defines".to_owned()
            }
            Self::PayloadInvalid { fields, unknown } => {
                let mut reason = "the payload does not fit the contract's field list".to_owned();
                if !fields.is_empty() {
                    reason.push_str(": ");
                    reason.push_str(&fields.join(", "));
                }
                if *unknown > 0 {
                    reason.push_str(&format!(
                        " ({unknown} field(s) the contract does not define)"
                    ));
                }
                reason
            }
            Self::HeaderInvalid => "the message does not name a contract message".to_owned(),
            Self::WrongPeer => "the sender is not the role this link's peer plays".to_owned(),
            Self::NotEntry => "the message cannot start a conversation".to_owned(),
        }
    }
}

/// Result of every fallible operation in this crate.
pub type Result<T> = std::result::Result<T, ConversationError>;

/// One problem while sending, receiving or driving a conversation.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ConversationError {
    /// A contract rule was broken.
    #[error("{}", .0.reason())]
    Violation(Violation),
    /// A node's config is not usable.
    #[error("the node config is not usable: {detail}")]
    Config {
        /// What is wrong, never a value.
        detail: &'static str,
    },
    /// The request has no conversation yet.
    #[error("the request has no conversation for this contract")]
    NoConversation,
    /// The send node has nothing to send.
    #[error("the send node's payload variable is missing or is not an object")]
    PayloadMissing,
    /// No link reaches the addressed role.
    #[error("no link reaches the role this message is addressed to")]
    NoPeer,
    /// The link's grant withholds a field of the message.
    #[error("the link's grant does not let {withheld} field(s) of the message cross")]
    FieldNotGranted {
        /// How many fields the grant withholds.
        withheld: usize,
    },
    /// The link exchange refused the message; `code` is its own stable code.
    #[error("the link exchange refused the message ({code})")]
    Send {
        /// The link exchange's stable code.
        code: &'static str,
    },
    /// The conversation store failed.
    #[error("the conversation store failed")]
    Store,
    /// The gate's audit trail could not be written.
    #[error("the audit trail could not be written")]
    Audit,
    /// The conversation changed under the writer.
    #[error("the conversation changed while it was being updated")]
    Conflict,
    /// The conversation's history passed its bound.
    #[error("the conversation holds more history than a request may carry")]
    TooLong,
    /// A waiting node could not be completed.
    #[error("the waiting node could not be completed ({code})")]
    Resolve {
        /// The engine's stable code.
        code: &'static str,
    },
}

impl ConversationError {
    /// The stable machine-readable code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Violation(violation) => violation.code(),
            Self::Config { .. } => codes::CONFIG_INVALID,
            Self::NoConversation => codes::NO_CONVERSATION,
            Self::PayloadMissing => codes::PAYLOAD_MISSING,
            Self::NoPeer => codes::NO_PEER,
            Self::FieldNotGranted { .. } => codes::FIELD_NOT_GRANTED,
            Self::Send { .. } => codes::SEND_FAILED,
            Self::Store => codes::STORE_FAILED,
            Self::Audit => codes::AUDIT_FAILED,
            Self::Conflict => codes::CONFLICT,
            Self::TooLong => codes::TOO_LONG,
            Self::Resolve { .. } => codes::RESOLVE_FAILED,
        }
    }
}

impl From<Violation> for ConversationError {
    fn from(violation: Violation) -> Self {
        Self::Violation(violation)
    }
}
