//! Normalizing fetched RFC 5322 bytes into [`IncomingMessage`] fields (split from the parent
//! module to keep each file reviewable).

use mail_parser::{HeaderName, Message, MessageParser, MimeHeaders, PartType};

use super::{IncomingMessage, MailAddress};

/// Converts raw or header-only RFC 5322 bytes into the fields of an [`IncomingMessage`]
/// (everything except `uid`, `raw_size`, `truncated`). Header-only normalization yields no
/// bodies and no attachments. Unparsable input yields `None`, which the caller treats as a
/// transient fetch failure rather than a silently dropped message.
pub(super) fn normalize(bytes: &[u8], headers_only: bool) -> Option<IncomingMessage> {
    let parsed = MessageParser::default().parse(bytes)?;
    let message_id = parsed.message_id().map(str::to_owned);
    let in_reply_to = parsed.in_reply_to().as_text().map(str::to_owned);
    let references = parsed
        .references()
        .as_text_list()
        .map(|ids| ids.iter().map(|id| id.to_string()).collect::<Vec<_>>())
        .unwrap_or_default();
    // `text_part`/`html_part` return the actual part, unlike `body_text`, which would
    // synthesize text from HTML when `text/plain` is missing — the caller must not get a
    // silently converted body ("do not strip HTML here").
    let (text, html, attachments) = if headers_only {
        (None, None, Vec::new())
    } else {
        let text = parsed.text_part(0).and_then(|part| match &part.body {
            PartType::Text(text) => Some(text.to_string()),
            _ => None,
        });
        let html = parsed.html_part(0).and_then(|part| match &part.body {
            PartType::Html(html) => Some(html.to_string()),
            _ => None,
        });
        (text, html, body_attachments(&parsed))
    };
    Some(IncomingMessage {
        uid: 0,
        raw_size: 0,
        truncated: false,
        message_id,
        in_reply_to,
        references,
        from: convert_address(parsed.from()).into_iter().next(),
        to: convert_address(parsed.to()),
        cc: convert_address(parsed.cc()),
        reply_to: convert_address(parsed.reply_to()),
        subject: parsed.subject().map(str::to_owned),
        date: parsed.date().map(|date| date.to_timestamp()),
        text,
        html,
        attachments,
        auto_submitted: is_auto_submitted(&parsed),
    })
}

/// A file name for attachments that announce none; downstream still gets the bytes.
const FALLBACK_FILENAME: &str = "attachment";

/// Collects the message's attachments into T1's [`crate::message::Attachment`] shape: decoded
/// bytes, the disposition or MIME-type file name (RFC 2231 continuations are decoded by
/// mail-parser), and the `Content-ID` (angle brackets stripped) on inline parts.
fn body_attachments(parsed: &Message<'_>) -> Vec<crate::message::Attachment> {
    parsed
        .attachments()
        .map(|part| {
            let bytes = match &part.body {
                PartType::Binary(bytes) | PartType::InlineBinary(bytes) => bytes.to_vec(),
                PartType::Text(text) | PartType::Html(text) => text.to_string().into_bytes(),
                PartType::Message(nested) => nested.raw_message.to_vec(),
                PartType::Multipart(_) => Vec::new(),
            };
            let content_type = part
                .content_type()
                .map(|ct| match &ct.c_subtype {
                    Some(subtype) => format!("{}/{}", ct.c_type, subtype),
                    None => ct.c_type.to_string(),
                })
                .unwrap_or_else(|| "application/octet-stream".to_owned());
            let content_id = part
                .content_id()
                .map(|id| id.trim_matches(['<', '>']).to_owned());
            let filename = part
                .attachment_name()
                .map(str::to_owned)
                .unwrap_or_else(|| FALLBACK_FILENAME.to_owned());
            crate::message::Attachment {
                filename,
                content_type: parse_content_type(&content_type),
                content_id,
                bytes,
            }
        })
        .collect()
}

/// Parses a content type for T1's [`crate::message::Attachment`], falling back to
/// `application/octet-stream` when the server sent something lettre's parser cannot take.
fn parse_content_type(value: &str) -> lettre::message::header::ContentType {
    const FALLBACK: &str = "application/octet-stream";
    if let Ok(parsed) = value.parse() {
        return parsed;
    }
    if let Ok(parsed) = FALLBACK.parse() {
        return parsed;
    }
    lettre::message::header::ContentType::TEXT_PLAIN
}

/// Converts a mail-parser address header into normalized addresses; RFC 5322 groups
/// (`undisclosed-recipients:;`) are flattened into their member addresses.
fn convert_address(address: Option<&mail_parser::Address<'_>>) -> Vec<MailAddress> {
    match address {
        None => Vec::new(),
        Some(mail_parser::Address::List(members)) => {
            members.iter().filter_map(addr_member).collect()
        }
        Some(mail_parser::Address::Group(groups)) => groups
            .iter()
            .flat_map(|group| group.addresses.iter())
            .filter_map(addr_member)
            .collect(),
    }
}

/// One mail-parser address to one normalized address; entries without a usable address
/// (group marker lines) are dropped.
fn addr_member(addr: &mail_parser::Addr<'_>) -> Option<MailAddress> {
    Some(MailAddress {
        name: addr.name.as_deref().map(str::to_owned),
        address: addr.address.as_deref()?.to_owned(),
    })
}

/// Whether the message announces automated origination per the brief: an `Auto-Submitted`
/// header with a value other than `no`, or `Precedence` set to `bulk` or `auto_reply`. Callers
/// use this flag so auto-replies never trigger auto-replies.
fn is_auto_submitted(parsed: &Message<'_>) -> bool {
    let auto_submitted = parsed
        .header(HeaderName::AutoSubmitted)
        .and_then(|value| value.as_text())
        .is_some_and(|value| !value.trim().eq_ignore_ascii_case("no"));
    let precedence = parsed
        .header(HeaderName::Other("precedence".into()))
        .and_then(|value| value.as_text())
        .map(str::trim)
        .is_some_and(|value| matches!(value.to_ascii_lowercase().as_str(), "bulk" | "auto_reply"));
    auto_submitted || precedence
}

// [imap-part-10]
