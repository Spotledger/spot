//! Stable error codes of the edge sync service: the `sync_node_host.*` family.
//!
//! The wire's own failures keep their `sync_http.*` code (the loop reports them as the last
//! error, never re-labels them); everything this crate adds is catalogued in
//! `crates/framework/spot-errors/catalogue/sync_node_host.toml`. No message carries a record's
//! content, a key, a certificate, an enrolment code or a personal value: names, counts and
//! positions only.

use std::fmt;

/// The hub presented a certificate other than the pinned one (even a CA-signed one).
pub const HUB_UNTRUSTED: &str = "sync_node_host.hub_untrusted";
/// The node has no identity or no pinned hub certificate yet, so the loop does not start.
pub const NOT_ENROLLED: &str = "sync_node_host.not_enrolled";
/// The enrolment code file is not a plain file with mode `0600`, or could not be read or wiped.
pub const CODE_FILE_UNSAFE: &str = "sync_node_host.code_file_unsafe";
/// A served record's envelope did not decode, or is of a version this node does not know.
pub const ENVELOPE_INVALID: &str = "sync_node_host.envelope_invalid";
/// A served record named a source, tenant or partition the hub does not own for this node.
pub const FOREIGN_RECORD: &str = "sync_node_host.foreign_record";
/// The apply path failed (not a refusal of one bundle: the store itself failed).
pub const APPLY_FAILED: &str = "sync_node_host.apply_failed";
/// The durable state (cursor, outbox) could not be read or written.
pub const STORE_FAILED: &str = "sync_node_host.store_failed";
/// A `[services.sync_node]` setting is out of bounds.
pub const INVALID_SETTINGS: &str = "sync_node_host.invalid_settings";
/// The node's own outbox yielded a record the node does not own; it is never pushed.
pub const NOT_OWNED: &str = "sync_node_host.not_owned";
/// The hub refused the handshake (version or metadata epoch).
pub const HUB_REFUSED: &str = "sync_node_host.hub_refused";

/// Every `sync_node_host.*` code, so the error-code scan sees each one.
pub const CODES: [&str; 10] = [
    HUB_UNTRUSTED,
    NOT_ENROLLED,
    CODE_FILE_UNSAFE,
    ENVELOPE_INVALID,
    FOREIGN_RECORD,
    APPLY_FAILED,
    STORE_FAILED,
    INVALID_SETTINGS,
    NOT_OWNED,
    HUB_REFUSED,
];

/// What went wrong. `code` is stable; callers match on it, never on the message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    /// The stable code: `sync_node_host.*`, or the wire's `sync_http.*` code.
    pub code: &'static str,
    /// What went wrong, without any secret or record content.
    pub message: String,
}

/// The crate's result type.
pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// An error with `code`.
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// The hub's certificate is not the pinned one.
    pub fn hub_untrusted() -> Self {
        Self::new(
            HUB_UNTRUSTED,
            "the hub presented a certificate other than the pinned one",
        )
    }

    /// No identity or no pin yet.
    pub fn not_enrolled(message: impl Into<String>) -> Self {
        Self::new(NOT_ENROLLED, message)
    }

    /// The enrolment code file is unsafe.
    pub fn code_file_unsafe(message: impl Into<String>) -> Self {
        Self::new(CODE_FILE_UNSAFE, message)
    }

    /// An envelope did not decode.
    pub fn envelope_invalid(message: impl Into<String>) -> Self {
        Self::new(ENVELOPE_INVALID, message)
    }

    /// A record is not for this node.
    pub fn foreign_record(message: impl Into<String>) -> Self {
        Self::new(FOREIGN_RECORD, message)
    }

    /// The apply path failed.
    pub fn apply_failed(message: impl Into<String>) -> Self {
        Self::new(APPLY_FAILED, message)
    }

    /// The durable state failed.
    pub fn store_failed(message: impl Into<String>) -> Self {
        Self::new(STORE_FAILED, message)
    }

    /// A setting is out of bounds.
    pub fn invalid_settings(message: impl Into<String>) -> Self {
        Self::new(INVALID_SETTINGS, message)
    }

    /// An outbox record is not owned by this node.
    pub fn not_owned(message: impl Into<String>) -> Self {
        Self::new(NOT_OWNED, message)
    }

    /// The hub refused the handshake.
    pub fn hub_refused(message: impl Into<String>) -> Self {
        Self::new(HUB_REFUSED, message)
    }

    /// Whether trying again later can help: every wire failure is retried by the loop's backoff;
    /// a local fault (a bad setting, an unsafe file) is not.
    pub fn retryable(&self) -> bool {
        !matches!(
            self.code,
            INVALID_SETTINGS | CODE_FILE_UNSAFE | NOT_ENROLLED
        )
    }
}

impl From<spot_sync_http::Error> for Error {
    fn from(error: spot_sync_http::Error) -> Self {
        Self {
            code: error.code,
            message: error.message,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for Error {}
