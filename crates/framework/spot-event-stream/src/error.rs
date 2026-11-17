//! Typed errors of the event stream engine, with the stable `eventstream.*` codes of
//! `crates/framework/spot-errors/catalogue/eventstream.toml`.
//!
//! A caller matches on [`StreamError::code`], never on the message. A wrapped storage,
//! coordinator or field-guard failure reports **its own** code (they are catalogued in their
//! own files), so no code is invented here.

use spot_events_contract::envelope::Position;
use spot_storage_contract::StorageError;

/// The stable `eventstream.*` vocabulary this crate defines, so the error-code scan sees
/// every code exactly where it is used.
pub const CODES: [&str; 9] = [
    CODE_CONSUMER_NOT_FOUND,
    CODE_CONSUMER_BUSY,
    CODE_RESYNC_REQUIRED,
    CODE_WINDOW_FULL,
    CODE_POSITION_INVALID,
    CODE_ACK_REFUSED,
    CODE_SPEC_INVALID,
    CODE_CLASS_UNKNOWN,
    CODE_ENTITIES_NEED_STATE,
];

/// No consumer with this name belongs to the caller.
pub const CODE_CONSUMER_NOT_FOUND: &str = "eventstream.consumer_not_found";
/// Another connection holds the consumer's lease, or the lease presented has been replaced.
pub const CODE_CONSUMER_BUSY: &str = "eventstream.consumer_busy";
/// The checkpoint or position is no longer held: events were lost to retention.
pub const CODE_RESYNC_REQUIRED: &str = "eventstream.resync_required";
/// The consumer has its in-flight window of delivered but unacknowledged events.
pub const CODE_WINDOW_FULL: &str = "eventstream.window_full";
/// The position is malformed, forged, or was issued for another consumer.
pub const CODE_POSITION_INVALID: &str = "eventstream.position_invalid";
/// The acknowledgement was beyond what was delivered, not a delivered batch's own position,
/// behind the checkpoint, or from a holder that lost its lease.
pub const CODE_ACK_REFUSED: &str = "eventstream.ack_refused";
/// The consumer definition is invalid, or changes something fixed for the life of the
/// consumer such as the partition count.
pub const CODE_SPEC_INVALID: &str = "eventstream.spec_invalid";

/// The consumer names an event class that is not `state`, `domain` or `audit` (telemetry is not
/// an event class).
pub const CODE_CLASS_UNKNOWN: &str = "eventstream.class_unknown";
/// The consumer selects entities but not the `state` class, which is the only class they apply to.
pub const CODE_ENTITIES_NEED_STATE: &str = "eventstream.entities_need_state";

/// One event-stream failure.
#[derive(Debug, thiserror::Error)]
pub enum StreamError {
    /// No consumer with this name exists (or it does not belong to the caller).
    #[error("no consumer named {name}")]
    ConsumerNotFound {
        /// The consumer name asked for.
        name: String,
    },

    /// Another connection holds the consumer's lease, or the lease presented is stale.
    #[error("consumer {name} is busy: {reason}")]
    ConsumerBusy {
        /// The consumer name.
        name: String,
        /// Plain-language reason.
        reason: String,
    },

    /// The checkpoint (or the position a caller stated) is no longer held, so events were
    /// lost to retention or to a stream-generation change. `retained_from` is the earliest
    /// position the engine can still vouch for.
    #[error("consumer {name} must resync from {retained_from:?}: {reason}")]
    ResyncRequired {
        /// The consumer that must resync.
        name: String,
        /// A position to restart from; never later than the slowest registered checkpoint.
        retained_from: Position,
        /// Plain-language reason.
        reason: String,
    },

    /// The consumer's in-flight window is full and `wait` was requested, so the pull would
    /// make no progress.
    #[error("consumer {name} partition {partition} has its in-flight window full")]
    WindowFull {
        /// The consumer name.
        name: String,
        /// The partition.
        partition: u32,
    },

    /// A position token is malformed, forged, of an unknown version, or was issued for a
    /// different consumer.
    #[error("invalid position: {reason}")]
    PositionInvalid {
        /// Plain-language reason.
        reason: String,
    },

    /// An acknowledgement was refused: beyond what was delivered, not a delivered batch's
    /// own position, behind the checkpoint, or from a holder that lost its lease.
    #[error("acknowledgement refused: {reason}")]
    AckRefused {
        /// Plain-language reason.
        reason: String,
    },

    /// The consumer definition is invalid, or changes something fixed for the life of the
    /// consumer.
    #[error("invalid consumer definition: {reason}")]
    SpecInvalid {
        /// Plain-language reason.
        reason: String,
    },

    /// The consumer names an event class that does not exist.
    #[error("unknown event class: expected `state`, `domain` or `audit`")]
    ClassUnknown,

    /// The consumer selects entities without selecting the `state` class.
    #[error("entities apply to the state class only: select `state` too, or drop the entities")]
    EntitiesNeedState,

    /// The feed read failed with an error other than a lost position.
    #[error("feed read failed: {0}")]
    Feed(#[source] StorageError),

    /// The coordinator refused a lease operation.
    #[error("coordinator refused: {message}")]
    Coordinator {
        /// The coordinator's own stable code.
        code: &'static str,
        /// The coordinator's own message.
        message: String,
    },

    /// The permission layer refused to build an envelope.
    #[error("permission filtering failed: {message}")]
    Permission {
        /// The field guard's own stable code.
        code: &'static str,
        /// Plain-language reason.
        message: String,
    },
}

impl StreamError {
    /// The stable machine-readable code, as `Catalogue::explain` accepts it.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::ConsumerNotFound { .. } => CODE_CONSUMER_NOT_FOUND,
            Self::ConsumerBusy { .. } => CODE_CONSUMER_BUSY,
            Self::ResyncRequired { .. } => CODE_RESYNC_REQUIRED,
            Self::WindowFull { .. } => CODE_WINDOW_FULL,
            Self::PositionInvalid { .. } => CODE_POSITION_INVALID,
            Self::AckRefused { .. } => CODE_ACK_REFUSED,
            Self::SpecInvalid { .. } => CODE_SPEC_INVALID,
            Self::ClassUnknown => CODE_CLASS_UNKNOWN,
            Self::EntitiesNeedState => CODE_ENTITIES_NEED_STATE,
            // Wrapped failures report their own code: storage.*, coordinator.*, fieldguard.*.
            Self::Feed(error) => error.kind.code(),
            Self::Coordinator { code, .. } | Self::Permission { code, .. } => code,
        }
    }

    /// A convenience constructor for a spec error.
    #[must_use]
    pub fn spec(reason: impl Into<String>) -> Self {
        Self::SpecInvalid {
            reason: reason.into(),
        }
    }

    /// A convenience constructor for a refused acknowledgement.
    #[must_use]
    pub fn ack(reason: impl Into<String>) -> Self {
        Self::AckRefused {
            reason: reason.into(),
        }
    }
}
