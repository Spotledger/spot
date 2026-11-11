//! MIME encoding for outgoing messages (WP-PLT-11.T1), pure: headers, bodies, attachments and
//! inline images are composed with lettre's message builder. The encoded bytes are exactly what
//! a transport hands to the server after dot-stuffing, and what tests parse back.

use lettre::Message;
use lettre::address::Envelope;
use lettre::message::Attachment as LettreAttachment;
use lettre::message::Body as LettreBody;
use lettre::message::Mailbox as LettreMailbox;
use lettre::message::header::ContentTransferEncoding as LettreContentTransferEncoding;
use lettre::message::header::HeaderValue as LettreHeaderValue;
use lettre::message::{MultiPart, SinglePart};

use crate::error::{MailError, PermanentFailure};
use crate::message::{Attachment, InlineImage, Mailbox, OutgoingMessage};

/// Encodes a validated message as MIME bytes with a generated message id
/// (`<uuid-v7@from-domain>`). Use [`build_mime_with_id`] when the id on the wire must match the
/// one reported in the receipt.
pub fn build_mime(message: &OutgoingMessage) -> Vec<u8> {
    let message_id = message.resolved_message_id();
    build_mime_with_id(message, &message_id)
}

/// Encodes a validated message as MIME bytes with a given message id (without angle brackets),
/// so the id on the wire matches the one a transport reports in its receipt.
pub fn build_mime_with_id(message: &OutgoingMessage, message_id: &str) -> Vec<u8> {
    match build_mime_result(message, message_id) {
        Ok(bytes) => bytes,
        // Unreachable for a modelled `OutgoingMessage`: lettre's builder only fails on a missing
        // or multi-`From` originator and on envelope derivation, all prevented by the message
        // model, and the envelope is provided explicitly. Failed encodes never surface as an
        // empty byte stream in tests, so keep the signal visible in logs.
        Err(error) => {
            tracing::error!(%error, "unreachable: lettre builder rejected a modelled message");
            Vec::new()
        }
    }
}

fn build_mime_result(message: &OutgoingMessage, message_id: &str) -> Result<Vec<u8>, MailError> {
    let mut builder = Message::builder()
        .from(convert_mailbox(&message.from))
        .message_id(Some(format!("<{message_id}>")))
        .subject(message.subject.clone())
        .envelope(build_envelope(message)?);
    if let Some(reply_to) = &message.reply_to {
        builder = builder.reply_to(convert_mailbox(reply_to));
    }
    for mailbox in &message.to {
        builder = builder.to(convert_mailbox(mailbox));
    }
    for mailbox in &message.cc {
        builder = builder.cc(convert_mailbox(mailbox));
    }
    if let Some(in_reply_to) = &message.in_reply_to {
        builder = builder.in_reply_to(format!("<{in_reply_to}>"));
    }
    if !message.references.is_empty() {
        builder = builder.references(
            message
                .references
                .iter()
                .map(|id| format!("<{id}>"))
                .collect::<Vec<_>>()
                .join(" "),
        );
    }
    for (name, value) in &message.headers {
        builder = builder.raw_header(LettreHeaderValue::new(name.0.clone(), value.clone()));
    }
    if let Some(date) = message.date {
        // An explicit `Date` keeps the encoded bytes reproducible; without one lettre stamps
        // the build time here, which makes two builds of the same message differ across a
        // second boundary.
        builder = builder.date(date);
    }
    match assemble_body(message)? {
        LeafBody::Body(body) => builder.body(body),
        LeafBody::Single(part) => builder.singlepart(part),
        LeafBody::Multi(part) => builder.multipart(part),
    }
    .map(|built| built.formatted())
    .map_err(|e| PermanentFailure::InvalidMessage(format!("MIME encoding failed: {e}")).into())
}

fn build_envelope(message: &OutgoingMessage) -> Result<Envelope, MailError> {
    let recipients = message
        .recipients()
        .map(|mailbox| mailbox.address.clone())
        .collect();
    Envelope::new(Some(message.from.address.clone()), recipients).map_err(|e| {
        PermanentFailure::InvalidMessage(format!("envelope rejected by lettre: {e}")).into()
    })
}

fn convert_mailbox(mailbox: &Mailbox) -> LettreMailbox {
    LettreMailbox {
        name: mailbox.name.clone(),
        email: mailbox.address.clone(),
    }
}

/// The body shape: a single body, a single MIME part, or a MIME sub-tree.
enum LeafBody {
    /// A bare `text/plain` body, pre-encoded by lettre.
    Body(LettreBody),
    /// One part, plain or html.
    Single(SinglePart),
    /// An assembled part sub-tree (alternative, related or mixed).
    Multi(MultiPart),
}

/// A bare text body as lettre encodes it: ASCII text is `7bit`, non-ASCII text is passed
/// through verbatim as `8bit` content so the SMTP transport can declare `BODY=8BITMIME`
/// instead of lettre's automatic quoted-printable, which would silently make the message
/// ASCII. A body that cannot be represented as 8-bit content (for example one very long
/// line) is a hard model failure.
fn text_body(text: &str) -> Result<LettreBody, MailError> {
    if text.is_ascii() {
        return Ok(LettreBody::new(text.to_owned()));
    }
    LettreBody::new_with_encoding(text.to_owned(), LettreContentTransferEncoding::EightBit).map_err(
        |_| {
            PermanentFailure::InvalidMessage(
                "text body cannot be represented as 8-bit content".to_owned(),
            )
            .into()
        },
    )
}

/// Assembles the body sub-tree: bare body, alternative text/html, inline images as related
/// parts, attachments as mixed parts.
fn assemble_body(message: &OutgoingMessage) -> Result<LeafBody, MailError> {
    let core = match (message.text.as_deref(), message.html.as_deref()) {
        (Some(text), None) => LeafBody::Body(text_body(text)?),
        (None, Some(html)) => LeafBody::Single(SinglePart::html(html.to_owned())),
        (Some(text), Some(html)) => LeafBody::Multi(MultiPart::alternative_plain_html(
            text.to_owned(),
            html.to_owned(),
        )),
        (None, None) => {
            return Err(PermanentFailure::InvalidMessage(
                "either text or html is required".to_owned(),
            )
            .into());
        }
    };
    if message.inline.is_empty() && message.attachments.is_empty() {
        return Ok(core);
    }
    if message.attachments.is_empty() {
        let related = match related_or_core(core, &message.inline)? {
            // `related_or_core` wraps the core whenever there are inline images, so this arm
            // always matches (the empty-inline case returned `core` above).
            LeafBody::Multi(part) => part,
            LeafBody::Body(_) | LeafBody::Single(_) => {
                return Err(PermanentFailure::InvalidMessage(
                    "no related part assembled".to_owned(),
                )
                .into());
            }
        };
        return Ok(LeafBody::Multi(related));
    }
    let mut mixed = match related_or_core(core, &message.inline)? {
        LeafBody::Body(body) => MultiPart::mixed().singlepart(SinglePart::plain(body)),
        LeafBody::Single(part) => MultiPart::mixed().singlepart(part),
        LeafBody::Multi(part) => MultiPart::mixed().multipart(part),
    };
    for attachment in &message.attachments {
        mixed = mixed.singlepart(attachment_part(attachment));
    }
    Ok(LeafBody::Multi(mixed))
}

/// Wraps the core body and the inline images into a `multipart/related` tree, with the core as
/// the root part the HTML references.
fn related_or_core(core: LeafBody, inline: &[InlineImage]) -> Result<LeafBody, MailError> {
    if inline.is_empty() {
        return Ok(core);
    }
    let mut related = match core {
        LeafBody::Body(text) => MultiPart::related().singlepart(SinglePart::plain(text)),
        LeafBody::Single(part) => MultiPart::related().singlepart(part),
        LeafBody::Multi(part) => MultiPart::related().multipart(part),
    };
    for image in inline {
        related = related.singlepart(inline_part(image));
    }
    Ok(LeafBody::Multi(related))
}

fn attachment_part(attachment: &Attachment) -> SinglePart {
    LettreAttachment::new(attachment.filename.clone())
        .body(attachment.bytes.clone(), attachment.content_type.clone())
}

fn inline_part(image: &InlineImage) -> SinglePart {
    LettreAttachment::new_inline(image.content_id.clone())
        .body(image.bytes.clone(), image.content_type.clone())
}
