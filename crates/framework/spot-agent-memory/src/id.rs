//! The addresses a memory entry lives at: the tenant, the entry ids and the CI-run id.
//!
//! Every id is validated text with one shape: dotted lower-case segments, each starting with a
//! letter (`episode.job_stuck.0001`, `backend.postgres_version`). An id is an address, never a
//! value, so it is always safe to log and to put in an error message. The CI-run id is the one
//! exception to the dotted rule because it is the CI system's own token.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::MemoryError;

/// How many characters of a refused id an error keeps.
const EXCERPT_CHARS: usize = 24;

/// A refused id's text, cut to [`EXCERPT_CHARS`] characters with its length, so an error never
/// echoes a long (or secret-shaped) value in full.
fn excerpt(text: &str) -> String {
    let mut kept: String = text.chars().take(EXCERPT_CHARS).collect();
    if text.chars().nth(EXCERPT_CHARS).is_some() {
        kept.push_str(&format!("... ({} bytes)", text.len()));
    }
    kept
}

/// Whether one dotted segment matches `[a-z][a-z0-9_-]*`.
fn segment_ok(segment: &str) -> bool {
    let mut chars = segment.chars();
    match chars.next() {
        Some(first) if first.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// Whether `text` is two or more dotted lower-case segments.
fn dotted_ok(text: &str) -> bool {
    let mut segments = text.split('.');
    let Some(first) = segments.next() else {
        return false;
    };
    if !segment_ok(first) {
        return false;
    }
    let mut count = 1;
    for segment in segments {
        if !segment_ok(segment) {
            return false;
        }
        count += 1;
    }
    count >= 2
}

/// Whether `text` is a lower-case tag with at least one character (`job.stuck`, `job_stuck`).
fn tag_ok(text: &str) -> bool {
    text.len() <= 96
        && !text.is_empty()
        && text
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && text.chars().all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.' || c == '-'
        })
}

/// Whether `text` is a CI run id: 1 to 128 characters of `[A-Za-z0-9._:-]`.
fn ci_run_ok(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= 128
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-'))
}

/// Checks `text` and names the reason it was refused.
fn check(text: &str, longest: usize, validator: fn(&str) -> bool) -> Result<(), &'static str> {
    if text.is_empty() {
        return Err("it is empty");
    }
    if text.len() > longest {
        return Err("it is too long");
    }
    if !validator(text) {
        return Err("it is not dotted lower-case");
    }
    Ok(())
}

/// Declares one validated id newtype: the struct, `parse`, `as_str`, `Display` and its serde
/// form (the text, so a store keeps the address and nothing else).
macro_rules! id_type {
    ($(#[$meta:meta])* $name:ident, $what:literal, $longest:literal, $validator:ident) => {
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String")]
        pub struct $name(String);

        // Deserialising goes through `parse`, so a stored or received id is validated exactly
        // like a typed one.
        impl TryFrom<String> for $name {
            type Error = MemoryError;

            fn try_from(text: String) -> Result<Self, MemoryError> {
                Self::parse(&text)
            }
        }

        impl $name {
            /// Parses the id, refusing anything outside its documented shape (empty, too long,
            /// or not dotted lower-case).
            ///
            /// # Errors
            ///
            /// [`MemoryError::InvalidId`] when the text is empty, too long or not dotted
            /// lower-case.
            pub fn parse(text: &str) -> Result<Self, MemoryError> {
                match check(text, $longest, $validator) {
                    Ok(()) => Ok(Self(text.to_owned())),
                    Err(reason) => Err(MemoryError::InvalidId {
                        what: $what.to_owned(),
                        value: excerpt(text),
                        reason: reason.to_owned(),
                    }),
                }
            }

            /// The id's text.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

id_type!(
    /// A tenant's own memory namespace. Episodic memory never crosses it.
    TenantId,
    "tenant",
    64,
    tenant_ok
);

id_type!(
    /// One recorded incident: symptoms, actions, outcome and the audit entries behind them.
    EpisodeId,
    "episode",
    192,
    dotted_ok
);

id_type!(
    /// A learned how-to: an ordered list of remedy steps that resolved a symptom.
    ProcedureId,
    "procedure",
    192,
    dotted_ok
);

id_type!(
    /// One deployment fact, keyed by what it says about the installation.
    FactId,
    "fact",
    192,
    dotted_ok
);

id_type!(
    /// The fact's key (`backend.postgres_version`), the stable name the fact is read by.
    FactKey,
    "fact key",
    192,
    dotted_ok
);

id_type!(
    /// A candidate remedy's address in the `candidates/` queue.
    CandidateId,
    "candidate",
    192,
    dotted_ok
);

id_type!(
    /// A symptom tag: the structured signature recall filters on (`job.stuck_running`).
    Symptom,
    "symptom",
    96,
    tag_ok
);

/// A tenant id is 1 to 64 characters of `[a-z0-9][a-z0-9_-]*`; it names no person.
fn tenant_ok(text: &str) -> bool {
    text.len() <= 64
        && !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// The CI system's run id, recorded on a candidate when promotion happens.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct CiRunId(String);

impl TryFrom<String> for CiRunId {
    type Error = MemoryError;

    fn try_from(text: String) -> Result<Self, MemoryError> {
        Self::parse(&text)
    }
}

impl CiRunId {
    /// Parses a run id: 1 to 128 characters of `[A-Za-z0-9._:-]`.
    ///
    /// # Errors
    ///
    /// [`MemoryError::InvalidId`] when the text is empty, too long or holds a character a run id
    /// cannot.
    pub fn parse(text: &str) -> Result<Self, MemoryError> {
        if ci_run_ok(text) {
            Ok(Self(text.to_owned()))
        } else {
            Err(MemoryError::InvalidId {
                what: "ci run".to_owned(),
                value: excerpt(text),
                reason: "it is 1 to 128 characters of `A-Z`, `a-z`, `0-9`, `.`, `_`, `:` or `-`"
                    .to_owned(),
            })
        }
    }

    /// The run id's text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CiRunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A record an episode is about, by type and id only.
///
/// This is the *only* way a memory entry names business data: the DocType and the record's id.
/// There is deliberately no field on this type, or on any memory type, that can carry a record's
/// value — that rule is the whole point of `Sanitiser` and of the entry shapes.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "RecordRefParts")]
pub struct RecordRef {
    /// The DocType name (`job`, `case`); never a business table name from a specific product.
    pub doctype: String,
    /// The record id, as the host renders it: letters, digits and `.`, `_`, `:`, `-` only, so a
    /// value (which has spaces and separators) cannot pass as an id.
    pub id: String,
}

/// The wire form of a [`RecordRef`], validated by [`RecordRef::new`] on the way in.
#[derive(Deserialize)]
struct RecordRefParts {
    doctype: String,
    id: String,
}

impl TryFrom<RecordRefParts> for RecordRef {
    type Error = MemoryError;

    fn try_from(parts: RecordRefParts) -> Result<Self, MemoryError> {
        Self::new(&parts.doctype, &parts.id)
    }
}

impl RecordRef {
    /// A reference to `(doctype, id)`.
    ///
    /// # Errors
    ///
    /// [`MemoryError::InvalidId`] when the DocType is not a lower-case identifier, or the id is
    /// empty, longer than 256 characters or carries a character an id cannot.
    pub fn new(doctype: &str, id: &str) -> Result<Self, MemoryError> {
        let doctype_ok = doctype.len() <= 64
            && doctype
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_lowercase())
            && doctype
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
        if !doctype_ok {
            return Err(MemoryError::InvalidId {
                what: "doctype".to_owned(),
                value: excerpt(doctype),
                reason: "a DocType is a lower-case identifier".to_owned(),
            });
        }
        if id.is_empty()
            || id.len() > 256
            || !id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-'))
        {
            return Err(MemoryError::InvalidId {
                what: "record".to_owned(),
                value: excerpt(id),
                reason:
                    "a record id is 1 to 256 characters of letters, digits, `.`, `_`, `:` or `-`"
                        .to_owned(),
            });
        }
        Ok(Self {
            doctype: doctype.to_owned(),
            id: id.to_owned(),
        })
    }
}

impl fmt::Display for RecordRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.doctype, self.id)
    }
}

/// A link to one audit entry, by the id the audit chain reports.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct AuditRef(String);

impl TryFrom<String> for AuditRef {
    type Error = MemoryError;

    fn try_from(text: String) -> Result<Self, MemoryError> {
        Self::parse(&text)
    }
}

impl AuditRef {
    /// A reference to an audit entry.
    ///
    /// # Errors
    ///
    /// [`MemoryError::InvalidId`] when the id is empty, longer than 128 characters or holds a
    /// control character.
    pub fn parse(text: &str) -> Result<Self, MemoryError> {
        if text.is_empty() || text.len() > 128 || text.chars().any(char::is_control) {
            return Err(MemoryError::InvalidId {
                what: "audit".to_owned(),
                value: excerpt(text),
                reason: "an audit id is 1 to 128 printable characters".to_owned(),
            });
        }
        Ok(Self(text.to_owned()))
    }

    /// The audit id's text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AuditRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::{AuditRef, CiRunId, FactKey, ProcedureId, RecordRef, Symptom, TenantId};

    #[test]
    fn ids_accept_their_shape_and_refuse_the_rest() {
        assert!(TenantId::parse("tenant_a").is_ok());
        assert!(TenantId::parse("Tenant_A").is_err());
        assert!(TenantId::parse("").is_err());
        assert!(TenantId::parse(&"a".repeat(65)).is_err());

        assert!(ProcedureId::parse("procedure.job_stuck").is_ok());
        assert!(ProcedureId::parse("procedure").is_err());
        assert!(ProcedureId::parse("Procedure.job").is_err());
        assert!(ProcedureId::parse("procedure..job").is_err());

        assert!(FactKey::parse("backend.postgres_version").is_ok());
        assert!(FactKey::parse("backend postgres").is_err());

        assert!(Symptom::parse("job.stuck_running").is_ok());
        assert!(Symptom::parse("job.Stuck").is_err());
        assert!(Symptom::parse("").is_err());

        assert!(
            CiRunId::parse("run-2026.04/17:1").is_err(),
            "`/` is not a run-id character"
        );
        assert!(CiRunId::parse("run-2026.04-17:1").is_ok());
        assert!(CiRunId::parse("").is_err());
    }

    #[test]
    fn deserialising_an_id_validates_it_like_parse() {
        assert!(serde_json::from_str::<TenantId>("\"tenant_a\"").is_ok());
        assert!(serde_json::from_str::<TenantId>("\"Not A Tenant\"").is_err());
        assert!(serde_json::from_str::<ProcedureId>("\"nodots\"").is_err());
        assert!(serde_json::from_str::<CiRunId>("\"bad/run\"").is_err());
        assert!(serde_json::from_str::<AuditRef>("\"\"").is_err());
        assert!(serde_json::from_str::<RecordRef>(r#"{"doctype":"job","id":"a b"}"#).is_err());
        let round: RecordRef = serde_json::from_str(r#"{"doctype":"job","id":"j-1"}"#).unwrap();
        assert_eq!(round.to_string(), "job:j-1");
    }

    #[test]
    fn a_refused_id_is_not_echoed_in_full() {
        let long = format!("Secret{}", "x".repeat(300));
        let error = TenantId::parse(&long).unwrap_err().to_string();
        assert!(error.len() < 200, "the error is short: {error}");
        assert!(!error.contains(&long));
        assert!(error.contains("300") || error.contains("306"));
    }

    #[test]
    fn a_record_reference_is_a_type_and_an_id_and_nothing_else() {
        let reference = RecordRef::new("job", "018f-0001").unwrap_or_else(|_| unreachable!());
        assert_eq!(reference.to_string(), "job:018f-0001");
        assert!(RecordRef::new("Job", "1").is_err());
        assert!(RecordRef::new("job", "").is_err());
        assert!(RecordRef::new("job", "a\nb").is_err());
        assert!(
            RecordRef::new("job", "1000.00 total").is_err(),
            "an id carries no space, so a value cannot pass as one"
        );
    }

    #[test]
    fn an_audit_reference_refuses_text_a_memory_entry_could_not_store() {
        assert!(AuditRef::parse("audit-0001").is_ok());
        assert!(AuditRef::parse("").is_err());
        assert!(AuditRef::parse("a\u{0}b").is_err());
    }
}
