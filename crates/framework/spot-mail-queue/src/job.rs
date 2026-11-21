//! The job-arguments shape (`MailJob`) and `enqueue_mail`.
//!
//! The job carries the message with attachments and inline images as `FileStore` keys, never
//! bytes, and the sender account's id, never a credential. The job version is part of the
//! stored arguments so a later release can read old jobs.

use serde::{Deserialize, Serialize};
use spot_core::SiteId;
use spot_files::key::FileKey;
use spot_jobs::NewJob;
use spot_mail::{HeaderName, Mailbox, OutgoingMessage};
use time::OffsetDateTime;

use crate::error::MailQueueError;
use crate::{MAIL_JOB_NAME, MAIL_JOB_VERSION, MAIL_QUEUE};

/// Most envelope recipients (to, cc and bcc together) in one queued message.
pub const MAX_RECIPIENTS: usize = 100;
/// Most attachments plus inline images in one queued message.
pub const MAX_FILES: usize = 50;
/// Most extra headers in one queued message.
pub const MAX_HEADERS: usize = 50;
/// Most bytes of text plus html body in one queued message (job arguments stay small).
pub const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;
/// Longest message id or account id, in bytes.
pub const MAX_ID_BYTES: usize = 255;

/// A mailbox as stored in a job: display name and address text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedMailbox {
    /// The display name, when present.
    pub name: Option<String>,
    /// The address, validated again when the job runs.
    pub address: String,
}

impl QueuedMailbox {
    /// A mailbox with only an address.
    pub fn address(address: impl Into<String>) -> Self {
        Self {
            name: None,
            address: address.into(),
        }
    }
}

/// An attachment held in the file store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedAttachment {
    /// The file name shown to the recipient.
    pub filename: String,
    /// The content type, for example `application/pdf`.
    pub content_type: String,
    /// The full `FileStore` key (`private/...`), looked up in the job's site only.
    pub file_key: String,
    /// The `Content-ID` when the part is inline, without angle brackets.
    pub content_id: Option<String>,
}

/// An inline image held in the file store, referenced from the html body by `cid:`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedInline {
    /// The `Content-ID` the html references, without angle brackets.
    pub content_id: String,
    /// The content type, for example `image/png`.
    pub content_type: String,
    /// The full `FileStore` key, looked up in the job's site only.
    pub file_key: String,
}

/// An extra header.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedHeader {
    /// The header name; structural headers are refused.
    pub name: String,
    /// The single-line value.
    pub value: String,
}

/// The serialisable form of an outgoing message: like `spot_mail::OutgoingMessage` with files
/// by reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedMessage {
    /// The sender; must be the sending account's address.
    pub from: QueuedMailbox,
    /// Where replies go, when not the sender.
    pub reply_to: Option<QueuedMailbox>,
    /// Primary recipients.
    pub to: Vec<QueuedMailbox>,
    /// Carbon copy recipients.
    pub cc: Vec<QueuedMailbox>,
    /// Blind carbon copy recipients.
    pub bcc: Vec<QueuedMailbox>,
    /// The subject line.
    pub subject: String,
    /// The plain text body.
    pub text: Option<String>,
    /// The html body.
    pub html: Option<String>,
    /// File attachments by key.
    pub attachments: Vec<QueuedAttachment>,
    /// Inline images by key.
    pub inline: Vec<QueuedInline>,
    /// The `Message-ID` this message answers, without angle brackets.
    pub in_reply_to: Option<String>,
    /// The `References` chain, oldest first, without angle brackets.
    pub references: Vec<String>,
    /// Extra headers.
    pub headers: Vec<QueuedHeader>,
}

impl QueuedMessage {
    /// A message from `from` with `subject`; every other field starts empty.
    pub fn new(from: QueuedMailbox, subject: impl Into<String>) -> Self {
        Self {
            from,
            reply_to: None,
            to: Vec::new(),
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: subject.into(),
            text: None,
            html: None,
            attachments: Vec::new(),
            inline: Vec::new(),
            in_reply_to: None,
            references: Vec::new(),
            headers: Vec::new(),
        }
    }

    /// Every envelope recipient address: to, then cc, then bcc.
    pub fn recipient_addresses(&self) -> impl Iterator<Item = &str> {
        self.to
            .iter()
            .chain(self.cc.iter())
            .chain(self.bcc.iter())
            .map(|mailbox| mailbox.address.as_str())
    }

    /// Builds the `OutgoingMessage` without files (the sender adds attachments and inline
    /// images after loading them) and with `message_id` as the wire id, so every retry sends
    /// the same `Message-ID`. Validates it.
    pub fn to_outgoing(&self, message_id: &str) -> Result<OutgoingMessage, spot_mail::MailError> {
        let mailbox = |m: &QueuedMailbox| Mailbox::new(m.name.clone(), &m.address);
        let list =
            |items: &[QueuedMailbox]| items.iter().map(mailbox).collect::<Result<Vec<_>, _>>();
        let mut out = OutgoingMessage::new(mailbox(&self.from)?, self.subject.clone());
        out.reply_to = self.reply_to.as_ref().map(mailbox).transpose()?;
        out.to = list(&self.to)?;
        out.cc = list(&self.cc)?;
        out.bcc = list(&self.bcc)?;
        out.text = self.text.clone();
        out.html = self.html.clone();
        out.in_reply_to = self.in_reply_to.clone();
        out.references = self.references.clone();
        out.message_id = Some(message_id.to_owned());
        for header in &self.headers {
            out.headers
                .push((HeaderName::new(&header.name)?, header.value.clone()));
        }
        out.validate()?;
        Ok(out)
    }

    fn check_limits(&self) -> Result<(), MailQueueError> {
        if self.recipient_addresses().count() > MAX_RECIPIENTS {
            return Err(MailQueueError::Limit {
                reason: "too many recipients",
            });
        }
        if self.attachments.len() + self.inline.len() > MAX_FILES {
            return Err(MailQueueError::Limit {
                reason: "too many attachments",
            });
        }
        if self.headers.len() > MAX_HEADERS {
            return Err(MailQueueError::Limit {
                reason: "too many headers",
            });
        }
        let body =
            self.text.as_deref().map_or(0, str::len) + self.html.as_deref().map_or(0, str::len);
        if body > MAX_BODY_BYTES {
            return Err(MailQueueError::Limit {
                reason: "body too large",
            });
        }
        Ok(())
    }

    fn check_file_keys(&self) -> Result<(), MailQueueError> {
        let keys = self
            .attachments
            .iter()
            .map(|a| a.file_key.as_str())
            .chain(self.inline.iter().map(|i| i.file_key.as_str()));
        for key in keys {
            if FileKey::parse(key).is_err() {
                return Err(MailQueueError::Invalid {
                    reason: "file key is not a valid file store key",
                });
            }
        }
        Ok(())
    }
}

/// The stored arguments of a `platform:send_mail` job. Holds no secret and no file bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailJob {
    /// The job-arguments version; see [`MAIL_JOB_VERSION`].
    pub version: u32,
    /// The sending account's id, resolved through the `AccountDirectory` at send time.
    pub account: String,
    /// The message id: the job's idempotency key and the wire `Message-ID`.
    pub message_id: String,
    /// The message.
    pub message: QueuedMessage,
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_BYTES
        && id.is_ascii()
        && !id
            .chars()
            .any(|c| c.is_ascii_control() || c == '<' || c == '>' || c == ' ')
}

/// Builds the `platform:send_mail` job for `message`, to be added to the operation's outbox in
/// phase 3 or 4 so mail leaves only after the business transaction commits. The idempotency
/// key is `message_id`, so enqueueing one message twice yields one job. Does not talk to a
/// database. `not_before` is the earliest send time (time is an argument, never read here).
///
/// Validates the message, the ids, the file keys and the size limits, so a bad message fails at
/// the caller rather than as a dead job.
pub fn enqueue_mail(
    site: SiteId,
    message: &QueuedMessage,
    account: &str,
    message_id: &str,
    not_before: OffsetDateTime,
) -> Result<NewJob, MailQueueError> {
    if !valid_id(account) {
        return Err(MailQueueError::Invalid {
            reason: "account id is empty, too long or has forbidden characters",
        });
    }
    if !valid_id(message_id) {
        return Err(MailQueueError::Invalid {
            reason: "message id is empty, too long or has forbidden characters",
        });
    }
    message.check_limits()?;
    message.check_file_keys()?;
    message
        .to_outgoing(message_id)
        .map_err(|_| MailQueueError::Invalid {
            reason: "message failed validation",
        })?;
    let job = MailJob {
        version: MAIL_JOB_VERSION,
        account: account.to_owned(),
        message_id: message_id.to_owned(),
        message: message.clone(),
    };
    let args = serde_json::to_value(&job).map_err(|_| MailQueueError::Invalid {
        reason: "message cannot be serialised",
    })?;
    Ok(NewJob::new(
        MAIL_JOB_NAME.to_owned(),
        args,
        MAIL_QUEUE.to_owned(),
        site,
        not_before,
    )
    .with_idempotency_key(message_id.to_owned()))
}
