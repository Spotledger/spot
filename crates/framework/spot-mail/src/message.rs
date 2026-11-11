//! The outgoing message model (WP-PLT-11.T1): validated recipients and extra headers, bodies,
//! attachments and inline images, plus threading and message-id plumbing. MIME encoding lives in
//! [`crate::mime`].

use std::fmt;
use std::time::SystemTime;

use lettre::Address;
use lettre::message::header::ContentType;
use uuid::Uuid;
use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::error::{MailError, PermanentFailure};

/// A secret that redacts itself in `Debug` and erases itself on drop (`zeroize`).
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct Secret(String);

impl Secret {
    /// Wraps a secret value.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Reads the secret. The caller must not log or copy it into durable storage.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad("[REDACTED]")
    }
}

/// Headers the message model owns: extra headers may not restate these.
const STRUCTURAL_HEADERS: &[&str] = &[
    "from",
    "sender",
    "reply-to",
    "to",
    "cc",
    "bcc",
    "subject",
    "date",
    "message-id",
    "in-reply-to",
    "references",
    "mime-version",
    "content-type",
    "content-transfer-encoding",
    "content-id",
    "content-disposition",
];

/// A validated RFC 5322 header field name for extra headers on `OutgoingMessage`. Names are
/// validated against the header grammar and may not be structural headers (those are set through
/// the typed fields of [`OutgoingMessage`]).
#[derive(Debug, Clone)]
pub struct HeaderName(pub(crate) lettre::message::header::HeaderName);

impl PartialEq for HeaderName {
    fn eq(&self, other: &Self) -> bool {
        self.as_str().eq_ignore_ascii_case(other.as_str())
    }
}

impl Eq for HeaderName {}

impl HeaderName {
    /// Validates and builds a header name.
    pub fn new(name: impl AsRef<str>) -> Result<Self, MailError> {
        let name = name.as_ref();
        if STRUCTURAL_HEADERS.contains(&name.to_ascii_lowercase().as_str()) {
            return Err(PermanentFailure::InvalidMessage(format!(
                "header {name:?} is structural and set through the message fields"
            ))
            .into());
        }
        let inner =
            lettre::message::header::HeaderName::new_from_ascii(name.to_owned()).map_err(|e| {
                PermanentFailure::InvalidMessage(format!("invalid header name {name:?}: {e}"))
            })?;
        Ok(Self(inner))
    }

    /// The header name as it appears on the wire.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for HeaderName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.as_str())
    }
}

/// A mail sender or recipient: optional display name plus a validated address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mailbox {
    /// The RFC 2047 display name, when present.
    pub name: Option<String>,
    /// The validated address.
    pub address: Address,
}

impl Mailbox {
    /// Builds and validates a mailbox.
    pub fn new(name: Option<String>, address: impl AsRef<str>) -> Result<Self, MailError> {
        if let Some(name) = &name
            && name.contains(['\r', '\n'])
        {
            return Err(PermanentFailure::InvalidMessage(
                "display name must be a single line".to_owned(),
            )
            .into());
        }
        Ok(Self {
            name,
            address: validate_address(address)?,
        })
    }
}

impl fmt::Display for Mailbox {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.name {
            Some(name) => write!(f, "{name} <{}>", self.address),
            None => write!(f, "{}", self.address),
        }
    }
}

/// A file attached to the message (`multipart/mixed`). Non-ASCII file names are RFC 2231 encoded
/// on the wire. The same type models attachments of ingested mail, where `content_id` marks
/// inline parts referenced by `cid:` links.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    /// The file name as presented to the recipient.
    pub filename: String,
    /// The parsed content type, for example `application/pdf`.
    pub content_type: ContentType,
    /// The `Content-ID` of an inline part, without the surrounding angle brackets, when the
    /// part is referenced from the HTML body; `None` for ordinary attachments.
    pub content_id: Option<String>,
    /// The file content.
    pub bytes: Vec<u8>,
}

impl Attachment {
    /// Builds an attachment, parsing the content type from its string form.
    pub fn new(
        filename: impl Into<String>,
        content_type: impl AsRef<str>,
        bytes: Vec<u8>,
    ) -> Result<Self, MailError> {
        let filename = filename.into();
        if filename.is_empty() {
            return Err(PermanentFailure::InvalidMessage(
                "attachment filename must not be empty".to_owned(),
            )
            .into());
        }
        let ct = content_type.as_ref();
        let content_type = ContentType::parse(ct).map_err(|e| {
            PermanentFailure::InvalidMessage(format!("invalid attachment content type {ct:?}: {e}"))
        })?;
        Ok(Self {
            filename,
            content_type,
            content_id: None,
            bytes,
        })
    }

    /// Sets the attachment's `Content-ID` (angle brackets stripped), marking it inline.
    #[must_use]
    pub fn with_content_id(mut self, content_id: impl Into<Option<String>>) -> Self {
        self.content_id = content_id.into();
        self
    }
}

/// An inline image referenced by `cid:` links from the HTML body (`multipart/related`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineImage {
    /// The `Content-ID` the HTML body references, without angle brackets.
    pub content_id: String,
    /// The parsed content type, for example `image/png`.
    pub content_type: ContentType,
    /// The image content.
    pub bytes: Vec<u8>,
}

impl InlineImage {
    /// Builds an inline image, parsing the content type from its string form.
    pub fn new(
        content_id: impl Into<String>,
        content_type: impl AsRef<str>,
        bytes: Vec<u8>,
    ) -> Result<Self, MailError> {
        let content_id = content_id.into();
        if content_id.is_empty() {
            return Err(PermanentFailure::InvalidMessage(
                "inline image content_id must not be empty".to_owned(),
            )
            .into());
        }
        let ct = content_type.as_ref();
        let content_type = ContentType::parse(ct).map_err(|e| {
            PermanentFailure::InvalidMessage(format!("invalid inline content type {ct:?}: {e}"))
        })?;
        Ok(Self {
            content_id,
            content_type,
            bytes,
        })
    }
}

/// An outgoing message: validated on the wire through this model, encoded by [`crate::mime`] and
/// sent by a [`crate::MailTransport`]. It knows nothing about documents or accounts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutgoingMessage {
    /// The sender address.
    pub from: Mailbox,
    /// Overrides the default `Reply-To` target if it must differ from `from`.
    pub reply_to: Option<Mailbox>,
    /// Primary recipients.
    pub to: Vec<Mailbox>,
    /// Carbon copy recipients, visible to all.
    pub cc: Vec<Mailbox>,
    /// Blind carbon copy recipients: hidden from the MIME headers and from other recipients.
    pub bcc: Vec<Mailbox>,
    /// The subject line; non-ASCII is RFC 2047 encoded.
    pub subject: String,
    /// The `text/plain` alternative.
    pub text: Option<String>,
    /// The `text/html` alternative.
    pub html: Option<String>,
    /// Files attached as `multipart/mixed` parts.
    pub attachments: Vec<Attachment>,
    /// Images inlined into the HTML as `multipart/related` parts.
    pub inline: Vec<InlineImage>,
    /// The `Message-ID` this message answers, without angle brackets.
    pub in_reply_to: Option<String>,
    /// The full `References` chain, oldest first, without angle brackets.
    pub references: Vec<String>,
    /// Extra headers, after structural headers are rejected by [`HeaderName`].
    pub headers: Vec<(HeaderName, String)>,
    /// The message id, without angle brackets. Generated as `<uuidv7@domain-of-from>` when absent.
    pub message_id: Option<String>,
    /// The RFC 5322 `Date` originator timestamp. Stamped with the build time by lettre when
    /// absent; set it explicitly when the bytes on the wire must be reproducible, for example in
    /// tests that rebuild the MIME for comparison and when storing sent copies.
    pub date: Option<SystemTime>,
}

impl OutgoingMessage {
    /// Starts a message from `from` with `subject`; every other field starts empty.
    pub fn new(from: Mailbox, subject: impl Into<String>) -> Self {
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
            message_id: None,
            date: None,
        }
    }

    /// The message id used on the wire: the explicit one, or a generated uuid v7 under the
    /// domain of the from address. Drifts between calls when none is set.
    pub fn resolved_message_id(&self) -> String {
        match &self.message_id {
            Some(id) => id.clone(),
            None => format!("{}@{}", Uuid::now_v7(), self.from.address.domain()),
        }
    }

    /// All envelope recipients: `to`, then `cc`, then `bcc`.
    pub fn recipients(&self) -> impl Iterator<Item = &Mailbox> {
        self.to.iter().chain(self.cc.iter()).chain(self.bcc.iter())
    }

    /// Validates the message for sending: at least one body part, at least one recipient,
    /// single-line subject and header values, and bare ids without angle brackets in threading
    /// and message-id fields.
    pub fn validate(&self) -> Result<(), MailError> {
        let mut problems: Vec<String> = Vec::new();
        if self.text.is_none() && self.html.is_none() {
            problems.push("either text or html is required".to_owned());
        }
        if self.recipients().next().is_none() {
            problems.push("at least one recipient in to, cc or bcc is required".to_owned());
        }
        let mailboxes = std::iter::once(&self.from)
            .chain(self.reply_to.iter())
            .chain(self.recipients());
        for mailbox in mailboxes {
            if let Some(name) = &mailbox.name
                && name.contains(['\r', '\n'])
            {
                problems.push(format!(
                    "display name for {} must be a single line",
                    mailbox.address
                ));
            }
        }
        if self.subject.contains(['\r', '\n']) {
            problems.push("subject must be a single line".to_owned());
        }
        for (name, value) in &self.headers {
            if value.contains(['\r', '\n']) {
                problems.push(format!("header {name} value must be a single line"));
            }
        }
        for id in self.references.iter().chain(self.in_reply_to.iter()) {
            if id.contains(['\r', '\n', '<', '>']) {
                problems.push(format!(
                    "threading id {id:?} must be a bare id without angle brackets"
                ));
            }
        }
        if let Some(id) = &self.message_id
            && id.contains(['\r', '\n', '<', '>'])
        {
            problems.push(format!(
                "message id {id:?} must be a bare id without angle brackets"
            ));
        }
        match problems.is_empty() {
            true => Ok(()),
            false => Err(PermanentFailure::InvalidMessage(problems.join("; ")).into()),
        }
    }
}

/// Parses and validates an address string: ASCII only, the lettre address grammar.
pub fn validate_address(address: impl AsRef<str>) -> Result<Address, MailError> {
    let raw = address.as_ref();
    if !raw.is_ascii() {
        return Err(PermanentFailure::InvalidMessage(format!(
            "address {raw:?} contains non-ASCII characters"
        ))
        .into());
    }
    raw.parse::<Address>().map_err(|e| {
        PermanentFailure::InvalidMessage(format!("invalid address {raw:?}: {e}")).into()
    })
}
