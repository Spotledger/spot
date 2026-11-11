//! The persistent outgoing mail queue on the job engine (WP-PLT-11.T3).
//!
//! `spot-mail` is only a transport. This crate makes mail reliable: a business transaction
//! adds the job built by [`enqueue_mail`] to its outbox (ADR-51 phase 3/4), so mail leaves only
//! after the transaction commits; [`MailSender`] runs the job, retries what may succeed later
//! and never sends one message twice once a send is recorded.
//!
//! - [`job`]: [`MailJob`] (the versioned job arguments, files by `FileStore` key) and
//!   [`enqueue_mail`] (job name `platform:send_mail`, queue `default`, idempotency key = the
//!   message id).
//! - [`sender`]: [`MailSender`], the handler. Read its module docs for the delivery guarantee
//!   (at-least-once, with the narrow window between the server's acceptance and the sent-log
//!   write — a recorded security finding — and a bounce for every recipient when the attempts the job is
//!   allowed run out or a worker timeout or panic kills the last one, a recorded security finding) and the secrets rule.
//! - [`ports`]: `AccountDirectory`, `CredentialResolver`, `TransportFactory`, `SentLog`,
//!   `BounceSink` and the in-memory sent log and bounce sink.
//! - `contract`: the `SentLog` contract every implementation must pass (behind the `testing`
//!   feature), which is what makes "it is a sent log" a checkable claim.
//! - [`error`]: the `mail_queue.*` codes.
//!
//! Every port call carries the job's `SiteId`; accounts, credentials, files, the sent log and
//! bounces are per site, and the egress guard sees the site as the tenant.

#[cfg(any(test, feature = "testing"))]
pub mod contract;
pub mod error;
pub mod job;
pub mod ports;
pub mod sender;

#[cfg(any(test, feature = "testing"))]
pub use contract::{SentLogContractCase, SentLogContractFailure, run_sent_log_contract};
pub use error::{MailQueueError, codes};
pub use job::{
    MailJob, QueuedAttachment, QueuedHeader, QueuedInline, QueuedMailbox, QueuedMessage,
    enqueue_mail,
};
pub use ports::{
    AccountDirectory, BounceClass, BounceKey, BounceRecord, BounceSink, CredentialError,
    CredentialResolver, InMemoryBounceSink, InMemorySentLog, SentLog, SmtpAccount,
    SmtpTransportFactory, TransportFactory, Unavailable,
};
pub use sender::{MailPorts, MailSender};

/// The job name of an outgoing mail job.
pub const MAIL_JOB_NAME: &str = "platform:send_mail";
/// The queue outgoing mail jobs run on.
pub const MAIL_QUEUE: &str = "default";
/// The current [`MailJob`] version.
pub const MAIL_JOB_VERSION: u32 = 1;
