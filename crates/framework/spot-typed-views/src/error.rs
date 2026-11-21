//! Generation errors: one per problem, naming the DocType and the field.

use std::fmt;

/// What went wrong, for callers that match on the cause rather than the text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum GenErrorKind {
    /// A name is not a snake_case storage name, or maps onto no Rust identifier.
    InvalidName,
    /// A select field has no options, or a table field no child DocType.
    BadField,
    /// Two names map onto the same Rust identifier.
    Collision,
}

/// One generation problem.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct GenError {
    /// The cause.
    pub kind: GenErrorKind,
    /// The DocType the problem is about.
    pub doctype: Option<String>,
    /// The field the problem is about.
    pub field: Option<String>,
    /// What is wrong, in a sentence.
    pub message: String,
}

impl GenError {
    /// An error with only a kind and a message.
    #[must_use]
    pub fn new(kind: GenErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            doctype: None,
            field: None,
            message: message.into(),
        }
    }

    /// Names the DocType.
    #[must_use]
    pub fn in_doctype(mut self, doctype: &str) -> Self {
        self.doctype = Some(doctype.to_owned());
        self
    }

    /// Names the field.
    #[must_use]
    pub fn in_field(mut self, field: &str) -> Self {
        self.field = Some(field.to_owned());
        self
    }

    /// The error as one printable line.
    #[must_use]
    pub fn line(&self) -> String {
        self.to_string()
    }
}

impl fmt::Display for GenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.doctype, &self.field) {
            (Some(d), Some(fl)) => write!(f, "`{d}.{fl}`: ")?,
            (Some(d), None) => write!(f, "`{d}`: ")?,
            (None, Some(fl)) => write!(f, "field `{fl}`: ")?,
            (None, None) => {}
        }
        f.write_str(&self.message)
    }
}

impl std::error::Error for GenError {}
