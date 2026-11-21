//! Typed errors and refusals with stable `sync_healing.*` codes.
//!
//! Callers match on [`HealingError::code`], never on the message. Every code is catalogued in
//! `crates/framework/spot-errors/catalogue/sync_healing.toml`.

/// What can go wrong in the healing driver.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HealingError {
    /// The settings cannot be used (a zero limit, or a backoff base above its cap).
    #[error("invalid healing settings: {reason}")]
    InvalidSettings {
        /// What was wrong.
        reason: String,
    },

    /// The direction of the repair is not certain: a role is missing, or the roles say the
    /// source is not an owner and the destination a mirror. Nothing is repaired.
    #[error("no repair for {node} / {scope}: the direction is not certain")]
    DirectionAmbiguous {
        /// The destination node of the comparison.
        node: String,
        /// The scope label.
        scope: String,
    },

    /// The mirror holds a newer version of a record than its owner. Nothing is repaired.
    #[error("no repair for {node} / {scope}: the mirror is ahead of its owner")]
    MirrorAhead {
        /// The destination node of the comparison.
        node: String,
        /// The scope label.
        scope: String,
    },
}

impl HealingError {
    /// The stable machine-readable code for this error.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidSettings { .. } => "sync_healing.invalid_settings",
            Self::DirectionAmbiguous { .. } => "sync_healing.direction_ambiguous",
            Self::MirrorAhead { .. } => "sync_healing.mirror_ahead",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_stable_and_unique() {
        let errors = [
            HealingError::InvalidSettings { reason: "x".into() },
            HealingError::DirectionAmbiguous {
                node: "n".into(),
                scope: "s".into(),
            },
            HealingError::MirrorAhead {
                node: "n".into(),
                scope: "s".into(),
            },
        ];
        let mut seen = std::collections::BTreeSet::new();
        for error in &errors {
            assert!(error.code().starts_with("sync_healing."));
            assert!(seen.insert(error.code()));
        }
        assert_eq!(seen.len(), 3);
    }
}
