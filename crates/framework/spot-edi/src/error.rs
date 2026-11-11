//! The typed errors of the EDI adapter (WP-INT-06.T1, ADR-26).
//!
//! Every failure carries a stable dotted `edi.*` code ([`EdiError::code`]); callers match on
//! the code, never on the message. A parse, write, schema or mapping problem is an
//! [`EdiError`]; the schema and envelope validators report **every** problem they find as a
//! [`Violation`](crate::schema::Violation) instead of failing on the first, so a sender fixes
//! one document once.
//!
//! # Codes
//!
//! | Code | Meaning |
//! | --- | --- |
//! | [`NOT_EDI`] | the input is not an X12 or EDIFACT interchange |
//! | [`BAD_ENCODING`] | the input is not ASCII text |
//! | [`BAD_DELIMITERS`] | the delimiters could not be discovered |
//! | [`TOO_LARGE`] | the input is over the byte limit |
//! | [`TOO_MANY_SEGMENTS`] | the input has more segments than the limit |
//! | [`TOO_MANY_ELEMENTS`] | a segment has more elements than the limit |
//! | [`BAD_SEGMENT`] | a segment is malformed (an empty tag, say) |
//! | [`MISSING_ENVELOPE`] | the header or trailer of an envelope is absent |
//! | [`UNBALANCED_ENVELOPE`] | an opening segment has no matching closing segment |
//! | [`ENVELOPE_COUNT`] | a declared segment or set count disagrees with the content |
//! | [`ENVELOPE_CONTROL`] | two control numbers that must match do not |
//! | [`ENVELOPE_ORDER`] | envelope segments are in the wrong order |
//! | [`SCHEMA_UNKNOWN`] | no schema has the requested id |
//! | [`SCHEMA_BAD`] | a schema document is not valid |
//! | [`SCHEMA_MISSING_SEGMENT`] | a required segment is absent |
//! | [`SCHEMA_UNEXPECTED_SEGMENT`] | a segment is not allowed at this point |
//! | [`SCHEMA_TOO_MANY`] | a segment occurs more than its maximum |
//! | [`SCHEMA_BAD_ELEMENT`] | an element fails its required or type check |
//! | [`MAPPING`] | a mapping rule failed |
//! | [`NO_MESSAGE`] | the interchange holds no transaction set or message |
//! | [`ACK_FAILED`] | an acknowledgement cannot be built |
//! | [`BAD_INPUT`] | the connector operation's input is not valid |
//! | [`UNKNOWN_OPERATION`](codes::UNKNOWN_OPERATION) | the connector has no such operation |

/// The stable codes this crate raises. Each has one entry in
/// `crates/framework/spot-errors/catalogue/edi.toml`.
pub mod codes {
    /// The input is not an X12 or EDIFACT interchange.
    pub const NOT_EDI: &str = "edi.not_edi";
    /// The input is not ASCII text.
    pub const BAD_ENCODING: &str = "edi.bad_encoding";
    /// The delimiters could not be discovered from the header.
    pub const BAD_DELIMITERS: &str = "edi.bad_delimiters";
    /// The input is larger than the configured byte limit.
    pub const TOO_LARGE: &str = "edi.too_large";
    /// The input has more segments than the configured limit.
    pub const TOO_MANY_SEGMENTS: &str = "edi.too_many_segments";
    /// A segment has more elements than the configured limit.
    pub const TOO_MANY_ELEMENTS: &str = "edi.too_many_elements";
    /// A segment is malformed (for example an empty tag).
    pub const BAD_SEGMENT: &str = "edi.bad_segment";
    /// The header or trailer of an envelope is absent.
    pub const MISSING_ENVELOPE: &str = "edi.missing_envelope";
    /// An opening envelope segment has no matching closing segment.
    pub const UNBALANCED_ENVELOPE: &str = "edi.unbalanced_envelope";
    /// A declared segment or transaction count disagrees with the content.
    pub const ENVELOPE_COUNT: &str = "edi.envelope_count";
    /// Two control numbers that must match do not.
    pub const ENVELOPE_CONTROL: &str = "edi.envelope_control";
    /// Envelope segments are in the wrong order.
    pub const ENVELOPE_ORDER: &str = "edi.envelope_order";
    /// No schema has the requested id.
    pub const SCHEMA_UNKNOWN: &str = "edi.schema_unknown";
    /// A schema document is not valid.
    pub const SCHEMA_BAD: &str = "edi.schema_bad";
    /// A required segment is absent.
    pub const SCHEMA_MISSING_SEGMENT: &str = "edi.schema_missing_segment";
    /// A segment is not allowed at this point in the message.
    pub const SCHEMA_UNEXPECTED_SEGMENT: &str = "edi.schema_unexpected_segment";
    /// A segment occurs more than its maximum number of times.
    pub const SCHEMA_TOO_MANY: &str = "edi.schema_too_many";
    /// An element fails its required or type check.
    pub const SCHEMA_BAD_ELEMENT: &str = "edi.schema_bad_element";
    /// A mapping rule failed.
    pub const MAPPING: &str = "edi.mapping";
    /// The interchange holds no transaction set or message.
    pub const NO_MESSAGE: &str = "edi.no_message";
    /// An acknowledgement cannot be built.
    pub const ACK_FAILED: &str = "edi.ack_failed";
    /// The connector operation's input is not valid.
    pub const BAD_INPUT: &str = "edi.bad_input";
    /// The connector has no such operation.
    pub const UNKNOWN_OPERATION: &str = "edi.unknown_operation";
}

/// One failure that stops a parse, a write or a mapping.
///
/// The schema and envelope validators do **not** use this for content problems: they collect
/// [`Violation`](crate::schema::Violation)s so every problem in a document is reported at
/// once. This enum is `#[non_exhaustive]`; match with a catch-all arm.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum EdiError {
    /// The input is not an X12 or EDIFACT interchange.
    #[error("edi.not_edi: the input does not start with an ISA, UNB or UNA segment")]
    NotEdi,
    /// The input is not ASCII text.
    #[error("edi.bad_encoding: the input is not ASCII at byte {offset}")]
    BadEncoding {
        /// The offset of the first non-ASCII byte.
        offset: usize,
    },
    /// The delimiters could not be discovered from the header.
    #[error("edi.bad_delimiters: {reason}")]
    BadDelimiters {
        /// What was wrong.
        reason: String,
    },
    /// The input is larger than the configured byte limit.
    #[error("edi.too_large: the input is {found} bytes, over the {limit} byte limit")]
    TooLarge {
        /// The input's size in bytes.
        found: usize,
        /// The configured limit.
        limit: usize,
    },
    /// The input has more segments than the configured limit.
    #[error("edi.too_many_segments: more than {limit} segments")]
    TooManySegments {
        /// The configured limit.
        limit: usize,
    },
    /// A segment has more elements than the configured limit.
    #[error("edi.too_many_elements: segment {tag} has more than {limit} elements")]
    TooManyElements {
        /// The segment's tag.
        tag: String,
        /// The configured limit.
        limit: usize,
    },
    /// A segment is malformed.
    #[error("edi.bad_segment: {reason}")]
    BadSegment {
        /// What was wrong.
        reason: String,
    },
    /// The header or trailer of an envelope is absent.
    #[error("edi.missing_envelope: the {syntax} interchange has no {what} segment")]
    MissingEnvelope {
        /// The syntax name (`X12` or `EDIFACT`).
        syntax: &'static str,
        /// The missing segment role (`header` or `trailer`).
        what: &'static str,
    },
    /// An opening envelope segment has no matching closing segment.
    #[error("edi.unbalanced_envelope: the {open} at segment {index} has no {close}")]
    UnbalancedEnvelope {
        /// The opening segment's tag.
        open: String,
        /// The closing segment's tag.
        close: String,
        /// The index of the opening segment.
        index: usize,
    },
    /// A declared count disagrees with the content.
    #[error("edi.envelope_count: {what} declares {declared} but the content has {found}")]
    EnvelopeCount {
        /// What declared the count (`IEA`, `GE`, `SE`, ...).
        what: String,
        /// The declared count.
        declared: u64,
        /// The count the content holds.
        found: u64,
    },
    /// Two control numbers that must match do not.
    #[error("edi.envelope_control: {what} control {found} does not match {expected}")]
    EnvelopeControl {
        /// What carries the mismatched number (`IEA`, `GE`, `SE`, ...).
        what: String,
        /// The control number found.
        found: String,
        /// The control number expected.
        expected: String,
    },
    /// Envelope segments are in the wrong order.
    #[error("edi.envelope_order: {reason}")]
    EnvelopeOrder {
        /// What is out of order.
        reason: String,
    },
    /// No schema has the requested id.
    #[error("edi.schema_unknown: no schema with id {id}")]
    SchemaUnknown {
        /// The requested schema id.
        id: String,
    },
    /// A schema document is not valid.
    #[error("edi.schema_bad: schema {id} is not valid: {reason}")]
    SchemaBad {
        /// The schema id, or the source when it could not be read.
        id: String,
        /// What was wrong.
        reason: String,
    },
    /// A mapping rule failed.
    #[error("edi.mapping: rule {rule} failed: {reason}")]
    Mapping {
        /// The rule that failed.
        rule: String,
        /// Why.
        reason: String,
    },
    /// The interchange holds no transaction set or message.
    #[error("edi.no_message: the interchange holds no transaction set or message")]
    NoMessage,
    /// An acknowledgement cannot be built.
    #[error("edi.ack_failed: {reason}")]
    AckFailed {
        /// Why.
        reason: String,
    },
    /// The connector operation's input is not valid.
    #[error("edi.bad_input: {reason}")]
    BadInput {
        /// What was wrong.
        reason: String,
    },
    /// The connector has no such operation.
    #[error("edi.unknown_operation: the connector has no operation named {operation}")]
    UnknownOperation {
        /// The operation name.
        operation: String,
    },
}

impl EdiError {
    /// The stable machine-readable code, also the prefix of the message.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotEdi => codes::NOT_EDI,
            Self::BadEncoding { .. } => codes::BAD_ENCODING,
            Self::BadDelimiters { .. } => codes::BAD_DELIMITERS,
            Self::TooLarge { .. } => codes::TOO_LARGE,
            Self::TooManySegments { .. } => codes::TOO_MANY_SEGMENTS,
            Self::TooManyElements { .. } => codes::TOO_MANY_ELEMENTS,
            Self::BadSegment { .. } => codes::BAD_SEGMENT,
            Self::MissingEnvelope { .. } => codes::MISSING_ENVELOPE,
            Self::UnbalancedEnvelope { .. } => codes::UNBALANCED_ENVELOPE,
            Self::EnvelopeCount { .. } => codes::ENVELOPE_COUNT,
            Self::EnvelopeControl { .. } => codes::ENVELOPE_CONTROL,
            Self::EnvelopeOrder { .. } => codes::ENVELOPE_ORDER,
            Self::SchemaUnknown { .. } => codes::SCHEMA_UNKNOWN,
            Self::SchemaBad { .. } => codes::SCHEMA_BAD,
            Self::Mapping { .. } => codes::MAPPING,
            Self::NoMessage => codes::NO_MESSAGE,
            Self::AckFailed { .. } => codes::ACK_FAILED,
            Self::BadInput { .. } => codes::BAD_INPUT,
            Self::UnknownOperation { .. } => codes::UNKNOWN_OPERATION,
        }
    }
}

/// The result type of this crate.
pub type Result<T> = std::result::Result<T, EdiError>;
