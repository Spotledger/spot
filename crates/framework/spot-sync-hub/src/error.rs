//! The hub error vocabulary and its stable catalogue codes (`catalogue/sync_hub.toml`).
//!
//! Messages carry entity names, ids, positions and node ids only: never a record image, never a
//! secret.

use spot_sync_acks::AckError;
use spot_sync_catchup::CatchUpError;
use spot_sync_closure::ClosureError;
use spot_sync_freshness::FreshnessError;
use spot_sync_obligations::Lane;
use thiserror::Error;

/// The relay configuration cannot work.
pub const INVALID_CONFIG: &str = "sync_hub.invalid_config";
/// A destination id is not registered.
pub const UNKNOWN_DESTINATION: &str = "sync_hub.unknown_destination";
/// A destination id is registered twice.
pub const DUPLICATE_DESTINATION: &str = "sync_hub.duplicate_destination";
/// An acknowledged prefix went backwards; the ack is ignored.
pub const PREFIX_REGRESSED: &str = "sync_hub.prefix_regressed";
/// An acknowledged prefix is beyond what the hub has processed; the ack is ignored.
pub const PREFIX_AHEAD: &str = "sync_hub.prefix_ahead_of_stream";
/// An acknowledged prefix is above the newest position the hub's own stream holds.
pub const ACK_BEYOND_HEAD: &str = "sync_hub.ack_beyond_head";
/// A node's stamp cannot be placed on the hub timeline; the document is refused.
pub const STAMP_REJECTED: &str = "sync_hub.stamp_rejected";
/// The hub stream adapter failed.
pub const STREAM_FAILED: &str = "sync_hub.stream_failed";
/// The hub stream returned entries out of position order or at or before the cursor.
pub const STREAM_DISORDER: &str = "sync_hub.stream_disorder";
/// The hub stream split one source transaction across reads, or interleaved it with another.
pub const STREAM_SPLIT_TXN: &str = "sync_hub.stream_split_txn";
/// The hub stream named more entities than the relay's lane map may hold.
pub const STREAM_ENTITY_LIMIT: &str = "sync_hub.stream_entity_limit";
/// An entity's entry carried another lane than the one the entity came in before.
pub const LANE_NOT_ENTITY: &str = "sync_hub.lane_not_entity";
/// The send queue adapter failed.
pub const QUEUE_FAILED: &str = "sync_hub.queue_failed";
/// A record named by a stream entry has no image at the hub.
pub const IMAGE_MISSING: &str = "sync_hub.image_missing";
/// Entries withheld because a record their closure needs is held ("dependency held").
pub const HELD_BY_DEPENDENCY: &str = "sync_hub.held_by_dependency";

/// Why the relay could not do what it was asked.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum HubError {
    /// A configuration value is unusable.
    #[error("The hub relay configuration is not usable: {reason}.")]
    InvalidConfig {
        /// Which rule is broken.
        reason: &'static str,
    },
    /// The destination is not registered.
    #[error("The destination {dest} is not registered with the hub relay.")]
    UnknownDestination {
        /// The destination id.
        dest: String,
    },
    /// The destination is registered already.
    #[error("The destination {dest} is already registered with the hub relay.")]
    DuplicateDestination {
        /// The destination id.
        dest: String,
    },
    /// The acknowledged prefix went backwards.
    #[error("The destination {dest} acknowledged prefix {after} after prefix {before}.")]
    PrefixRegressed {
        /// The destination id.
        dest: String,
        /// The prefix already acknowledged.
        before: u64,
        /// The lower prefix just reported.
        after: u64,
    },
    /// The acknowledged prefix is past the hub's cursor.
    #[error("The destination {dest} acknowledged prefix {prefix}, past the hub cursor {cursor}.")]
    PrefixAhead {
        /// The destination id.
        dest: String,
        /// The prefix reported.
        prefix: u64,
        /// The highest position the hub has processed.
        cursor: u64,
    },
    /// The acknowledged prefix is above the newest position the hub's own stream holds.
    #[error(
        "The destination {dest} acknowledged prefix {claimed}, above the hub stream head {head}."
    )]
    AckBeyondHead {
        /// The destination id.
        dest: String,
        /// The prefix the node claimed.
        claimed: u64,
        /// The hub's own stream head.
        head: u64,
    },
    /// A stamp that cannot be placed on the hub timeline; the document is not accepted.
    #[error("The stamp from {node} cannot be placed on the hub timeline; the document is refused.")]
    StampRejected {
        /// The node.
        node: String,
    },
    /// The stream adapter failed.
    #[error("The hub stream failed: {reason}.")]
    StreamFailed {
        /// A short, secret-free description.
        reason: String,
    },
    /// The stream broke its ordering contract.
    #[error("The hub stream returned position {position} after position {after}.")]
    StreamDisorder {
        /// The position that broke the order.
        position: u64,
        /// The position it had to exceed.
        after: u64,
    },
    /// The stream broke its whole-transaction contract: one source transaction arrived in more
    /// than one piece, so a bundle would be applied half-and-half.
    #[error(
        "The hub stream split source transaction {txn}, which appears at positions {}; a source \
         transaction is read whole.",
        join(.positions)
    )]
    StreamSplitTxn {
        /// The source transaction.
        txn: String,
        /// Every position of that transaction in the refused read, in read order.
        positions: Vec<u64>,
    },
    /// The stream named more entities than the relay's lane map is bound to hold.
    #[error(
        "The hub stream named the new entity {entity} at position {position} while the lane map \
         already holds its bound of {bound}; the read is refused."
    )]
    StreamEntityLimit {
        /// The entity that did not fit.
        entity: String,
        /// The position of the entry that named it.
        position: u64,
        /// The configured bound (`RelayConfig::max_entities`).
        bound: usize,
    },
    /// The lane of an entity changed, so the versions of one record would swap between lanes.
    #[error(
        "The entity {entity} came in the {} lane, after the {} lane; a record's lane is a function \
         of its entity.",
        lane_name(*.now),
        lane_name(*.was)
    )]
    LaneNotEntity {
        /// The entity.
        entity: String,
        /// The lane the entity came in before.
        was: Lane,
        /// The lane of the refused entry.
        now: Lane,
    },
    /// The queue adapter failed.
    #[error("The send queue failed: {reason}.")]
    QueueFailed {
        /// A short, secret-free description.
        reason: String,
    },
    /// A record has no image at the hub.
    #[error("The {entity} record {id} named by the stream has no image at the hub.")]
    ImageMissing {
        /// The entity.
        entity: String,
        /// The record id.
        id: String,
    },
    /// The closure for a destination could not be built.
    #[error("{0}")]
    Closure(ClosureError),
    /// The acknowledgement ledger refused for a reason other than the head bound (its own code).
    #[error("{0}")]
    Ack(AckError),
    /// The freshness check failed for a reason other than an unrepresentable stamp: an internal
    /// error of the hub, never a silent "no findings".
    #[error("{0}")]
    Freshness(FreshnessError),
    /// The catch-up planner refused (its own code).
    #[error("{0}")]
    CatchUp(CatchUpError),
}

impl HubError {
    /// The stable dotted catalogue code. A closure failure keeps the `sync_closure.*` code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidConfig { .. } => INVALID_CONFIG,
            Self::UnknownDestination { .. } => UNKNOWN_DESTINATION,
            Self::DuplicateDestination { .. } => DUPLICATE_DESTINATION,
            Self::PrefixRegressed { .. } => PREFIX_REGRESSED,
            Self::PrefixAhead { .. } => PREFIX_AHEAD,
            Self::AckBeyondHead { .. } => ACK_BEYOND_HEAD,
            Self::StampRejected { .. } => STAMP_REJECTED,
            Self::StreamFailed { .. } => STREAM_FAILED,
            Self::StreamDisorder { .. } => STREAM_DISORDER,
            Self::StreamSplitTxn { .. } => STREAM_SPLIT_TXN,
            Self::StreamEntityLimit { .. } => STREAM_ENTITY_LIMIT,
            Self::LaneNotEntity { .. } => LANE_NOT_ENTITY,
            Self::QueueFailed { .. } => QUEUE_FAILED,
            Self::ImageMissing { .. } => IMAGE_MISSING,
            Self::Closure(e) => e.code(),
            Self::Ack(e) => e.code(),
            Self::Freshness(e) => e.code(),
            Self::CatchUp(e) => e.code(),
        }
    }
}

impl From<CatchUpError> for HubError {
    fn from(e: CatchUpError) -> Self {
        Self::CatchUp(e)
    }
}

impl From<ClosureError> for HubError {
    fn from(e: ClosureError) -> Self {
        Self::Closure(e)
    }
}

/// "1, 2 and 3": the positions of one transaction in one read, for the message.
fn join(positions: &[u64]) -> String {
    let mut out = String::new();
    for (i, position) in positions.iter().enumerate() {
        if i > 0 {
            out.push_str(if i + 1 == positions.len() {
                " and "
            } else {
                ", "
            });
        }
        out.push_str(&position.to_string());
    }
    out
}

/// The lane's name as the message says it. `Lane` has no `Display` of its own.
fn lane_name(lane: Lane) -> &'static str {
    match lane {
        Lane::Mandatory => "mandatory",
        Lane::Bulk => "bulk",
    }
}
