//! The governance mode of one record type in one scope (ADR-080 section 2.1).

use serde::{Deserialize, Serialize};

/// How a record type is entered in a scope.
///
/// The mode is a setting value, per record type and scope, and **`free` is the default**: a
/// platform that changes nothing behaves exactly as it did before governance existed.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum GovernanceMode {
    /// No governance: a record may be created from any channel (the default, "off").
    #[default]
    Free,
    /// A record may be created only inside a running request, or through the audited
    /// outside-process permission with a reason (ADR-49).
    Governed,
    /// As `governed`, and the generic "New" action is replaced by "Start", with **no bypass**:
    /// this crate offers no permission or override route and defers to the entry crate
    /// (WP-BPM-13, ADR-080 section 2.3).
    ProcessOnly,
}

impl GovernanceMode {
    /// The wire text: `free`, `governed` or `process_only`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Free => "free",
            Self::Governed => "governed",
            Self::ProcessOnly => "process_only",
        }
    }

    /// Parses the wire text. Unknown text is `None`, never a guess.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "free" => Some(Self::Free),
            "governed" => Some(Self::Governed),
            "process_only" => Some(Self::ProcessOnly),
            _ => None,
        }
    }

    /// Every mode, in declaration order.
    #[must_use]
    pub fn all() -> [Self; 3] {
        [Self::Free, Self::Governed, Self::ProcessOnly]
    }

    /// Whether this mode is enforced by *this* crate's guard. `process_only` is not: it defers
    /// to the entry crate, so this crate must offer no bypass for it.
    #[must_use]
    pub fn enforced_here(self) -> bool {
        matches!(self, Self::Free | Self::Governed)
    }
}

impl std::fmt::Display for GovernanceMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
