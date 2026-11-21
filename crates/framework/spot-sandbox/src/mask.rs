//! Deterministic, keyed masking: what a closed field becomes, and the report that says how
//! much was masked without ever holding a value.
//!
//! # Why HMAC and a per-sandbox key
//!
//! A pseudonym must be stable inside one sandbox (the same customer is the same fake in every
//! document, journal row, audit diff and denormalised copy) and meaningless outside it. An
//! HMAC keyed with a per-sandbox secret gives both: the mapping is deterministic while the key
//! lives, and unusable once it is discarded. The key is caller-supplied — this crate reads no
//! clock and no random source — and wiped when the [`Masker`] drops.
//!
//! # Why amounts are never touched
//!
//! ADR-12 forbids floating point for money, and a masked ledger that no longer balances is
//! worse than useless: it hides the bug a sandbox exists to find. Numeric fields tagged
//! `financial` are therefore left exactly as they are. Masking keeps ledgers balanced because
//! it changes only identifiers and text. Numeric identifiers tagged `pii` or `health` **are**
//! masked, with the same digit count and no fractional arithmetic.
//!
//! # Shapes
//!
//! Format-preserving transformations are applied where a shape matters: e-mail (reserved
//! `example.invalid` domain), phone (leading `+`, separators and digit count), IBAN (country,
//! length and a recomputed mod-97 check). Everything else becomes an opaque `p-<hex>` token.

use std::fmt;

use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::Sha256;
use spot_meta::FieldType;
use zeroize::Zeroizing;

use crate::MASK_DOMAIN;
use crate::classify::FieldClass;
use crate::error::{Error, Result};
use crate::plan::MaskingPolicy;

type HmacSha256 = Hmac<Sha256>;

/// How a closed field is treated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MaskKind {
    /// An identifier (text or numeric): a deterministic pseudonym, shape preserved where it
    /// matters.
    Identifier,
    /// Long free text: scrubbed word by word to hash tokens.
    FreeText,
    /// An attachment: replaced with a placeholder.
    Attachment,
    /// A secret: removed entirely.
    Secret,
    /// A numeric ledger amount: **kept as it is**, so the ledger stays balanced.
    AmountKept,
    /// A link, a child table or an id: kept, because the identity is on the target document.
    LinkKept,
    /// The field is not closed and is not touched: only an explicit Keep decision of the
    /// administrator (recorded in the policy) or a classification the policy does not close.
    Unprotected,
}

impl MaskKind {
    /// Whether this kind changes the value. An attachment reference is kept in the document
    /// (the attachment row itself becomes a placeholder), and amounts and links are kept by
    /// design.
    #[must_use]
    pub fn masks(self) -> bool {
        matches!(
            self,
            MaskKind::Identifier | MaskKind::FreeText | MaskKind::Secret
        )
    }
}

/// Decides how `class` is treated under `policy`.
///
/// The order matters: a link is never masked (its id is the reference), a secret is stripped
/// next, an attachment becomes a placeholder, and a numeric `financial` field is an amount and
/// stays.
#[must_use]
pub fn kind_for(class: &FieldClass, policy: &MaskingPolicy) -> MaskKind {
    if class.is_link() {
        return MaskKind::LinkKept;
    }
    if !class.is_closed(policy) {
        return MaskKind::Unprotected;
    }
    if class.ty == FieldType::Secret || class.tags.contains("secret") {
        return MaskKind::Secret;
    }
    if class.ty == FieldType::Attach {
        return MaskKind::Attachment;
    }
    if class.tags.contains("financial") && matches!(class.ty, FieldType::Decimal | FieldType::Int) {
        return MaskKind::AmountKept;
    }
    match class.ty {
        FieldType::LongText | FieldType::Json => MaskKind::FreeText,
        FieldType::Text | FieldType::Select | FieldType::Int | FieldType::Decimal => {
            MaskKind::Identifier
        }
        FieldType::Date | FieldType::Time | FieldType::DateTime => MaskKind::Identifier,
        // A position or a booking period is personal data: never left unprotected.
        FieldType::Bool | FieldType::Bytes | FieldType::Geo | FieldType::Range => {
            MaskKind::Identifier
        }
        FieldType::Link
        | FieldType::DynamicLink
        | FieldType::Table
        | FieldType::TableMultiselect
        | FieldType::Attach
        | FieldType::Secret => MaskKind::LinkKept,
    }
}

/// The per-sandbox key. It never appears in a record, an error or a report, and the bytes are
/// zeroed when the masker drops.
pub struct SandboxKey(Zeroizing<[u8; 32]>);

impl SandboxKey {
    /// Wraps 32 bytes of caller-supplied key material.
    #[must_use]
    pub fn new(bytes: [u8; 32]) -> Self {
        Self(Zeroizing::new(bytes))
    }

    /// Parses a 64-character hex key. A short or malformed key is refused; the text is never
    /// echoed in the error.
    ///
    /// # Errors
    /// [`Error::Mask`] when the text is not 64 hex characters.
    pub fn from_hex(text: &str) -> Result<Self> {
        if text.len() != 64 {
            return Err(Error::Mask {
                reason: "the sandbox key must be 64 hex characters".to_owned(),
            });
        }
        let mut bytes = [0u8; 32];
        for (index, slot) in bytes.iter_mut().enumerate() {
            let pair = text
                .get(index * 2..index * 2 + 2)
                .ok_or_else(|| Error::Mask {
                    reason: "the sandbox key is not hex".to_owned(),
                })?;
            *slot = u8::from_str_radix(pair, 16).map_err(|_| Error::Mask {
                reason: "the sandbox key is not hex".to_owned(),
            })?;
        }
        Ok(Self::new(bytes))
    }

    /// The key bytes, for a host that has to hand the same key to the masker. Not exported in
    /// any record.
    #[must_use]
    pub fn expose(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for SandboxKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SandboxKey(<redacted>)")
    }
}

/// What one masking run did, by count. It carries no value, no key and no digest.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MaskReport {
    /// Documents read.
    pub documents: usize,
    /// Fields whose value changed.
    pub fields_masked: usize,
    /// Denormalised copies masked because their source was masked.
    pub derived_masked: usize,
    /// Numeric ledger amounts deliberately kept.
    pub amounts_kept: usize,
    /// Link or table fields deliberately kept.
    pub links_kept: usize,
    /// Free-text values scrubbed to hash tokens.
    pub free_text_scrubbed: usize,
    /// Journal rows whose value changed.
    pub journal_masked: usize,
    /// Audit rows whose before/after changed.
    pub audit_masked: usize,
    /// Attachments replaced with a placeholder.
    pub attachments_replaced: usize,
    /// Secrets removed.
    pub secrets_stripped: usize,
    /// Integrations removed.
    pub integrations_stripped: usize,
    /// Cache entries purged.
    pub caches_purged: usize,
    /// Search rows purged (index and embeddings).
    pub search_rows_purged: usize,
    /// Whether the ledger's amounts were proved unchanged after masking.
    pub ledger_checked: bool,
    /// Always `true`: the key is wiped when the run ends.
    pub key_discarded: bool,
}

/// Masks values with one sandbox key. Construct it, call it, drop it: the key is wiped with
/// the value.
pub struct Masker {
    key: SandboxKey,
}

impl fmt::Debug for Masker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Masker { key: SandboxKey(<redacted>) }")
    }
}

impl Masker {
    /// A masker over `key`.
    #[must_use]
    pub fn new(key: SandboxKey) -> Self {
        Self { key }
    }

    /// The raw HMAC digest of `domain` and `value`. The domain is the value's *shape*, never
    /// the field name, so a denormalised copy and its source produce the same pseudonym.
    fn digest(&self, domain: &str, value: &str, counter: u8) -> [u8; 32] {
        // HMAC accepts a key of any length, so the error arm is unreachable; a zero digest
        // would still be deterministic, never a panic.
        let Ok(mut mac) = HmacSha256::new_from_slice(self.key.expose()) else {
            return [0u8; 32];
        };
        mac.update(MASK_DOMAIN);
        mac.update(&[0]);
        mac.update(domain.as_bytes());
        mac.update(&[0]);
        mac.update(value.as_bytes());
        mac.update(&[counter]);
        let bytes = mac.finalize().into_bytes();
        let mut out = [0u8; 32];
        out.copy_from_slice(&bytes);
        out
    }

    /// Enough digest bytes for a transformation of `len` characters, by counter.
    fn stream(&self, domain: &str, value: &str, len: usize) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(len.max(32));
        let mut counter = 0u8;
        while bytes.len() < len {
            bytes.extend_from_slice(&self.digest(domain, value, counter));
            counter = counter.wrapping_add(1);
        }
        bytes
    }

    /// A stable opaque token: `p-` and 16 lower-case hex characters.
    #[must_use]
    pub fn token(&self, domain: &str, value: &str) -> String {
        let digest = self.digest(domain, value, 0);
        format!(
            "p-{}{}{}{}",
            hex(&digest[0..2]),
            hex(&digest[2..4]),
            hex(&digest[4..6]),
            hex(&digest[6..8])
        )
    }

    /// A short content tag used for attachment names and hashes.
    #[must_use]
    pub fn tag(&self, domain: &str, value: &str) -> String {
        let digest = self.digest(domain, value, 0);
        hex(&digest[..8])
    }

    /// The masked form of one value under `kind`.
    ///
    /// Amounts, links and unprotected fields are returned unchanged. Values that are not JSON
    /// strings are handled without floating point: a number keeps its digit count and type
    /// where possible.
    #[must_use]
    pub fn mask_value(&self, kind: MaskKind, value: &Value, placeholder: &str) -> Value {
        match kind {
            MaskKind::Unprotected | MaskKind::AmountKept | MaskKind::LinkKept => value.clone(),
            MaskKind::Secret => Value::Null,
            MaskKind::Attachment => Value::String(placeholder.to_owned()),
            MaskKind::FreeText => match value {
                Value::String(text) => Value::String(self.scrub(text)),
                other => Value::String(self.token("text", &scalar_text(other))),
            },
            MaskKind::Identifier => match value {
                Value::String(text) => Value::String(self.mask_text(text)),
                Value::Number(number) => self.mask_number(number),
                Value::Bool(flag) => Value::Bool(*flag),
                Value::Null => Value::Null,
                other => Value::String(self.token("text", &scalar_text(other))),
            },
        }
    }

    /// The masked form of a text value: shape-preserving for e-mail, phone and IBAN, opaque
    /// otherwise.
    #[must_use]
    pub fn mask_text(&self, text: &str) -> String {
        match shape_of(text) {
            Shape::Email => {
                let (local, domain) = match text.split_once('@') {
                    Some(parts) => parts,
                    None => (text, ""),
                };
                format!(
                    "u.{}@example.invalid",
                    self.tag("email", &format!("{local}@{domain}"))
                )
            }
            Shape::Phone => self.mask_phone(text),
            Shape::Iban => self.mask_iban(text),
            Shape::Other => self.token("text", text),
        }
    }

    /// Digits stay digits, separators stay separators, the leading `+` stays.
    fn mask_phone(&self, text: &str) -> String {
        let bytes = self.stream("phone", text, text.len());
        let mut out = String::with_capacity(text.len());
        for (index, ch) in text.chars().enumerate() {
            if ch.is_ascii_digit() {
                let byte = bytes.get(index).copied().unwrap_or(0);
                out.push(char::from(b'0' + (byte % 10)));
            } else {
                out.push(ch);
            }
        }
        out
    }

    /// Country, length and separators stay; the BBAN is deterministic; the check digits are
    /// recomputed so an IBAN validator still accepts the sandbox value.
    fn mask_iban(&self, text: &str) -> String {
        let compact: Vec<char> = text.chars().filter(|c| !c.is_whitespace()).collect();
        if compact.len() < 8 {
            return self.token("text", text);
        }
        let country: String = compact[0..2].iter().collect();
        let bytes = self.stream("iban", text, compact.len());
        let mut mapped = String::with_capacity(compact.len());
        for (index, ch) in compact.iter().enumerate() {
            if index < 2 {
                mapped.push(*ch);
                continue;
            }
            let byte = bytes.get(index).copied().unwrap_or(0);
            if ch.is_ascii_digit() {
                mapped.push(char::from(b'0' + (byte % 10)));
            } else if ch.is_ascii_alphabetic() {
                let upper = char::from(b'A' + (byte % 26));
                mapped.push(if ch.is_ascii_lowercase() {
                    upper.to_ascii_lowercase()
                } else {
                    upper
                });
            } else {
                mapped.push(*ch);
            }
        }
        let bban: String = mapped.chars().skip(4).collect();
        let check = iban_check_digits(&country, &bban);
        let mut compact_out: Vec<char> = mapped.chars().collect();
        let check_chars: Vec<char> = check.chars().collect();
        for (offset, ch) in check_chars.into_iter().enumerate() {
            if let Some(slot) = compact_out.get_mut(2 + offset) {
                *slot = ch;
            }
        }
        let mut out = String::with_capacity(text.len());
        let mut mapped_iter = compact_out.into_iter();
        for ch in text.chars() {
            if ch.is_whitespace() {
                out.push(ch);
            } else if let Some(next) = mapped_iter.next() {
                out.push(next);
            }
        }
        out
    }

    /// A number keeps its digit count. A number too large for `i64`/`u64` becomes a string of
    /// the same digits rather than a float.
    fn mask_number(&self, number: &serde_json::Number) -> Value {
        let text = number.to_string();
        let masked = mask_digits(&self.stream("number", &text, text.len()), &text);
        if number.is_u64()
            && let Ok(value) = masked.parse::<u64>()
        {
            return Value::Number(value.into());
        }
        if number.is_i64()
            && let Ok(value) = masked.parse::<i64>()
        {
            return Value::Number(value.into());
        }
        // A fractional or oversized number becomes text: no float and no lost digit.
        Value::String(masked)
    }

    /// Scrubs free text to hash tokens, one per whitespace-separated word. The separators are
    /// normalised to a single space; no original word survives.
    #[must_use]
    pub fn scrub(&self, text: &str) -> String {
        let mut tokens: Vec<String> = Vec::new();
        for word in text.split_whitespace() {
            tokens.push(format!("h:{}", self.tag("word", word)));
        }
        if tokens.is_empty() {
            String::new()
        } else {
            tokens.join(" ")
        }
    }

    /// The placeholder for an attachment: a hashed name and nothing else.
    #[must_use]
    pub fn attachment_name(&self, filename: &str) -> String {
        format!("file-{}.bin", self.tag("attachment", filename))
    }
}

/// The shapes that are preserved. Detected from the value, never from a field name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shape {
    Email,
    Phone,
    Iban,
    Other,
}

fn shape_of(value: &str) -> Shape {
    let trimmed = value.trim();
    if let Some((local, domain)) = trimmed.split_once('@')
        && !local.is_empty()
        && !domain.is_empty()
        && domain.contains('.')
        && !trimmed.chars().any(char::is_whitespace)
        && trimmed.matches('@').count() == 1
    {
        return Shape::Email;
    }
    let compact: String = trimmed.chars().filter(|c| !c.is_whitespace()).collect();
    let iban_chars = compact.chars().count();
    if (15..=34).contains(&iban_chars) {
        let bytes = compact.as_bytes();
        if bytes[0].is_ascii_alphabetic()
            && bytes[1].is_ascii_alphabetic()
            && bytes[2].is_ascii_digit()
            && bytes[3].is_ascii_digit()
            && bytes.iter().all(u8::is_ascii_alphanumeric)
        {
            return Shape::Iban;
        }
    }
    let digits = trimmed.chars().filter(char::is_ascii_digit).count();
    let phone_chars = trimmed
        .chars()
        .all(|c| c.is_ascii_digit() || matches!(c, '+' | '-' | ' ' | '.' | '(' | ')'));
    if digits >= 7 && phone_chars {
        return Shape::Phone;
    }
    Shape::Other
}

fn mask_digits(bytes: &[u8], text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (index, ch) in text.chars().enumerate() {
        if ch.is_ascii_digit() {
            let byte = bytes.get(index).copied().unwrap_or(0);
            out.push(char::from(b'0' + (byte % 10)));
        } else {
            out.push(ch);
        }
    }
    out
}

/// The mod-97 check digits of a masked IBAN: BBAN + country + "00", letters expanded to
/// numbers, remainder computed digit by digit (integer arithmetic only).
fn iban_check_digits(country: &str, bban: &str) -> String {
    let mut expanded = String::with_capacity(bban.len() + country.len() + 2);
    for ch in bban.chars().chain(country.chars()).chain(['0', '0']) {
        if ch.is_ascii_digit() {
            expanded.push(ch);
        } else if ch.is_ascii_alphabetic() {
            let value = ch.to_ascii_uppercase() as u32 - 'A' as u32 + 10;
            expanded.push_str(&value.to_string());
        }
    }
    let mut remainder: u32 = 0;
    for digit in expanded.chars().filter_map(|c| c.to_digit(10)) {
        remainder = (remainder * 10 + digit) % 97;
    }
    let check = 98 - remainder;
    format!("{check:02}")
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// The text of a JSON value for hashing a non-string value.
fn scalar_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod geo_range_tests {
    use super::*;

    #[test]
    fn a_position_and_a_range_are_masked_as_identifiers_never_left_open() {
        let policy = MaskingPolicy::default();
        for ty in [FieldType::Geo, FieldType::Range] {
            let class = FieldClass::plain("booking", "place", ty);
            assert_eq!(kind_for(&class, &policy), MaskKind::Identifier);
        }
    }
}
