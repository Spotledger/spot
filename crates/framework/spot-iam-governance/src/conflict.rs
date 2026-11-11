//! Why a grant is blocked or needs an override (ADR-070 section 8, rule 1).
//!
//! A [`Conflict`] is either a segregation-of-duties conflict (from `spot-sod`, with the full
//! reason of which side is held how) or a [`Refusal`] — a policy reason the grant may not
//! proceed: a delegated-administration limit, an IdP-managed role, or an incomplete override.
//! Only SoD conflicts can be overridden; a refusal never can.

use serde::{Deserialize, Serialize};
use spot_sod::Severity;

/// A policy reason a grant is refused. The `code` is the stable `iam_governance.*` code, so a
/// caller can branch on it without matching the message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct Refusal {
    /// The stable `iam_governance.*` code.
    pub code: String,
    /// What the refusal is about (the role, the request).
    pub subject: String,
    /// Why the grant is refused, in words.
    pub detail: String,
}

impl Refusal {
    /// A refusal with this code, subject and detail.
    #[must_use]
    pub fn new(code: &str, subject: &str, detail: &str) -> Refusal {
        Refusal {
            code: code.to_owned(),
            subject: subject.to_owned(),
            detail: detail.to_owned(),
        }
    }
}

/// Why a grant is blocked or needs an override.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Conflict {
    /// A segregation-of-duties conflict the grant would create.
    Sod(spot_sod::Conflict),
    /// A policy refusal: delegation, an IdP-managed role, or an incomplete override.
    Refusal(Refusal),
}

impl Conflict {
    /// The stable code: the SoD rule id for a conflict, the refusal's code for a refusal.
    #[must_use]
    pub fn code(&self) -> &str {
        match self {
            Conflict::Sod(c) => &c.rule_id,
            Conflict::Refusal(r) => &r.code,
        }
    }

    /// The rule id, when this is a segregation-of-duties conflict.
    #[must_use]
    pub fn rule_id(&self) -> Option<&str> {
        match self {
            Conflict::Sod(c) => Some(&c.rule_id),
            Conflict::Refusal(_) => None,
        }
    }

    /// The severity, when this is a segregation-of-duties conflict.
    #[must_use]
    pub fn severity(&self) -> Option<Severity> {
        match self {
            Conflict::Sod(c) => Some(c.severity),
            Conflict::Refusal(_) => None,
        }
    }

    /// The user the conflict is about, when it is a segregation-of-duties conflict.
    #[must_use]
    pub fn user(&self) -> Option<&str> {
        match self {
            Conflict::Sod(c) => Some(&c.user),
            Conflict::Refusal(_) => None,
        }
    }

    /// The scope the conflict sits in, when it is a segregation-of-duties conflict.
    #[must_use]
    pub fn scope(&self) -> Option<&str> {
        match self {
            Conflict::Sod(c) => c.scope.as_deref(),
            Conflict::Refusal(_) => None,
        }
    }

    /// Whether this conflict may be overridden (only SoD conflicts may).
    #[must_use]
    pub fn is_overridable(&self) -> bool {
        matches!(self, Conflict::Sod(_))
    }

    /// The segregation-of-duties conflict, when this is one.
    #[must_use]
    pub fn as_sod(&self) -> Option<&spot_sod::Conflict> {
        match self {
            Conflict::Sod(c) => Some(c),
            Conflict::Refusal(_) => None,
        }
    }
}

/// Sorts conflicts deterministically: SoD conflicts first by (severity descending, rule id,
/// scope), then refusals by (code, subject). The output of the gate does not depend on the
/// input order.
pub(crate) fn sort_conflicts(conflicts: &mut [Conflict]) {
    conflicts.sort_by_key(key);
}

/// A sort key: SoD before refusals, severity descending inside SoD.
fn key(c: &Conflict) -> (u8, std::cmp::Reverse<Severity>, String, String, String) {
    match c {
        Conflict::Sod(s) => (
            0,
            std::cmp::Reverse(s.severity),
            s.rule_id.clone(),
            s.scope.clone().unwrap_or_default(),
            s.user.clone(),
        ),
        Conflict::Refusal(r) => (
            1,
            std::cmp::Reverse(Severity::Low),
            r.code.clone(),
            String::new(),
            r.subject.clone(),
        ),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn only_sod_conflicts_are_overridable() {
        let refusal =
            Conflict::Refusal(Refusal::new("iam_governance.delegation_denied", "r", "no"));
        assert!(!refusal.is_overridable());
        assert_eq!(refusal.rule_id(), None);
        assert_eq!(refusal.code(), "iam_governance.delegation_denied");
    }
}
