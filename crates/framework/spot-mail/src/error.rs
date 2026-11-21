//! Typed errors for the mail transport (WP-PLT-11.T1): transient failures are retryable by the
//! persistent outgoing queue (WP-PLT-06), permanent failures are not. Every SMTP reply that
//! caused the failure is carried with its reply code.

use thiserror::Error;

/// An SMTP reply line, split into code and text.
///
/// The code is authoritative for retry classification. The text is the transport's description
/// of the exchange; lettre surfaces full numeric codes but discards the server's reply text
/// lines, so `text` never contains secret data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmtpReply {
    /// The SMTP reply code, for example `451` or `550`.
    pub code: u16,
    /// Human-readable description of the exchange.
    pub text: String,
}

impl SmtpReply {
    /// Builds a reply from a code and description.
    pub fn new(code: u16, text: impl Into<String>) -> Self {
        Self {
            code,
            text: text.into(),
        }
    }

    /// True when the reply is a 4xx transient negative completion.
    pub fn is_transient(&self) -> bool {
        (400..500).contains(&self.code)
    }
}

/// A failure that the queue may retry later: connection refused or reset, command timeout, a 4xx
/// reply, or a TLS handshake failure.
#[derive(Debug, Clone, Error)]
pub enum TransientFailure {
    /// The server answered with a 4xx reply.
    #[error("SMTP transient reply {0:?}")]
    Reply(SmtpReply),
    /// The TCP connection could not be established.
    #[error("connection refused")]
    ConnectionRefused,
    /// The connection was reset by the peer.
    #[error("connection reset")]
    ConnectionReset,
    /// A command timed out waiting for the server.
    #[error("command timed out")]
    Timeout,
    /// The TLS handshake failed.
    #[error("TLS handshake failed")]
    TlsHandshake,
}

/// A failure the queue must not retry automatically.
#[derive(Debug, Clone, Error)]
pub enum PermanentFailure {
    /// The server answered with a 5xx reply.
    #[error("SMTP permanent reply {0:?}")]
    Reply(SmtpReply),
    /// The server refused the credentials, or no compatible AUTH mechanism was offered.
    #[error("authentication failed{}", .reply.as_ref().map(|r| format!(" ({r:?})")).unwrap_or_default())]
    Authentication {
        /// The server's reply, when there was one.
        reply: Option<SmtpReply>,
    },
    /// The message or configuration itself was invalid.
    #[error("invalid message: {0}")]
    InvalidMessage(String),
    /// A client-side error that will not change between retries.
    #[error("{0}")]
    Client(String),
}

/// Errors of the mail transport, classified for retry policy. See [`MailError::is_retryable`].
#[derive(Debug, Clone, Error)]
pub enum MailError {
    /// Retry later: the queue may requeue the message for this transport.
    #[error("transient failure: {0}")]
    Transient(#[from] TransientFailure),
    /// Do not retry: fix the message, the credentials or the configuration.
    #[error("permanent failure: {0}")]
    Permanent(#[from] PermanentFailure),
}

impl MailError {
    /// True when the failure may resolve on retry (connection, timeout, 4xx replies, TLS
    /// handshake).
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Transient(_))
    }

    /// The SMTP reply that caused the failure, when there was one.
    pub fn reply(&self) -> Option<&SmtpReply> {
        match self {
            Self::Transient(TransientFailure::Reply(reply))
            | Self::Permanent(PermanentFailure::Reply(reply)) => Some(reply),
            Self::Permanent(PermanentFailure::Authentication { reply }) => reply.as_ref(),
            _ => None,
        }
    }
}
