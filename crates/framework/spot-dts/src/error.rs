//! The generator's errors. Every error names what it is about; a collision names both sources.

use std::fmt;

/// The kind of a [`DtsError`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DtsErrorKind {
    /// A name that cannot become valid TypeScript (`InvalidName`).
    InvalidName,
    /// Two source names mapping onto one declared name (`Collision`).
    Collision,
    /// A reference to a DocType the input does not contain (`UnknownDocType`).
    UnknownDocType,
}

/// One generation error: the kind, the message, and the DocType it is about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DtsError {
    /// The kind.
    pub kind: DtsErrorKind,
    /// The message, naming the sources involved.
    pub message: String,
    /// The DocType the error is about, when one is.
    pub doctype: Option<String>,
}

impl DtsError {
    /// An error of `kind` carrying `message`.
    #[must_use]
    pub fn new(kind: DtsErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            doctype: None,
        }
    }

    /// Attaches the DocType the error is about (builder style).
    #[must_use]
    pub fn about_name(mut self, doctype: &str) -> Self {
        self.doctype = Some(doctype.to_owned());
        self
    }
}

impl fmt::Display for DtsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for DtsError {}
