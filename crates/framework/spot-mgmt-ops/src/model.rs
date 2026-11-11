//! The operation data model: [`OpId`], [`Risk`], [`TargetKind`], [`ParamSpec`],
//! [`ParamType`] and [`ParamValue`], assembled into an [`OperationSpec`].
//!
//! An operation is data: a name, typed parameters and a risk level, from which both the API
//! validation and the command line are derived. Everything in this module is exactly that —
//! no behaviour, no I/O, no execution (the agent and the transport live elsewhere).
//!
//! # Rules (the behaviour this module is, in our own words)
//!
//! - An [`OpId`] is a dotted lower-case path of 2 to 4 segments, each matching
//!   `[a-z][a-z0-9_]*`, and it is validated on construction — including through serde, so a
//!   malformed id cannot enter a spec from a payload.
//! - [`Risk`] is ordered `Read < Low < Medium < High < Critical`; the ordering is part of
//!   the contract (WP-ADM-03 attaches approval rules to it later; here it is only data).
//! - [`ParamType`] keeps the set small — boolean, bounded integer, capped and optionally
//!   pattern-checked text, a closed enum, whole-second durations and flat lists — and there
//!   is deliberately no `Any`.
//! - A [`ParamValue`] carries exactly one typed value; an enum parameter stores its choice
//!   as `ParamValue::Text` so the value shape never forks per type.
//! - Parameter names are lower-case snake identifiers ([`is_param_name`]); they become
//!   `--kebab-case` flags on the command line, and the catalogue refuses a name whose flag
//!   would collide with a boolean parameter's `--no-<name>` false form.

use serde::{Deserialize, Serialize};

/// Why an operation id was refused by [`OpId::try_from`].
///
/// Construction happens in module code and in payload deserialization; the catalogue can
/// never see a malformed id (an [`OpId`] cannot exist invalid), so this type carries no
/// stable `ops.*` code.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum OpIdError {
    /// The id does not have 2 to 4 dot-separated segments.
    #[error("an operation id needs 2 to 4 dot-separated segments, got {got}")]
    SegmentCount {
        /// How many segments the text actually had.
        got: usize,
    },
    /// One segment is not lower-case: every segment must match `[a-z][a-z0-9_]*`.
    #[error(
        "segment `{segment}` of an operation id must be lower-case: only a-z, then a-z, 0-9 and underscore are accepted"
    )]
    Segment {
        /// The offending segment.
        segment: String,
    },
}

/// A dotted lower-case operation id of 2 to 4 segments, for example
/// `server.service.restart` or `tenant.backup.run`.
///
/// Validated on construction: two to four segments, each one lower-case letter followed by
/// lower-case letters, digits or underscores. An invalid id cannot exist, so every method
/// can trust the shape; serde deserializes through [`TryFrom`] for the same guarantee.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct OpId(String);

impl OpId {
    /// The id named `text`, when it is a valid dotted lower-case path of 2 to 4 segments.
    #[must_use]
    pub fn new(text: &str) -> Option<OpId> {
        Self::try_new(text).ok()
    }

    /// The id named `text`, or why it was refused.
    ///
    /// # Errors
    ///
    /// Returns [`OpIdError`] when the text has fewer than 2 or more than 4 dot-separated
    /// segments, or when any segment is not lower-case.
    pub fn try_new(text: &str) -> Result<OpId, OpIdError> {
        let segments: Vec<&str> = text.split('.').collect();
        if !(2..=4).contains(&segments.len()) {
            return Err(OpIdError::SegmentCount {
                got: segments.len(),
            });
        }
        for segment in &segments {
            if !is_segment(segment) {
                return Err(OpIdError::Segment {
                    segment: (*segment).to_owned(),
                });
            }
        }
        Ok(OpId(text.to_owned()))
    }

    /// The id as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The id's segments, in order.
    pub fn segments(&self) -> impl Iterator<Item = &str> {
        self.0.split('.')
    }
}

impl TryFrom<String> for OpId {
    type Error = OpIdError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        OpId::try_new(&text)
    }
}

impl TryFrom<&str> for OpId {
    type Error = OpIdError;

    fn try_from(text: &str) -> Result<Self, Self::Error> {
        OpId::try_new(text)
    }
}

/// Whether one segment matches `[a-z][a-z0-9_]*`.
fn is_segment(segment: &str) -> bool {
    let mut chars = segment.chars();
    match chars.next() {
        Some(first) if first.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Whether a parameter name is a lower-case identifier: one `[a-z]` letter and then only
/// `[a-z0-9_]`. These are the names that become `--kebab-case` flags, so hyphens and
/// upper-case letters are refused at catalogue build time.
#[must_use]
pub(crate) fn is_param_name(name: &str) -> bool {
    is_segment(name)
}

/// How high-stakes an operation is. Ordered `Read < Low < Medium < High < Critical`; the
/// ordering is part of the contract (WP-ADM-03 attaches approval rules to it later).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Risk {
    /// Reads state; changes nothing.
    Read,
    /// A reversible, low-impact change.
    Low,
    /// A change that needs care but is routine.
    Medium,
    /// A change with real blast radius or data exposure.
    High,
    /// A dangerous operation: destructive, security-affecting or hard to undo.
    Critical,
}

/// What kind of thing an operation acts on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TargetKind {
    /// Every server the management API sees.
    Fleet,
    /// One tagged group of servers.
    Group,
    /// One server.
    Server,
    /// One tenant and its application data.
    Tenant,
}

/// The type of one operation parameter.
///
/// The set is deliberately small: it is the whole vocabulary both the console screens and
/// the command line derive from. There is no `Any` — an operation that needs an arbitrary
/// payload is not a typed operation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ParamType {
    /// A boolean, rendered as a bare `--flag` when true and `--no-flag` when false.
    Bool,
    /// A 64-bit signed integer that must sit within `[min, max]` (bounds may touch).
    Int {
        /// The smallest accepted value.
        min: i64,
        /// The largest accepted value.
        max: i64,
    },
    /// Text of at most `max_len` characters; when `pattern` is set, the WHOLE value must
    /// match it (the crate anchors the pattern, so an author's `[a-z]+` means all of it).
    Text {
        /// The maximum length in characters (not bytes); 0 allows only empty text.
        max_len: usize,
        /// A whole-value regular expression, or `None` for "any text".
        pattern: Option<String>,
    },
    /// One of the listed choices, stored as `ParamValue::Text`. The set may not be empty.
    Enum(
        /// The accepted choices, in the order the console shows them.
        Vec<String>,
    ),
    /// A duration in whole seconds, written `30s` on the command line.
    Duration,
    /// A list of the inner type, rendered as one flag occurrence per element. Lists are
    /// flat (no list of lists — the command line cannot carry one) and empty lists are not
    /// valid values: a caller that means "nothing" omits the parameter.
    List(
        /// The element type.
        Box<ParamType>,
    ),
}

/// One typed parameter value, exactly the runtime shape of [`ParamType`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ParamValue {
    /// A boolean value (parameter type [`ParamType::Bool`]).
    Bool(
        /// The value.
        bool,
    ),
    /// An integer value (parameter type [`ParamType::Int`]).
    Int(
        /// The value.
        i64,
    ),
    /// A text value; also the storage for [`ParamType::Enum`] choices.
    Text(
        /// The value.
        String,
    ),
    /// A duration in whole seconds (parameter type [`ParamType::Duration`]).
    Duration(
        /// The value in seconds.
        u64,
    ),
    /// A list value (parameter type [`ParamType::List`]).
    List(
        /// The elements, each one valid for the list's element type.
        Vec<ParamValue>,
    ),
}

/// One declared parameter of an operation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParamSpec {
    /// The parameter name: a lower-case snake identifier, rendered `--kebab-case`.
    pub name: String,
    /// The parameter's type.
    pub ty: ParamType,
    /// Whether validation refuses an invocation that omits this parameter.
    pub required: bool,
    /// The value used when the caller omits a non-required parameter.
    pub default: Option<ParamValue>,
    /// What the parameter means, shown by the console next to the input field.
    pub doc: String,
    /// Whether the value is a secret: it never gets a default, is redacted in `Debug`, and
    /// is rendered only as a placeholder.
    pub secret: bool,
}

/// The declaration of one management operation: what it is called, what it takes, how
/// risky it is and what it acts on.
///
/// This is the data the console generates its screens from and the CLI generates its flags
/// from — the single source both clients derive from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationSpec {
    /// The dotted lower-case id, for example `server.service.restart`.
    pub id: OpId,
    /// One-line human name, shown in lists and search results.
    pub title: String,
    /// What the operation does, one or two sentences for the console.
    pub summary: String,
    /// The parameters, in declaration order (which is also the rendering order).
    pub params: Vec<ParamSpec>,
    /// How high-stakes the operation is.
    pub risk: Risk,
    /// What kind of target the operation acts on.
    pub target: TargetKind,
}

/// The `--kebab-case` flag spelling of a parameter name.
#[must_use]
pub(crate) fn kebab(name: &str) -> String {
    name.replace('_', "-")
}

/// Whether a parameter renders boolean-style flags: a plain [`ParamType::Bool`] or a list
/// of booleans, whose elements render as bare `--flag` / `--no-flag` occurrences.
#[must_use]
pub(crate) fn is_boolish(ty: &ParamType) -> bool {
    match ty {
        ParamType::Bool => true,
        ParamType::List(inner) => matches!(inner.as_ref(), ParamType::Bool),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn ids_validate_their_shape_on_construction() {
        assert!(OpId::new("server.service.restart").is_some());
        assert!(OpId::new("tenant.backup.run").is_some());
        assert!(OpId::new("a.b").is_some());
        assert!(OpId::new("a.b.c.d").is_some());
        // Too few or too many segments.
        assert!(OpId::new("restart").is_none());
        assert!(OpId::new("a.b.c.d.e").is_none());
        assert!(OpId::new("").is_none());
        // Upper case, leading digit, leading underscore, empty segment.
        assert!(OpId::new("Server.service").is_none());
        assert!(OpId::new("9p.test").is_none());
        assert!(OpId::new("_srv.test").is_none());
        assert!(OpId::new("a..b").is_none());
    }

    #[test]
    fn ids_derive_serde_through_validation() {
        let id = OpId::new("server.service.restart").unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"server.service.restart\"");
        assert_eq!(serde_json::from_str::<OpId>(&json).unwrap(), id);
        assert!(serde_json::from_str::<OpId>("\"Server.service\"").is_err());
        assert!(serde_json::from_str::<OpId>("\"one.segment\"").is_ok());
        assert!(serde_json::from_str::<OpId>("\"too.many.segments.here.ok\"").is_err());
    }

    #[test]
    fn risk_is_ordered_read_to_critical() {
        let mut risks = [
            Risk::Critical,
            Risk::Read,
            Risk::Medium,
            Risk::Low,
            Risk::High,
        ];
        risks.sort();
        assert_eq!(
            risks,
            [
                Risk::Read,
                Risk::Low,
                Risk::Medium,
                Risk::High,
                Risk::Critical
            ]
        );
        assert!(Risk::Read < Risk::Low);
        assert!(Risk::High < Risk::Critical);
    }

    #[test]
    fn param_names_are_lower_case_snake() {
        assert!(is_param_name("wait"));
        assert!(is_param_name("max_wait"));
        assert!(is_param_name("x9"));
        assert!(!is_param_name(""));
        assert!(!is_param_name("Max_Wait"));
        assert!(!is_param_name("max-wait"));
        assert!(!is_param_name("9x"));
        assert!(!is_param_name("_x"));
    }

    #[test]
    fn kebab_maps_snake_names_to_flags() {
        assert_eq!(kebab("max_wait"), "max-wait");
        assert_eq!(kebab("wait"), "wait");
    }

    #[test]
    fn serde_renders_risk_and_target_lower_case() {
        assert_eq!(serde_json::to_string(&Risk::Read).unwrap(), "\"read\"");
        assert_eq!(
            serde_json::to_string(&TargetKind::Tenant).unwrap(),
            "\"tenant\""
        );
    }
}
