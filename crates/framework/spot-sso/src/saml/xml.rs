//! XML plumbing shared by the SAML modules: safe parsing, namespace-aware lookup, text access
//! and the timestamp form used on the wire.
//!
//! Parsing is deliberately hostile to the entity and DTD family: a document carrying a
//! `<!DOCTYPE` or `<!ENTITY` declaration is refused before the parser sees it
//! ([`crate::error::codes::SAML_DTD_FORBIDDEN`]), and `roxmltree` is configured with
//! `allow_dtd = false` and a node limit as a second line of defence. External entities are
//! never resolved.

use base64::{Engine, engine::general_purpose::STANDARD};
use roxmltree::{Document, Node, ParsingOptions};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::error::{self as e, Result, SsoError};

/// The SAML 2.0 protocol namespace.
pub(crate) const NS_PROTOCOL: &str = "urn:oasis:names:tc:SAML:2.0:protocol";
/// The SAML 2.0 assertion namespace.
pub(crate) const NS_ASSERTION: &str = "urn:oasis:names:tc:SAML:2.0:assertion";
/// The SAML 2.0 metadata namespace.
pub(crate) const NS_METADATA: &str = "urn:oasis:names:tc:SAML:2.0:metadata";
/// The XML Signature namespace.
pub(crate) const NS_DS: &str = "http://www.w3.org/2000/09/xmldsig#";

/// The largest number of XML nodes a SAML message may hold, a bound on parser memory.
const NODES_LIMIT: u32 = 100_000;

/// Refuses a document that carries a DTD, an entity declaration or an external reference.
///
/// The check is a byte scan, so it runs before any parser. False positives (the token inside a
/// comment) are acceptable: nothing legitimate in SAML needs a DTD.
pub(crate) fn scan_forbidden_constructs(text: &str) -> Result<()> {
    for needle in [
        "<!DOCTYPE",
        "<!ENTITY",
        "<!ATTLIST",
        "<!ELEMENT",
        "<!NOTATION",
    ] {
        if text.contains(needle) {
            return Err(dtd_forbidden());
        }
    }
    Ok(())
}

/// Parses a SAML XML document, refusing any DTD and bounding the node count.
///
/// # Errors
///
/// [`e::codes::SAML_DTD_FORBIDDEN`] for a DTD or entity declaration;
/// [`e::codes::SAML_XML_INVALID`] for malformed XML or a limit reached.
pub(crate) fn parse_document(text: &str) -> Result<Document<'_>> {
    scan_forbidden_constructs(text)?;
    let options = ParsingOptions {
        allow_dtd: false,
        nodes_limit: NODES_LIMIT,
        entity_resolver: None,
    };
    match Document::parse_with_options(text, options) {
        Ok(document) => Ok(document),
        Err(roxmltree::Error::DtdDetected) => Err(dtd_forbidden()),
        Err(_) => Err(xml_invalid()),
    }
}

/// Whether `node` is an element with exactly this namespace and local name.
pub(crate) fn is_tag(node: Node<'_, '_>, namespace: &str, local: &str) -> bool {
    node.is_element()
        && node.tag_name().namespace() == Some(namespace)
        && node.tag_name().name() == local
}

/// The first child element of `parent` with this namespace and local name.
pub(crate) fn child<'a, 'input>(
    parent: Node<'a, 'input>,
    namespace: &str,
    local: &str,
) -> Option<Node<'a, 'input>> {
    parent
        .children()
        .find(|child| is_tag(*child, namespace, local))
}

/// Every child element of `parent` with this namespace and local name.
pub(crate) fn children<'a, 'input>(
    parent: Node<'a, 'input>,
    namespace: &str,
    local: &str,
) -> Vec<Node<'a, 'input>> {
    parent
        .children()
        .filter(|child| is_tag(*child, namespace, local))
        .collect()
}

/// The element's simple text value, refusing a value split by a comment, PI or child element.
///
/// This is the defence against XML comment injection (`<NameID>a<!---->.b</NameID>`): the text
/// a reader would concatenate is not the text the signature covered, so any split is refused.
///
/// # Errors
///
/// [`e::codes::SAML_COMMENT_IN_VALUE`] when a comment, PI or nested element splits the value.
pub(crate) fn simple_text(node: Node<'_, '_>) -> Result<String> {
    let mut value = String::new();
    for child in node.children() {
        if child.is_text() {
            value.push_str(child.text().unwrap_or(""));
        } else if child.is_element() {
            return Err(comment_in_value());
        } else {
            // A comment or processing instruction anywhere inside the value.
            return Err(comment_in_value());
        }
    }
    Ok(value)
}

/// The element's text value without the comment/PI refusal, for fields where a nested node is
/// not meaningful but the caller only needs what was seen (never used for identity).
pub(crate) fn concatenated_text(node: Node<'_, '_>) -> String {
    node.children()
        .filter(|child| child.is_text())
        .filter_map(|child| child.text())
        .collect()
}

/// Decodes Base64 with the standard alphabet, ignoring XML whitespace.
///
/// # Errors
///
/// [`e::codes::SAML_NOT_BASE64`] when the text is not standard Base64.
pub(crate) fn decode_base64(text: &str) -> Result<Vec<u8>> {
    let compact: String = text.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    if let Ok(bytes) = STANDARD.decode(compact.as_bytes()) {
        return Ok(bytes);
    }
    // Some senders omit padding; accept it, but still only the standard alphabet.
    let unpadded = compact.trim_end_matches('=');
    base64::engine::general_purpose::STANDARD_NO_PAD
        .decode(unpadded.as_bytes())
        .map_err(|_| not_base64())
}

/// Encodes bytes as standard Base64, no line breaks.
pub(crate) fn encode_base64(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

/// Escapes text for an XML character data position.
pub(crate) fn escape_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Escapes text for an XML attribute value.
pub(crate) fn escape_attribute(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(ch),
        }
    }
    out
}

/// Formats a unix instant as the SAML wire form `YYYY-MM-DDThh:mm:ssZ`.
#[must_use]
pub(crate) fn format_instant(seconds: i64) -> String {
    let Some(instant) = OffsetDateTime::from_unix_timestamp(seconds).ok() else {
        return "1970-01-01T00:00:00Z".to_owned();
    };
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        instant.year(),
        u8::from(instant.month()),
        instant.day(),
        instant.hour(),
        instant.minute(),
        instant.second()
    )
}

/// Parses a SAML `dateTime` (RFC 3339) into a unix instant.
#[must_use]
pub(crate) fn parse_instant(text: &str) -> Option<i64> {
    OffsetDateTime::parse(text, &Rfc3339)
        .ok()
        .map(OffsetDateTime::unix_timestamp)
}

fn dtd_forbidden() -> SsoError {
    SsoError::new(
        e::codes::SAML_DTD_FORBIDDEN,
        "The sign-in response could not be read.",
        "The identity provider sent a document with a DTD or entity declaration; SAML never uses one.",
    )
}

fn xml_invalid() -> SsoError {
    SsoError::new(
        e::codes::SAML_XML_INVALID,
        "The sign-in response could not be read.",
        "Check the identity provider sends well-formed SAML XML.",
    )
}

fn not_base64() -> SsoError {
    SsoError::new(
        e::codes::SAML_NOT_BASE64,
        "The sign-in response could not be read.",
        "Check the identity provider Base64-encodes the SAML response.",
    )
}

fn comment_in_value() -> SsoError {
    SsoError::new(
        e::codes::SAML_COMMENT_IN_VALUE,
        "The sign-in response was not trusted.",
        "An assertion value was split by a comment or processing instruction; refuse the message.",
    )
}
