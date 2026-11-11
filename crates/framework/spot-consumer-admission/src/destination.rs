//! Canonical destinations: the one string a bucket is keyed by, whatever spelling arrived.
//!
//! The throttle digests the string it is given, so the front door has to decide what "the same
//! destination" means. This module answers that, and nothing else:
//!
//! # E-mail
//!
//! 1. surrounding whitespace is trimmed;
//! 2. anything that is not ASCII is refused (`bad_destination`): there is no NFC normalisation
//!    here, and refusing is the only honest answer for a local part this crate cannot canonicalise;
//! 3. the address must split into a non-empty local part and a non-empty domain at one `@`;
//! 4. the local part must be RFC 5322 `atext` and dots, and is lower-cased;
//! 5. a `+tag` suffix is removed from the local part **only** when `strip_plus_tags` is set
//!    (`AdmissionPolicy::strip_plus_tags`, on by default). A local part that is nothing but a tag
//!    is refused;
//! 6. the domain must be dot-separated ASCII labels and is lower-cased.
//!
//! No provider rules are applied: dots are **not** dropped, so `a.b@example.com` and
//! `ab@example.com` stay two destinations. That is deliberate — dropping them is a Gmail rule and
//! would refuse to send to a real `a.b@` mailbox at any other provider.
//!
//! # Phone
//!
//! A leading `+`, then digits with spaces, dashes and parentheses removed, and 8 to 15 digits
//! (the E.164 length). Anything else is `bad_destination`; this crate never guesses a country
//! code, so `03001234567` is refused rather than assumed to be Pakistani.
//!
//! # The properties the tests pin
//!
//! Canonicalisation is **idempotent** (canonicalising a canonical form gives the same bytes) and
//! never panics on arbitrary input: it either answers a canonical form or `bad_destination`.

use std::fmt;

use crate::error::AdmissionError;

/// The longest destination accepted before canonicalisation, in bytes.
pub const MAX_DESTINATION_BYTES: usize = 320;

/// The longest domain accepted, in bytes (RFC 1035).
const MAX_DOMAIN_BYTES: usize = 253;

/// A destination as the caller received it.
///
/// It is never logged or returned: [`Destination`] prints `<redacted>`, because the raw string a
/// person typed is personal data.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Destination<'a> {
    /// An e-mail address, as typed.
    Email(&'a str),
    /// A phone number, as typed.
    Phone(&'a str),
}

impl Destination<'_> {
    /// Which kind this is, without exposing the value.
    pub fn kind(&self) -> DestinationKind {
        match self {
            Self::Email(_) => DestinationKind::Email,
            Self::Phone(_) => DestinationKind::Phone,
        }
    }
}

impl fmt::Debug for Destination<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Email(_) => f.write_str("Destination::Email(<redacted>)"),
            Self::Phone(_) => f.write_str("Destination::Phone(<redacted>)"),
        }
    }
}

/// Whether a canonical destination is an e-mail address or a phone number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DestinationKind {
    /// An e-mail address.
    Email,
    /// A phone number in `+`-prefixed E.164 shape.
    Phone,
}

/// A destination in the one form this crate and the throttle count.
///
/// The value is the string that is digested (through the throttle) and the string a code should
/// actually be sent to. `Debug` and `Display` are redacted: use [`CanonicalDestination::as_str`]
/// when the caller needs the bytes.
#[derive(Clone, PartialEq, Eq)]
pub struct CanonicalDestination {
    kind: DestinationKind,
    value: String,
}

impl CanonicalDestination {
    /// The canonical bytes: `local@domain` in lower case, or `+` and the digits.
    pub fn as_str(&self) -> &str {
        &self.value
    }

    /// Which kind this is.
    pub fn kind(&self) -> DestinationKind {
        self.kind
    }

    /// The canonical bytes, owned.
    pub fn into_string(self) -> String {
        self.value
    }
}

impl fmt::Debug for CanonicalDestination {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The kind is an operational fact; the address is personal data.
        write!(f, "CanonicalDestination({:?}, <redacted>)", self.kind)
    }
}

impl fmt::Display for CanonicalDestination {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CanonicalDestination(<redacted>)")
    }
}

/// The canonical form of `destination`, or `bad_destination` for anything this crate will not
/// count.
///
/// See the module documentation for the exact rules. `strip_plus_tags` is the policy flag:
/// `true` makes `alice+x@example.com` the same destination as `alice@example.com`.
pub fn canonicalise(
    destination: Destination<'_>,
    strip_plus_tags: bool,
) -> Result<CanonicalDestination, AdmissionError> {
    match destination {
        Destination::Email(raw) => email(raw, strip_plus_tags),
        Destination::Phone(raw) => phone(raw),
    }
}

/// The e-mail rules of the module documentation.
fn email(raw: &str, strip_plus_tags: bool) -> Result<CanonicalDestination, AdmissionError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(bad("the destination is empty"));
    }
    if trimmed.len() > MAX_DESTINATION_BYTES {
        return Err(bad("the destination is too long"));
    }
    if !trimmed.is_ascii() {
        return Err(bad(
            "the destination is not ASCII; a non-ASCII local part or domain is not accepted",
        ));
    }
    let Some((local, domain)) = trimmed.rsplit_once('@') else {
        return Err(bad("an e-mail destination needs one @"));
    };
    if local.is_empty() || domain.is_empty() {
        return Err(bad("an e-mail destination needs a local part and a domain"));
    }
    if local.contains('@') {
        return Err(bad("an e-mail destination has more than one unquoted @"));
    }
    if !local.bytes().all(is_local_byte) {
        return Err(bad(
            "the local part has a byte an unquoted address cannot contain",
        ));
    }
    let lowered = local.to_ascii_lowercase();
    let local = if strip_plus_tags {
        match lowered.find('+') {
            // A local part that is only a tag leaves nothing to send to.
            Some(0) => return Err(bad("a plus tag cannot replace the local part")),
            Some(at) => &lowered[..at],
            None => lowered.as_str(),
        }
    } else {
        lowered.as_str()
    };
    if !valid_domain(domain) {
        return Err(bad("the domain is not a plain dot-separated ASCII domain"));
    }
    let domain = domain.to_ascii_lowercase();
    Ok(CanonicalDestination {
        kind: DestinationKind::Email,
        value: format!("{local}@{domain}"),
    })
}

/// The phone rules of the module documentation.
fn phone(raw: &str) -> Result<CanonicalDestination, AdmissionError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(bad("the destination is empty"));
    }
    if trimmed.len() > MAX_DESTINATION_BYTES {
        return Err(bad("the destination is too long"));
    }
    if !trimmed.starts_with('+') {
        return Err(bad(
            "a phone destination must start with + and a country code",
        ));
    }
    let mut digits = String::with_capacity(trimmed.len());
    for (index, byte) in trimmed.bytes().enumerate() {
        match byte {
            b'+' if index == 0 => {}
            b'0'..=b'9' => digits.push(char::from(byte)),
            b' ' | b'-' | b'(' | b')' => {}
            _ => {
                return Err(bad(
                    "a phone destination carries only a leading +, digits, spaces, dashes and parentheses",
                ));
            }
        }
    }
    if !(8..=15).contains(&digits.len()) {
        return Err(bad(
            "a phone destination needs 8 to 15 digits after the leading + (E.164)",
        ));
    }
    Ok(CanonicalDestination {
        kind: DestinationKind::Phone,
        value: format!("+{digits}"),
    })
}

/// `atext` and `.`: what an unquoted local part may carry (RFC 5322 section 3.2.3).
fn is_local_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'/'
                | b'='
                | b'?'
                | b'^'
                | b'_'
                | b'`'
                | b'{'
                | b'|'
                | b'}'
                | b'~'
                | b'.'
        )
}

/// Dot-separated ASCII labels, 1 to 63 bytes each, no leading or trailing dash, at least two
/// labels (so a bare hostname cannot pass as a mailbox domain).
fn valid_domain(domain: &str) -> bool {
    if domain.len() > MAX_DOMAIN_BYTES {
        return false;
    }
    let labels: Vec<&str> = domain.split('.').collect();
    if labels.len() < 2 {
        return false;
    }
    labels.iter().all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            && !label.starts_with('-')
            && !label.ends_with('-')
    })
}

/// A `bad_destination` error with a fixed sentence.
fn bad(reason: &'static str) -> AdmissionError {
    AdmissionError::BadDestination { reason }
}
