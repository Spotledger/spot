//! The object-safe transport contract for outgoing mail (WP-PLT-11.T1). Implementations are
//! [`crate::smtp::SmtpTransport`] (real SMTP) and [`crate::capture::CaptureTransport`]
//! (in-process capture for other crates' tests, without the async runtime cost of a fake socket
//! server).

use std::future::Future;
use std::pin::Pin;

use crate::Address;
use crate::error::MailError;
use crate::message::OutgoingMessage;

/// The result of a send: receipts do not fail as a whole when only some recipients are rejected.
/// Partial rejection is an ordinary outcome, not an error; the caller (the outgoing queue)
/// decides what to do about the rejected recipients.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendReceipt {
    /// The message id used on the wire, without angle brackets.
    pub message_id: String,
    /// Recipients the server accepted.
    pub accepted: Vec<Address>,
    /// Recipients the server rejected, each with the failing SMTP reply.
    pub rejected: Vec<(Address, crate::error::SmtpReply)>,
}

/// Sends an outgoing message. Object-safe so callers can hold `Arc<dyn MailTransport>` as a
/// plugin point in front of the outgoing queue.
///
/// The future is hand-boxed rather than `async-trait`-generated: the workspace does not carry the
/// extra dependency and the boxed form is what makes the trait object-safe.
pub trait MailTransport: Send + Sync {
    /// Sends one message and reports which recipients were accepted. Fails with
    /// [`MailError::Transient`] when the caller should retry later, [`MailError::Permanent`]
    /// when it should not, and succeeds with a partial receipt when only some recipients were
    /// rejected.
    fn send<'a>(
        &'a self,
        message: &'a OutgoingMessage,
    ) -> Pin<Box<dyn Future<Output = Result<SendReceipt, MailError>> + Send + 'a>>;
}
