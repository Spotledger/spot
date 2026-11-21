//! The no-business-data rule: `Sanitiser::check` decides whether text may enter the memory
//! namespace at all.
//!
//! Memory may reference records **only by type and id** — `RecordRef { doctype, id }` and an
//! audit id. It may never hold a record's value. The rule is enforced in three layers:
//!
//! 1. **Structure.** No memory type has a field that can carry a document payload: text is a
//!    [`Summary`] (bounded, and only ever produced by this module), everything else is an id, an
//!    enum, an integer or a scalar parameter.
//! 2. **Scan.** [`Sanitiser::check`] reads the text against three field classes — `secret`,
//!    `financial` and `pii` — using `spot-redact`'s rule set (card numbers and IBANs by Luhn and
//!    mod-97, provider keys, credentials in parameters and URLs, emails and phones) plus this
//!    crate's numeric-amount patterns and its deny-list of field names
//!    ([`spot_redact::DEFAULT_FIELD_NAMES`] for secrets, and the financial and personal names
//!    below). A **secret or financial hit rejects the text**; a **pii hit is redacted** and the
//!    text is accepted with the redaction recorded.
//! 3. **Bound.** Longer than [`MAX_CONTENT_BYTES`], or carrying a control character, is refused:
//!    a document dump or a binary payload cannot pass.
//!
//! Nothing here echoes a caught value: every error names the class and the shape, never the
//! text.

use std::fmt;

use regex::Regex;
use serde::Serialize;
use spot_redact::{
    DEFAULT_FIELD_NAMES, MARKER_CLOSE, MARKER_OPEN, Patterns, RedactionPolicy, Redactor,
};

use crate::error::MemoryError;

/// The longest text a memory entry may hold, in bytes.
pub const MAX_CONTENT_BYTES: usize = 1_024;

/// The financial field names whose values are record values, never memory text.
const FINANCIAL_FIELDS: &[&str] = &[
    "amount",
    "balance",
    "bic",
    "card_number",
    "discount",
    "grand_total",
    "iban",
    "net_total",
    "price",
    "qty",
    "quantity",
    "rate",
    "sort_code",
    "subtotal",
    "tax",
    "total",
    "unit_price",
];

/// The personal field names whose values are record values, never memory text.
const PII_FIELDS: &[&str] = &[
    "address",
    "customer",
    "customer_name",
    "date_of_birth",
    "dob",
    "email",
    "first_name",
    "full_name",
    "last_name",
    "mobile",
    "national_id",
    "party",
    "passport",
    "phone",
    "supplier",
    "supplier_name",
];

/// The field class a scan attributed a value to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldClass {
    /// Personal data: redacted, never rejected on its own.
    Pii,
    /// Money, quantities and amounts: rejected.
    Financial,
    /// Secrets, credentials and keys: rejected.
    Secret,
}

impl FieldClass {
    /// The stable snake_case text form, which is also the serde form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pii => "pii",
            Self::Financial => "financial",
            Self::Secret => "secret",
        }
    }
}

impl fmt::Display for FieldClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why [`Sanitiser::check`] refused a text.
///
/// Every variant names a class and a shape; none carries the caught value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SanitiserError {
    /// The text is longer than [`MAX_CONTENT_BYTES`].
    TooLong {
        /// The text's byte length.
        bytes: usize,
        /// The maximum accepted.
        max: usize,
    },
    /// The text carries a control character other than a newline or a tab: not prose.
    ControlCharacters,
    /// A deny-listed field name carries a value (`total: 1,200.00`).
    FieldValue {
        /// The class the field name belongs to.
        class: FieldClass,
    },
    /// The text carries a numeric amount, whatever it is called.
    Amount,
    /// A `spot-redact` rule matched: a card number, an IBAN, a key, a credential, an email.
    Pattern {
        /// The class the matched rule belongs to.
        class: FieldClass,
    },
}

impl SanitiserError {
    /// The class the refusal belongs to, when it has one.
    #[must_use]
    pub fn class(self) -> Option<FieldClass> {
        match self {
            Self::FieldValue { class } | Self::Pattern { class } => Some(class),
            Self::Amount => Some(FieldClass::Financial),
            Self::TooLong { .. } | Self::ControlCharacters => None,
        }
    }

    /// The shape the scan caught (`field_value`, `amount`, `pattern`, `too_long`, `control`).
    #[must_use]
    pub fn shape(self) -> &'static str {
        match self {
            Self::TooLong { .. } => "too_long",
            Self::ControlCharacters => "control_characters",
            Self::FieldValue { .. } => "field_value",
            Self::Amount => "amount",
            Self::Pattern { .. } => "pattern",
        }
    }
}

impl fmt::Display for SanitiserError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLong { bytes, max } => {
                write!(f, "the content is {bytes} bytes long, and {max} is the maximum")
            }
            Self::ControlCharacters => {
                f.write_str("the content carries a control character, so it is not prose")
            }
            Self::FieldValue { class } => write!(
                f,
                "the content assigns a value to a {class} field; memory may reference records only by type and id"
            ),
            Self::Amount => f.write_str(
                "the content carries a numeric amount; memory may reference records only by type and id",
            ),
            Self::Pattern { class } => write!(
                f,
                "the content carries a {class} value; memory may reference records only by type and id"
            ),
        }
    }
}

impl std::error::Error for SanitiserError {}

impl From<SanitiserError> for MemoryError {
    fn from(error: SanitiserError) -> Self {
        match error {
            SanitiserError::TooLong { .. } | SanitiserError::ControlCharacters => {
                MemoryError::Unstorable {
                    detail: error.to_string(),
                }
            }
            other => MemoryError::Unsanitised {
                class: other
                    .class()
                    .map_or("content", FieldClass::as_str)
                    .to_owned(),
                shape: other.shape().to_owned(),
            },
        }
    }
}

/// Text a memory entry may hold: bounded prose that passed [`Sanitiser::check`].
///
/// The only constructor takes the `String` this module produced, so a caller cannot build one
/// from a record's value: there is no `From<String>`, no public field and no `Deserialize`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Summary {
    /// The accepted text, with any personal data already replaced by a redaction marker.
    text: String,
    /// The classes that were redacted on the way in, in the order they were found.
    redactions: Vec<FieldClass>,
}

impl Summary {
    /// The accepted text, with every personal value replaced by its marker.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.text
    }

    /// The classes redacted on the way in (empty when the text was clean).
    #[must_use]
    pub fn redactions(&self) -> &[FieldClass] {
        &self.redactions
    }

    /// Whether personal data was redacted.
    #[must_use]
    pub fn was_redacted(&self) -> bool {
        !self.redactions.is_empty()
    }
}

impl fmt::Display for Summary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

/// The scanner behind the no-business-data rule.
///
/// Build it once (`Sanitiser::standard`), share it, and pass every free-text field of every
/// entry through [`Sanitiser::check`].
pub struct Sanitiser {
    redactor: Redactor,
    secret_fields: Regex,
    financial_fields: Regex,
    pii_fields: Regex,
    amounts: Vec<Regex>,
    max_bytes: usize,
}

impl Sanitiser {
    /// The standard scanner: every `spot-redact` rule on, the pii class on (so an email is
    /// redacted rather than let through), and this crate's amount and field-name patterns.
    ///
    /// # Errors
    ///
    /// [`MemoryError::Unstorable`] when the redaction policy cannot be compiled, or a pattern
    /// cannot be built.
    pub fn standard() -> Result<Self, MemoryError> {
        let policy = RedactionPolicy {
            patterns: Patterns {
                pii_email: true,
                pii_phone: true,
                ..Patterns::default()
            },
            field_names: FINANCIAL_FIELDS
                .iter()
                .chain(PII_FIELDS.iter())
                .map(|name| (*name).to_owned())
                .collect(),
        };
        let redactor = Redactor::from_policy(&policy).map_err(|error| MemoryError::Unstorable {
            detail: format!("the redaction policy did not load: {error}"),
        })?;
        let build = |names: &[&str]| -> Result<Regex, MemoryError> {
            let alternation = names.join("|");
            Regex::new(&format!("(?i)\\b(?:{alternation})\\b\\s*[:=]")).map_err(|error| {
                MemoryError::Unstorable {
                    detail: format!("a field-name pattern did not compile: {error}"),
                }
            })
        };
        let secret_names: Vec<&str> = DEFAULT_FIELD_NAMES.to_vec();
        let amounts = amount_patterns()?;
        Ok(Self {
            redactor,
            secret_fields: build(&secret_names)?,
            financial_fields: build(FINANCIAL_FIELDS)?,
            pii_fields: build(PII_FIELDS)?,
            amounts,
            max_bytes: MAX_CONTENT_BYTES,
        })
    }

    /// The same scanner with a different size bound.
    ///
    /// # Errors
    ///
    /// As [`Sanitiser::standard`].
    pub fn with_max_bytes(max_bytes: usize) -> Result<Self, MemoryError> {
        let mut sanitiser = Self::standard()?;
        sanitiser.max_bytes = max_bytes.max(1);
        Ok(sanitiser)
    }

    /// The size bound this scanner applies.
    #[must_use]
    pub fn max_bytes(&self) -> usize {
        self.max_bytes
    }

    /// Checks `content` and returns the text a memory entry may hold.
    ///
    /// A secret or financial value rejects the content; personal data is redacted and the content
    /// is accepted with the redaction recorded.
    ///
    /// # Errors
    ///
    /// [`SanitiserError`] naming the class and shape that stopped the content. Never the value.
    pub fn check(&self, content: &str) -> Result<Summary, SanitiserError> {
        if content.len() > self.max_bytes {
            return Err(SanitiserError::TooLong {
                bytes: content.len(),
                max: self.max_bytes,
            });
        }
        if content
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t')
        {
            return Err(SanitiserError::ControlCharacters);
        }
        if self.secret_fields.is_match(content) {
            return Err(SanitiserError::FieldValue {
                class: FieldClass::Secret,
            });
        }
        if self.financial_fields.is_match(content) {
            return Err(SanitiserError::FieldValue {
                class: FieldClass::Financial,
            });
        }
        if self.pii_fields.is_match(content) {
            return Err(SanitiserError::FieldValue {
                class: FieldClass::Pii,
            });
        }
        if self.amounts.iter().any(|pattern| pattern.is_match(content)) {
            return Err(SanitiserError::Amount);
        }
        let (redacted, count) = self.redactor.redact_str_counted(content);
        if count == 0 {
            return Ok(Summary {
                text: content.to_owned(),
                redactions: Vec::new(),
            });
        }
        let classes = marker_classes(&redacted);
        match classes
            .iter()
            .find(|class| matches!(class, FieldClass::Secret | FieldClass::Financial))
        {
            Some(class) => Err(SanitiserError::Pattern { class: *class }),
            None => Ok(Summary {
                text: redacted.into_owned(),
                redactions: if classes.is_empty() {
                    vec![FieldClass::Pii]
                } else {
                    classes
                },
            }),
        }
    }
}

/// The classes `spot-redact`'s markers in `text` belong to, in the order they appear.
fn marker_classes(text: &str) -> Vec<FieldClass> {
    let mut classes = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(MARKER_OPEN) {
        let after = &rest[start + MARKER_OPEN.len()..];
        let Some(end) = after.find(MARKER_CLOSE) else {
            break;
        };
        let class = match &after[..end] {
            "email" | "phone" => FieldClass::Pii,
            "card_number" | "iban" => FieldClass::Financial,
            _ => FieldClass::Secret,
        };
        if !classes.contains(&class) {
            classes.push(class);
        }
        rest = &after[end..];
    }
    classes
}

/// The numeric-amount shapes no memory entry may hold: a currency amount, grouped thousands, a
/// decimal with cents, and a numeric value assigned to an amount-ish name.
fn amount_patterns() -> Result<Vec<Regex>, MemoryError> {
    let patterns = [
        r"(?i)(?:rs\.?|pkr|usd|eur|gbp|inr|aed|sar|jpy|chf|\$|€|£|₹)\s?\d[\d,]*(?:\.\d+)?",
        r"\b\d{1,3}(?:,\d{3})+(?:\.\d+)?\b",
        r"\b\d+\.\d{2}\b",
        r"(?i)\b(?:amount|balance|discount|price|qty|quantity|rate|subtotal|tax|total)\b\s*[:=]\s*-?\d[\d,.]*",
    ];
    patterns
        .iter()
        .map(|pattern| {
            Regex::new(pattern).map_err(|error| MemoryError::Unstorable {
                detail: format!("an amount pattern did not compile: {error}"),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{FieldClass, Sanitiser, SanitiserError};

    fn sanitiser() -> Sanitiser {
        Sanitiser::standard().unwrap()
    }

    #[test]
    fn a_record_reference_passes() {
        let summary = sanitiser()
            .check("job:018f-0001 was stuck in running after 3 retries; job.unstick retried it")
            .unwrap();
        assert!(summary.as_str().contains("018f-0001"));
        assert!(!summary.was_redacted());
    }

    #[test]
    fn secrets_are_refused_not_redacted() {
        let sanitiser = sanitiser();
        for content in [
            "password=hunter2hunter2",
            "the key AKIAIOSFODNN7EXAMPLE was used",
            "token: abcdefghijklmnop",
            "postgres://user:pw@db:5432/site",
        ] {
            let error = sanitiser.check(content).unwrap_err();
            assert_eq!(
                error.class(),
                Some(FieldClass::Secret),
                "`{content}` is refused as a secret, not redacted"
            );
        }
    }

    #[test]
    fn amounts_and_financial_fields_are_refused() {
        let sanitiser = sanitiser();
        for content in [
            "the invoice total was Rs 12,500",
            "amount: 1500",
            "unit_price = 99.50",
            "a payment of 1,234,567 was posted",
            "balance 250.00",
        ] {
            let error = sanitiser.check(content).unwrap_err();
            assert_eq!(
                error.class(),
                Some(FieldClass::Financial),
                "`{content}` is refused as a financial value"
            );
        }
        assert_eq!(
            sanitiser
                .check("a card 4111 1111 1111 1111 was used")
                .unwrap_err()
                .class(),
            Some(FieldClass::Financial)
        );
        assert_eq!(
            sanitiser
                .check("IBAN DE89370400440532013000")
                .unwrap_err()
                .class(),
            Some(FieldClass::Financial)
        );
    }

    #[test]
    fn personal_data_is_redacted_and_never_returned() {
        let summary = sanitiser()
            .check("the operator bob@example.com retried the job")
            .unwrap();
        assert!(!summary.as_str().contains("bob@example.com"));
        assert!(summary.as_str().contains(MARKER));
        assert_eq!(summary.redactions(), [FieldClass::Pii]);
    }

    const MARKER: &str = "[redacted:";

    #[test]
    fn a_document_dump_is_refused() {
        let sanitiser = sanitiser();
        // A rendered invoice: an amount, a tax amount and a counterparty.
        let invoice = "Sales Invoice SINV-2026-0001\ncustomer: Acme Trading\nnet_total: 1000\n\
                       tax: 170.00\ntotal: 1170.00\nIBAN DE89370400440532013000";
        assert!(sanitiser.check(invoice).is_err());
        // A PDF-ish blob: control characters.
        assert_eq!(
            sanitiser.check("%PDF-1.7\n\u{1}\u{2}\u{3}").unwrap_err(),
            SanitiserError::ControlCharacters
        );
        // A long body: over the bound.
        let body = "a".repeat(2_000);
        assert!(matches!(
            sanitiser.check(&body).unwrap_err(),
            SanitiserError::TooLong { .. }
        ));
    }

    #[test]
    fn the_scan_is_idempotent_and_bounded() {
        let sanitiser = sanitiser();
        let first = sanitiser
            .check("mail bob@example.com about the retry")
            .unwrap();
        let second = sanitiser.check(first.as_str()).unwrap();
        assert_eq!(second.as_str(), first.as_str());
        assert!(
            !second.was_redacted(),
            "a marker is inert on the second pass"
        );
    }
}
