//! The one error type, with stable, machine-readable `sod.*` codes.
//!
//! Codes are stable: for logs, metrics and API responses use [`SodError::code`], never the
//! message text. Every load error names the rule id (or catalogue position) it was found in,
//! so a site fixing a rule file sees which rule each problem belongs to.

/// Something failed while loading or building a rule set or a role catalogue.
///
/// Variants carry what went wrong in words; `RuleSet::from_toml` returns the first problem,
/// deterministic in document order.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SodError {
    /// A rule-set or catalogue document failed to parse: it is not TOML of the expected
    /// shape, or a rule uses a key the model does not accept (a typo is an error, never an
    /// ignored key).
    #[error("cannot read {at}: {detail}")]
    Parse {
        /// The document or section the text came from.
        at: String,
        /// What the parser objected to.
        detail: String,
    },

    /// A rule id is named twice in one rule set, or a catalogue entry names a role twice.
    #[error("{rule}: {detail}")]
    Duplicate {
        /// The rule id (or catalogue position) the duplicate was found in.
        rule: String,
        /// What was duplicated.
        detail: String,
    },

    /// A rule has fewer than two sides, so it can never separate anything.
    #[error("{rule}: {detail}")]
    SideCount {
        /// The rule id the problem was found in.
        rule: String,
        /// How many sides the rule has.
        detail: String,
    },

    /// A side of a rule holds no duty (and no role), so it can never be satisfied.
    #[error("{rule}: {detail}")]
    SideEmpty {
        /// The rule id the problem was found in.
        rule: String,
        /// Which side is empty, and what a side needs.
        detail: String,
    },

    /// The same duty is named on two sides of one rule, so no user could ever hold one side
    /// without the other: the rule can never be violated and always means an authoring
    /// mistake.
    #[error("{rule}: {detail}")]
    Overlap {
        /// The rule id the problem was found in.
        rule: String,
        /// The duty shared between the sides.
        detail: String,
    },

    /// A severity text the model does not know.
    #[error("{rule}: {detail}")]
    Severity {
        /// The rule id the bad severity was found in.
        rule: String,
        /// The severity text and what is accepted.
        detail: String,
    },

    /// A name (rule id, duty, role or catalogue role) is missing, empty or only whitespace
    /// after trimming.
    #[error("{rule}: {detail}")]
    Name {
        /// The rule id or catalogue position the name was found in.
        rule: String,
        /// Which name is empty and what it must be.
        detail: String,
    },
}

/// The stable machine-readable code of an error, for logs, API responses and metrics.
impl SodError {
    /// The stable machine-readable code of this error.
    ///
    /// The codes, with their meanings:
    ///
    /// - `sod.parse_failed` — a document failed to parse (bad TOML or an unknown key);
    /// - `sod.duplicate_id` — a rule id or catalogue role named twice;
    /// - `sod.side_count` — a rule has fewer than two sides;
    /// - `sod.side_empty` — a side holds no duty;
    /// - `sod.side_overlap` — one duty on two sides of one rule;
    /// - `sod.severity_unknown` — a severity the model does not know;
    /// - `sod.name_empty` — a blank rule id, duty, role or catalogue name.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Parse { .. } => PARSE,
            Self::Duplicate { .. } => DUPLICATE,
            Self::SideCount { .. } => SIDE_COUNT,
            Self::SideEmpty { .. } => SIDE_EMPTY,
            Self::Overlap { .. } => OVERLAP,
            Self::Severity { .. } => SEVERITY,
            Self::Name { .. } => NAME,
        }
    }

    /// The rule id (or document position) the error names, for grouping problems by rule.
    #[must_use]
    pub fn rule(&self) -> &str {
        match self {
            Self::Parse { at, .. } => at,
            Self::Duplicate { rule, .. }
            | Self::SideCount { rule, .. }
            | Self::SideEmpty { rule, .. }
            | Self::Overlap { rule, .. }
            | Self::Severity { rule, .. }
            | Self::Name { rule, .. } => rule,
        }
    }
}

/// A rule-set or catalogue document failed to parse (bad TOML or an unknown key).
pub const PARSE: &str = "sod.parse_failed";
/// A rule id is named twice in one rule set, or a catalogue role is named twice.
pub const DUPLICATE: &str = "sod.duplicate_id";
/// A rule has fewer than two sides.
pub const SIDE_COUNT: &str = "sod.side_count";
/// A side of a rule holds no duty.
pub const SIDE_EMPTY: &str = "sod.side_empty";
/// The same duty is named on two sides of one rule.
pub const OVERLAP: &str = "sod.side_overlap";
/// A severity the model does not know.
pub const SEVERITY: &str = "sod.severity_unknown";
/// A blank rule id, duty, role or catalogue name.
pub const NAME: &str = "sod.name_empty";

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<(SodError, &'static str)> {
        vec![
            (
                SodError::Parse {
                    at: "rules".to_owned(),
                    detail: "bad".to_owned(),
                },
                PARSE,
            ),
            (
                SodError::Duplicate {
                    rule: "sod.a".to_owned(),
                    detail: "d".to_owned(),
                },
                DUPLICATE,
            ),
            (
                SodError::SideCount {
                    rule: "sod.a".to_owned(),
                    detail: "1".to_owned(),
                },
                SIDE_COUNT,
            ),
            (
                SodError::SideEmpty {
                    rule: "sod.a".to_owned(),
                    detail: "side 0".to_owned(),
                },
                SIDE_EMPTY,
            ),
            (
                SodError::Overlap {
                    rule: "sod.a".to_owned(),
                    detail: "x".to_owned(),
                },
                OVERLAP,
            ),
            (
                SodError::Severity {
                    rule: "sod.a".to_owned(),
                    detail: "severe".to_owned(),
                },
                SEVERITY,
            ),
            (
                SodError::Name {
                    rule: "sod.a".to_owned(),
                    detail: "id".to_owned(),
                },
                NAME,
            ),
        ]
    }

    #[test]
    fn codes_are_stable_and_cover_every_variant() {
        for (error, code) in sample() {
            assert_eq!(error.code(), code, "code drifted for {error:?}");
        }
    }

    #[test]
    fn every_error_names_its_rule_or_position() {
        for (error, _) in sample() {
            assert!(!error.rule().is_empty(), "{error:?} names nothing");
        }
    }
}
