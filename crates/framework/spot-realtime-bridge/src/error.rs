//! Typed errors of the bridge, with the stable `rtbridge.*` codes of
//! `crates/framework/spot-errors/catalogue/rtbridge.toml`.
//!
//! Storage failures pass through as [`BridgeError::Feed`] and keep their own `storage.*`
//! codes; a caller matches on [`BridgeError::code`], never on the text.

use spot_storage_contract::StorageError;

/// The `rtbridge.*` vocabulary this crate defines, so the error-code scan sees every code
/// exactly where it is used. `CODES` itself is never a raise site — each code's literal
/// appears again at its raise (see the `code()` match and the pump).
pub const CODES: [&str; 6] = [
    CODE_FEED_READ_FAILED,
    CODE_POSITION_LOST,
    CODE_POSITION_STORE_FAILED,
    CODE_DOCTYPE_UNKNOWN,
    CODE_BAD_PRINCIPAL,
    CODE_NOTICE_REJECTED,
];

/// The change feed (either log) failed with an error that is not a position problem.
pub const CODE_FEED_READ_FAILED: &str = "rtbridge.feed_read_failed";
/// The stored position can no longer be continued; the pump has notified the rooms and
/// restarted from the feed's start.
pub const CODE_POSITION_LOST: &str = "rtbridge.position_lost";
/// Reading or saving the durable position failed; nothing was published this pump.
pub const CODE_POSITION_STORE_FAILED: &str = "rtbridge.position_store_failed";
/// A join asked for a DocType the permission snapshot does not know; worded identically to
/// every other denial.
pub const CODE_DOCTYPE_UNKNOWN: &str = "rtbridge.doctype_unknown";
/// The wire principal's permission assignments could not be loaded; worded identically to
/// every other denial.
pub const CODE_BAD_PRINCIPAL: &str = "rtbridge.bad_principal";
/// A user notice was refused at the realtime boundary and not sent: its stored `user` is not
/// a record id, or its payload is not an object of ids and short scalars within the size
/// rule. The entry is counted and skipped; the payload is never logged.
pub const CODE_NOTICE_REJECTED: &str = "rtbridge.notice_rejected";

/// One [`FeedPump`](crate::pump::FeedPump) or authorisation failure.
#[derive(Debug, thiserror::Error)]
pub enum BridgeError {
    /// Reading the change feed or the outbox failed. A `ResyncRequired` storage error is
    /// never returned this way: the pump handles it (it publishes the resync marker and
    /// restarts from the log's start), so a caller never sees it twice.
    #[error("the realtime feed read failed: {feed}: {error}")]
    Feed {
        /// Which log failed: `changes` or `outbox`.
        feed: &'static str,
        /// The storage error as raised.
        #[source]
        error: StorageError,
    },

    /// Reading or saving the durable position failed. The pump publishes nothing on such a
    /// run and leaves both stored positions untouched (a load failure leaves them to load
    /// again), so a caller can retry later without losing anything.
    #[error("the realtime position store failed: {while_doing}: {message}")]
    PositionStore {
        /// Which operation failed: `load` or `save`.
        while_doing: &'static str,
        /// The store's own plain-language message.
        message: String,
    },
}

impl BridgeError {
    /// The stable machine-readable code, as `Catalogue::explain` accepts it.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Feed { error, .. } => match error.kind {
                // ResyncRequired never reaches a caller wrapped this way: the pump
                // handles it inside (resync markers, position reset); this arm exists so
                // the code names the vocabulary honestly if the wrapping ever changes.
                spot_storage_contract::ErrorKind::ResyncRequired => CODE_POSITION_LOST,
                _ => CODE_FEED_READ_FAILED,
            },
            Self::PositionStore { .. } => CODE_POSITION_STORE_FAILED,
        }
    }
}
