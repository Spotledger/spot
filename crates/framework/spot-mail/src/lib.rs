//! Outgoing e-mail transport for the native runtime (WP-PLT-11.T1): message model, MIME building
//! and an SMTP transport that classifies failures so the persistent queue can retry the right
//! ones. See blueprint §2 (wave 3) and blueprint §"Lifecycle events" (ADR-51): mail is sent from
//! the `*_committed` phase, never inside a business transaction.
//!
//! Contents:
//! - [`message::OutgoingMessage`]: validated recipients, bodies, attachments and inline images,
//! - [`mime::build_mime`]: pure message to MIME bytes via lettre's builder,
//! - [`transport::MailTransport`]: object-safe send trait with `SendReceipt`,
//! - [`smtp::SmtpTransport`]: lettre-backed async SMTP with Plain/STARTTLS/TLS,
//! - [`smtp::CaptureTransport`]: in-process transport for other crates' tests,
//! - [`imap::fetch_new`]: read-only IMAP ingestion with UID watermarks, size limits and
//!   threading keys (WP-PLT-11.T2),
//! - [`error::MailError`]: transient/permanent classification for retry policy.
//!
//! This crate is only the transport: it knows nothing about documents, accounts or queues.

pub mod capture;
pub mod error;
pub mod imap;
pub mod message;
pub mod mime;
pub mod smtp;
pub mod transport;

pub use capture::CaptureTransport;
pub use error::{MailError, PermanentFailure, SmtpReply, TransientFailure};
pub use imap::{
    FetchBatch, FetchLimits, FetchOutcome, ImapConfig, ImapSecurity, IncomingMessage, MailAddress,
    MailboxCursor, fetch_new, fetch_new_pinned, thread_key,
};
pub use message::{
    Attachment, HeaderName, InlineImage, Mailbox, OutgoingMessage, Secret, validate_address,
};
pub use smtp::{Credentials, SmtpConfig, SmtpTransport, TransportSecurity};
pub use transport::{MailTransport, SendReceipt};

/// The validated lettre address type, re-exported so builders can name the field type.
pub use lettre::Address;
