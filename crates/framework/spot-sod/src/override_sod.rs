//! Overrides and the rights the grantor needs: an override covers exactly one (rule, user,
//! scope) and is accepted only when the grantor holds the `override_sod` right and records a
//! reason and a mitigating control (ADR-070 section 8, rule 1).

use serde::{Deserialize, Serialize};

use crate::analysis::GrantCheck;

/// The right a grantor must hold to record an SoD override (ADR-070 section 8, rule 1).
///
/// Defined here as a name, not a dependency: `spot-sod` treats rights as opaque strings and
/// the identity service (WP-IAM-08) decides what its own rights model calls them — the only
/// contract is this constant's text.
pub const OVERRIDE_SOD_RIGHT: &str = "override_sod";

/// The grantor's rights, as the decision sees them: the set of right names the grantor
/// holds.
///
/// A minimal stand-in for the permissions engine's right set (this crate does not depend on
/// `spot-permissions`; recorded as a judgement call in the task's report): the identity
/// service builds it from whatever its own model carries.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Rights(Vec<String>);

impl Rights {
    /// The rights named `rights`, deduplicated and sorted.
    #[must_use]
    pub fn new<I: IntoIterator<Item = S>, S: AsRef<str>>(rights: I) -> Rights {
        let mut set: Vec<String> = rights
            .into_iter()
            .map(|r| r.as_ref().trim().to_owned())
            .filter(|r| !r.is_empty())
            .collect();
        set.sort();
        set.dedup();
        Rights(set)
    }

    /// The rights, sorted.
    #[must_use]
    pub fn as_slice(&self) -> &[String] {
        &self.0
    }

    /// Whether the set holds the [`OVERRIDE_SOD_RIGHT`] right.
    #[must_use]
    pub fn has_override_sod(&self) -> bool {
        self.0.iter().any(|r| r == OVERRIDE_SOD_RIGHT)
    }
}

/// One recorded override of one SoD conflict: it covers **exactly** the (rule, user, scope)
/// triple it names — never a different rule, never a different user, never a different
/// scope. It is itself an auditable record (ADR-070 section 8, rule 4) and appears in the
/// access review.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct SodOverride {
    /// The rule id the override covers.
    pub rule_id: String,
    /// The user the override covers.
    pub user: String,
    /// The scope the override covers; `None` covers the unscoped conflict (and only that —
    /// an override with no scope does not silently cover every scope of a `same_scope`
    /// rule).
    pub scope: Option<String>,
    /// Why the conflict is tolerated: required, non-blank after trimming.
    pub reason: String,
    /// The compensating control that makes it tolerable: required, non-blank after
    /// trimming.
    pub mitigating_control: String,
    /// Who granted the override (the grantor's user name).
    pub granted_by: String,
    /// When it was granted, as a plain instant (`i64`); this crate reads no clock, so the
    /// caller stamps it.
    pub granted_at: i64,
}

impl SodOverride {
    /// Whether this override covers the conflict (rule `rule_id`, user `user`, scope
    /// `scope`): all three must match exactly — a `None` scope here means the unscoped
    /// conflict only.
    #[must_use]
    pub fn covers(&self, rule_id: &str, user: &str, scope: Option<&str>) -> bool {
        self.rule_id == rule_id && self.user == user && self.scope == scope.map(str::to_owned)
    }
}

/// What [`apply_override`] decided.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OverrideDecision {
    /// The grant may proceed: every conflicting rule is covered and the records are complete.
    Allowed,
    /// The override is refused; every gap is listed, not just the first.
    Refused(OverrideRefusal),
}

/// Why one conflicting rule has no acceptable override.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Uncovered {
    /// No override names this rule and user at all.
    NoOverride,
    /// An override names the rule and user but a **different scope**, so it does not cover
    /// this conflict (an override covers one scope only).
    ScopeMismatch {
        /// The scope the override on record covers instead.
        recorded: Option<String>,
    },
}

/// Why an override was refused. Every missing thing is listed at once: the grantor sees all
/// gaps, not the first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OverrideRefusal {
    /// The grantor does not hold the `override_sod` right.
    pub missing_right: bool,
    /// The conflicting rules with no acceptable override, as (rule id, why uncovered).
    pub uncovered_rules: Vec<(String, Uncovered)>,
    /// Whether the matching override's `reason` was blank after trimming.
    pub blank_reason: bool,
    /// Whether the matching override's `mitigating_control` was blank after trimming.
    pub blank_mitigating_control: bool,
}

/// Applies the grantor's override attempts to a [`GrantCheck`]: allowed only when the
/// grantor holds the [`OVERRIDE_SOD_RIGHT`] right, **every** conflicting rule has an
/// override matching this (user, scope), and each matching override's `reason` and
/// `mitigating_control` are non-blank after trimming.
///
/// An override covers one (rule, user, scope) only and never silently covers a different
/// rule; a refusal lists exactly what is missing ([`OverrideRefusal`]). A
/// [`GrantCheck::Clear`] needs no override and is allowed whatever the rights: nothing is
/// being overridden.
#[must_use]
pub fn apply_override(
    check: &GrantCheck,
    overrides: &[SodOverride],
    grantor_rights: &Rights,
) -> OverrideDecision {
    let GrantCheck::Conflicts(conflicts) = check else {
        return OverrideDecision::Allowed;
    };
    let mut refusal = OverrideRefusal {
        missing_right: !grantor_rights.has_override_sod(),
        uncovered_rules: Vec::new(),
        blank_reason: false,
        blank_mitigating_control: false,
    };
    for conflict in conflicts {
        // The overrides naming this rule and user; the scope may still be wrong.
        let covering: Vec<&SodOverride> = overrides
            .iter()
            .filter(|ov| {
                ov.rule_id == conflict.rule_id
                    && ov.user == conflict.user
                    && ov.covers(&conflict.rule_id, &conflict.user, conflict.scope.as_deref())
            })
            .collect();
        if covering.is_empty() {
            // Why nothing covers it: no override at all for this rule and user, or one on a
            // different scope only.
            let named: Vec<&SodOverride> = overrides
                .iter()
                .filter(|ov| ov.rule_id == conflict.rule_id && ov.user == conflict.user)
                .collect();
            let why = named
                .first()
                .map_or(Uncovered::NoOverride, |ov| Uncovered::ScopeMismatch {
                    recorded: ov.scope.clone(),
                });
            refusal
                .uncovered_rules
                .push((conflict.rule_id.clone(), why));
            continue;
        }
        // The covering override's reason and control must both be non-blank. When several
        // cover the same conflict (the caller may pass duplicates), a blank in any is a
        // gap the grantor must fix.
        for ov in covering {
            if ov.reason.trim().is_empty() {
                refusal.blank_reason = true;
            }
            if ov.mitigating_control.trim().is_empty() {
                refusal.blank_mitigating_control = true;
            }
        }
    }
    if refusal.missing_right
        || !refusal.uncovered_rules.is_empty()
        || refusal.blank_reason
        || refusal.blank_mitigating_control
    {
        OverrideDecision::Refused(refusal)
    } else {
        OverrideDecision::Allowed
    }
}
