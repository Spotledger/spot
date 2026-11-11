//! The three severities of ADR-067 section 1.

use serde::{Deserialize, Serialize};

/// How bad a condition is: `info`, `amber` (the system is handling it, or it is minor) or `red`
/// (a person is needed or damage is possible).
///
/// Ordered from least to most severe, so the worst of a set is the maximum and an escalation is
/// a step upwards. The order is the ADR's: [`Severity::Info`] < [`Severity::Amber`] <
/// [`Severity::Red`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Nothing is broken and nobody needs to know; the daily digest may mention it.
    Info,
    /// The system is handling it or it is minor: a daily digest and the screen, never a page.
    Amber,
    /// A person is needed, or damage is possible: the only severity that interrupts.
    Red,
}

impl Severity {
    /// Every severity, least severe first.
    pub const ALL: [Self; 3] = [Self::Info, Self::Amber, Self::Red];

    /// The wire name ADR-067 uses (`info`, `amber`, `red`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Amber => "amber",
            Self::Red => "red",
        }
    }

    /// Whether this severity needs a person without waiting for an age threshold.
    ///
    /// Only [`Severity::Red`] does; an amber condition joins the `needs you` list once it has
    /// been open past the configured threshold (ADR-067 section 6).
    #[must_use]
    pub const fn needs_a_person_at_once(self) -> bool {
        matches!(self, Self::Red)
    }
}
