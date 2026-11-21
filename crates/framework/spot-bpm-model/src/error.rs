//! Construction and text errors for `spot-bpm-model`.
//!
//! Two error types, deliberately small:
//!
//! - [`ModelError`] is raised when a value is built in code with a bad id,
//!   kind or float-carrying config (ADR-12: never a float for money,
//!   quantities or time).
//! - [`TextError`] is raised when the TOML text form cannot be read or
//!   written. Semantic problems are *not* parse failures: [`validate`] reports
//!   them as [`Problem`](crate::Problem)s, so an unknown node kind still parses
//!   and is reported, never rejected (ADR-080).
//!
//! Neither carries a dotted code: the stable vocabulary of this crate lives in
//! [`Problem`](crate::Problem) (`bpm_model.*`, catalogued in
//! `crates/framework/spot-errors/catalogue/bpm_model.toml`). Messages never
//! name a secret or a raw SQL string.

use crate::id::NodeId;

/// A value could not be built: a bad name, a bad kind, or a float in config.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ModelError {
    /// A node id, process name or rule id does not match
    /// `[a-z][a-z0-9_]{0,63}`.
    #[error("invalid name `{raw}`: must match [a-z][a-z0-9_]{{0,63}}")]
    InvalidName {
        /// The rejected text.
        raw: String,
    },
    /// A node kind or rule expression is empty.
    #[error("invalid kind `{raw}`: a node kind is not empty")]
    KindInvalid {
        /// The rejected kind text.
        raw: String,
    },
    /// A float was found inside a node's config (ADR-12).
    #[error("float in config of node `{node}` at `{path}`")]
    FloatInConfig {
        /// The node holding the offending config.
        node: NodeId,
        /// Path to the float inside the config, e.g. `amounts[2]`.
        path: Box<str>,
    },
}

/// The TOML text form could not be read or written.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum TextError {
    /// The text is not well-formed TOML.
    #[error("not valid TOML: {message}")]
    Toml {
        /// The parser's message (already names the offending key or path).
        message: String,
    },
    /// The TOML is well-formed but is not a process definition document.
    #[error("not a process definition: {message}")]
    Shape {
        /// What is missing or the wrong type.
        message: String,
    },
    /// A name in the document breaks the id grammar.
    #[error(transparent)]
    Name(#[from] ModelError),
}
