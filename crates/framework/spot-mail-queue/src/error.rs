//! The stable codes of the mail queue and the enqueue-side error. The codes ride job results,
//! bounce records and log lines; none of them carries a message body, an address list or a
//! secret.

use thiserror::Error;

/// Stable catalogue codes (`crates/framework/spot-errors/catalogue/mail_queue.toml`).
pub mod codes {
    /// The job arguments could not be parsed.
    pub const MALFORMED_JOB: &str = "mail_queue.malformed_job";
    /// The job version is not known to this worker.
    pub const UNSUPPORTED_VERSION: &str = "mail_queue.unsupported_version";
    /// The message failed validation.
    pub const INVALID_MESSAGE: &str = "mail_queue.invalid_message";
    /// A queue limit was exceeded.
    pub const LIMIT_EXCEEDED: &str = "mail_queue.limit_exceeded";
    /// The sender account does not exist for the site.
    pub const UNKNOWN_ACCOUNT: &str = "mail_queue.unknown_account";
    /// The account directory is unavailable (retry).
    pub const DIRECTORY_UNAVAILABLE: &str = "mail_queue.directory_unavailable";
    /// The From address is not the account's address.
    pub const SENDER_MISMATCH: &str = "mail_queue.sender_mismatch";
    /// The credential service is unavailable (retry).
    pub const CREDENTIAL_UNAVAILABLE: &str = "mail_queue.credential_unavailable";
    /// The credential reference has no secret.
    pub const CREDENTIAL_NOT_FOUND: &str = "mail_queue.credential_not_found";
    /// An attachment is not in the file store.
    pub const ATTACHMENT_MISSING: &str = "mail_queue.attachment_missing";
    /// The file store is unavailable (retry).
    pub const ATTACHMENT_UNAVAILABLE: &str = "mail_queue.attachment_unavailable";
    /// The attachments exceed the total size limit.
    pub const ATTACHMENT_TOO_LARGE: &str = "mail_queue.attachment_too_large";
    /// The account's transport settings are invalid.
    pub const TRANSPORT_CONFIG: &str = "mail_queue.transport_config";
    /// The egress guard returned no address to connect to, so nothing may be sent.
    pub const PINNED_REQUIRED: &str = "mail_queue.pinned_required";
    /// The server failed temporarily (retry).
    pub const SMTP_TRANSIENT: &str = "mail_queue.smtp_transient";
    /// The server refused the message for good.
    pub const SMTP_PERMANENT: &str = "mail_queue.smtp_permanent";
    /// The server refused the credentials.
    pub const SMTP_AUTHENTICATION: &str = "mail_queue.smtp_authentication";
    /// One recipient was rejected by the server.
    pub const RECIPIENT_REJECTED: &str = "mail_queue.recipient_rejected";
    /// The sent log is unavailable (retry, before sending).
    pub const SENT_LOG_UNAVAILABLE: &str = "mail_queue.sent_log_unavailable";
    /// The bounce sink is unavailable (retry).
    pub const BOUNCE_UNAVAILABLE: &str = "mail_queue.bounce_unavailable";
    /// Every attempt the job was allowed failed with an error that might have succeeded later;
    /// the message is given up on and every recipient has a bounce record.
    pub const RETRIES_EXHAUSTED: &str = "mail_queue.retries_exhausted";
}

/// Why a message could not be turned into a job. The text names the field, never its value.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MailQueueError {
    /// The message, account or message id is invalid.
    #[error("{code}: {reason}", code = codes::INVALID_MESSAGE)]
    Invalid {
        /// Which part is invalid, in plain words (no values).
        reason: &'static str,
    },
    /// A queue limit was exceeded.
    #[error("{code}: {reason}", code = codes::LIMIT_EXCEEDED)]
    Limit {
        /// Which limit, in plain words.
        reason: &'static str,
    },
}

impl MailQueueError {
    /// The stable catalogue code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Invalid { .. } => codes::INVALID_MESSAGE,
            Self::Limit { .. } => codes::LIMIT_EXCEEDED,
        }
    }
}
