//! The operations a Document node performs.

/// What a Document node does to its document.
///
/// Every operation that writes goes through the document engine's own lifecycle, so a step
/// changes a document exactly as a user's click would (ADR-49, ADR-080 section 1). `Map` writes
/// no document: it evaluates a mapping into the Case's variables only.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Operation {
    /// Create a new document from a field mapping.
    CreateFrom,
    /// Update a draft through the update lifecycle.
    Update,
    /// Submit (post) a draft through the post lifecycle.
    Submit,
    /// Cancel a posted document through the cancel lifecycle.
    Cancel,
    /// Evaluate a mapping into the Case's variables without touching a document.
    Map,
}

impl Operation {
    /// The token the node config, the node output and the Case file spell.
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Self::CreateFrom => "create_from",
            Self::Update => "update",
            Self::Submit => "submit",
            Self::Cancel => "cancel",
            Self::Map => "map",
        }
    }

    /// Parses an operation token.
    ///
    /// The process model's `DocumentAction` tokens are accepted as aliases: `create` is
    /// `create_from`, and `post` is `submit`.
    #[must_use]
    pub fn parse(token: &str) -> Option<Self> {
        match token {
            "create_from" | "create" => Some(Self::CreateFrom),
            "update" => Some(Self::Update),
            "submit" | "post" => Some(Self::Submit),
            "cancel" => Some(Self::Cancel),
            "map" => Some(Self::Map),
            _ => None,
        }
    }

    /// The inverse operation a compensation step declares, when the operation has one.
    ///
    /// A `submit` is undone by a `cancel`; the other operations have no inverse here (ADR-080
    /// section 6). WP-BPM-09 reads the declaration the node config carries.
    #[must_use]
    pub fn compensation(self) -> Option<Self> {
        match self {
            Self::Submit => Some(Self::Cancel),
            _ => None,
        }
    }

    /// Whether the operation writes a document (and so records a Case-file entry).
    #[must_use]
    pub fn writes(self) -> bool {
        !matches!(self, Self::Map)
    }

    /// Whether the operation changes a document that already exists (and so needs its id).
    #[must_use]
    pub fn targets_existing(self) -> bool {
        matches!(self, Self::Update | Self::Submit | Self::Cancel)
    }
}
