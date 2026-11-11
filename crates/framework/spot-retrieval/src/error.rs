//! Typed errors of the retrieval crate, with the stable `retrieval.*` codes of
//! `crates/framework/spot-errors/catalogue/retrieval.toml`.
//!
//! A caller matches on [`RetrievalError::code`], never on the text. Nothing here names a
//! document's content, so an error can be logged without leaking a value.

/// The `retrieval.*` vocabulary this crate defines, so the error-code scan sees every code
/// exactly where it is used. `CODES` itself is never a raise site — each code's literal
/// appears again in the [`RetrievalError::code`] match.
pub const CODES: [&str; 11] = [
    CODE_POLICY_PARSE,
    CODE_POLICY_FIELD_RESTRICTED,
    CODE_POLICY_UNKNOWN_DOCTYPE,
    CODE_POLICY_UNKNOWN_FIELD,
    CODE_POLICY_INVALID,
    CODE_EMBED_FAILED,
    CODE_STORE_FAILED,
    CODE_POSITION_FAILED,
    CODE_FEED_FAILED,
    CODE_DOCUMENT_TOO_LARGE,
    CODE_QUERY_TOO_LONG,
];

/// The index policy is not valid TOML, or carries a key the policy does not know.
pub const CODE_POLICY_PARSE: &str = "retrieval.policy_parse";
/// The policy indexes a field the permission rules mark secret, masked or hidden, and does
/// not list it under `restricted_ok`.
pub const CODE_POLICY_FIELD_RESTRICTED: &str = "retrieval.policy_field_restricted";
/// The policy names a DocType the permission snapshot does not know.
pub const CODE_POLICY_UNKNOWN_DOCTYPE: &str = "retrieval.policy_unknown_doctype";
/// The policy names a field the DocType does not have.
pub const CODE_POLICY_UNKNOWN_FIELD: &str = "retrieval.policy_unknown_field";
/// The policy is internally inconsistent (a duplicate field, a chunk window that cannot
/// progress, `restricted_ok` naming a field that is not indexed).
pub const CODE_POLICY_INVALID: &str = "retrieval.policy_invalid";
/// The embedding provider failed, or returned a shape the indexer cannot accept.
pub const CODE_EMBED_FAILED: &str = "retrieval.embed_failed";
/// The vector store refused a write or a read.
pub const CODE_STORE_FAILED: &str = "retrieval.store_failed";
/// The durable feed position could not be saved.
pub const CODE_POSITION_FAILED: &str = "retrieval.position_failed";
/// Reading the change feed failed.
pub const CODE_FEED_FAILED: &str = "retrieval.feed_failed";
/// A document's indexable text is larger than the indexer will embed; the document is
/// skipped, not the run.
pub const CODE_DOCUMENT_TOO_LARGE: &str = "retrieval.document_too_large";
/// The query text is longer than the retriever will embed.
pub const CODE_QUERY_TOO_LONG: &str = "retrieval.query_too_long";

/// One retrieval failure. Every variant carries its stable code; none carries a document
/// value.
#[derive(Debug, thiserror::Error)]
pub enum RetrievalError {
    /// The policy text is not valid TOML, or an unknown key appeared.
    #[error("the index policy could not be parsed: {detail}")]
    PolicyParse {
        /// The parser's message, which quotes only policy keys, never document values.
        detail: String,
    },

    /// The policy indexes a restricted field without an explicit allowance.
    #[error("the index policy indexes `{field}` of `{doctype}`, which is restricted")]
    PolicyFieldRestricted {
        /// The DocType.
        doctype: String,
        /// The field.
        field: String,
    },

    /// The policy names a DocType the permission snapshot does not know.
    #[error("the index policy names DocType `{doctype}`, which the metadata does not know")]
    PolicyUnknownDoctype {
        /// The DocType.
        doctype: String,
    },

    /// The policy names a field the DocType does not have.
    #[error("the index policy names field `{field}` of `{doctype}`, which does not exist")]
    PolicyUnknownField {
        /// The DocType.
        doctype: String,
        /// The field.
        field: String,
    },

    /// The policy is internally inconsistent.
    #[error("the index policy is inconsistent: {detail}")]
    PolicyInvalid {
        /// The reason, naming only policy keys.
        detail: String,
    },

    /// The embedding provider failed.
    #[error("the embedding provider failed: {detail}")]
    Embed {
        /// The provider's message, which must name no document value.
        detail: String,
    },

    /// The vector store failed.
    #[error("the vector store failed: {detail}")]
    Store {
        /// The store's message, which must name no document value.
        detail: String,
    },

    /// The durable position could not be saved.
    #[error("the retrieval position store failed: {detail}")]
    Position {
        /// The store's message.
        detail: String,
    },

    /// Reading the change feed failed.
    #[error("the change feed read failed: {detail}")]
    Feed {
        /// The feed's message.
        detail: String,
    },

    /// A document's indexable text is larger than the indexer will embed.
    #[error("document `{id}` of `{doctype}` has {chars} indexable characters, over the limit")]
    DocumentTooLarge {
        /// The DocType.
        doctype: String,
        /// The record id.
        id: String,
        /// How many characters the document's indexable fields hold.
        chars: usize,
    },

    /// The query text is longer than the retriever will embed.
    #[error("the query has {chars} characters, over the limit")]
    QueryTooLong {
        /// How many characters the query holds.
        chars: usize,
    },
}

impl RetrievalError {
    /// The stable machine-readable code, as `Catalogue::explain` accepts it.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::PolicyParse { .. } => CODE_POLICY_PARSE,
            Self::PolicyFieldRestricted { .. } => CODE_POLICY_FIELD_RESTRICTED,
            Self::PolicyUnknownDoctype { .. } => CODE_POLICY_UNKNOWN_DOCTYPE,
            Self::PolicyUnknownField { .. } => CODE_POLICY_UNKNOWN_FIELD,
            Self::PolicyInvalid { .. } => CODE_POLICY_INVALID,
            Self::Embed { .. } => CODE_EMBED_FAILED,
            Self::Store { .. } => CODE_STORE_FAILED,
            Self::Position { .. } => CODE_POSITION_FAILED,
            Self::Feed { .. } => CODE_FEED_FAILED,
            Self::DocumentTooLarge { .. } => CODE_DOCUMENT_TOO_LARGE,
            Self::QueryTooLong { .. } => CODE_QUERY_TOO_LONG,
        }
    }
}
