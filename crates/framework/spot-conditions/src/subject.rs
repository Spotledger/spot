//! What a condition is about.

use std::borrow::Borrow;
use std::fmt;

use serde::{Deserialize, Serialize};

/// The subject of a condition: the thing that is in the abnormal state (ADR-067 section 1).
///
/// A node, a stream, an entity, a record, the tenant, or an app's own kind of thing. The engine
/// treats it as opaque text compared exactly, so an app names its subjects in its own scheme and
/// nothing here parses them; the definition says what kind they are
/// ([`ConditionDef::subject_kind`](crate::ConditionDef::subject_kind)).
///
/// A condition is de-duplicated per `(id, subject)`, not per id: two nodes with the same problem
/// are two conditions.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Subject(String);

impl Subject {
    /// The given text as a subject.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    /// The subject as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for Subject {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Subject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for Subject {
    fn from(text: &str) -> Self {
        Self(text.to_owned())
    }
}

impl From<String> for Subject {
    fn from(text: String) -> Self {
        Self(text)
    }
}
