//! The analysis: which users hold a forbidden combination today
//! ([`analyze`]), what one proposed grant would create ([`check_grant`]), and which single
//! role already carries both sides of a rule ([`internal_conflicts`], the "Workday finding").
//!
//! Everything is deterministic: holdings are canonicalised (sorted, deduplicated — a direct
//! duty before the same duty through a role), and the conflicts come out sorted by (user,
//! severity descending, rule id, scope), whatever order the assignments were given in.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::model::{
    Assignment, Conflict, Duty, Grant, HeldDuty, Hold, Role, RoleCatalogue, RuleSet, ScopeMode,
    Severity, SodRule, Via,
};
use crate::override_sod::SodOverride;

/// One hold of a duty: the duty, the scope it is held in, and how it came to be held.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Holding {
    duty: Duty,
    scope: Option<String>,
    via: Via,
}

/// What one user holds, after the catalogue's expansion: duties (direct and through roles)
/// and the roles themselves (a rule may name a role directly).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Holdings {
    duties: Vec<Holding>,
    roles: Vec<(Role, Option<String>)>,
}

impl Holdings {
    /// Expands one user's assignments into holdings, canonical: sorted and deduplicated, a
    /// direct hold before the same hold through a role ([`Via`]'s enum order).
    fn of(assignments: &[Assignment], user: &str, catalogue: &RoleCatalogue) -> Holdings {
        let mut duties: Vec<Holding> = Vec::new();
        let mut roles: Vec<(Role, Option<String>)> = Vec::new();
        for a in assignments.iter().filter(|a| a.user == user) {
            let scope = a.scope.clone();
            match &a.grant {
                Grant::Duty(duty) => duties.push(Holding {
                    duty: duty.clone(),
                    scope,
                    via: Via::Direct,
                }),
                Grant::Role(role) => {
                    roles.push((role.clone(), scope.clone()));
                    for duty in catalogue.duties_of(role) {
                        duties.push(Holding {
                            duty,
                            scope: scope.clone(),
                            via: Via::ThroughRole { role: role.clone() },
                        });
                    }
                }
            }
        }
        duties.sort();
        duties.dedup();
        roles.sort();
        roles.dedup();
        Holdings { duties, roles }
    }
}

/// Whether a hold's scope counts under `mode` against the conflict's anchor scope.
fn scope_matches(mode: ScopeMode, scope: &Option<String>, anchor: &Option<String>) -> bool {
    match mode {
        ScopeMode::AnyScope => true,
        ScopeMode::SameScope => scope == anchor,
    }
}

/// The first hold of `side` (in canonical holdings order) whose scope counts under `mode`
/// against `anchor`, as a [`HeldDuty`] for `side_index` of the rule.
fn satisfied_hold(
    side_index: usize,
    side: &crate::model::Side,
    holdings: &Holdings,
    mode: ScopeMode,
    anchor: &Option<String>,
) -> Option<HeldDuty> {
    for holding in &holdings.duties {
        if side.names_duty(&holding.duty) && scope_matches(mode, &holding.scope, anchor) {
            return Some(HeldDuty {
                side: side_index,
                hold: Hold::Duty {
                    duty: holding.duty.clone(),
                },
                via: holding.via.clone(),
                scope: holding.scope.clone(),
            });
        }
    }
    for (role, scope) in &holdings.roles {
        if side.names_role(role) && scope_matches(mode, scope, anchor) {
            return Some(HeldDuty {
                side: side_index,
                hold: Hold::Role { role: role.clone() },
                via: Via::Direct,
                scope: scope.clone(),
            });
        }
    }
    None
}

/// Every conflict `rule` and `user`'s holdings produce: one per (rule, user, scope). A
/// `same_scope` rule is violated once per shared scope; an `any_scope` rule is violated
/// once, unscoped.
fn conflicts_for_user(rule: &SodRule, user: &str, holdings: &Holdings) -> Vec<Conflict> {
    match rule.scope {
        ScopeMode::AnyScope => {
            let Some(first) = satisfied_hold(0, &rule.sides[0], holdings, rule.scope, &None) else {
                return vec![];
            };
            let Some(held) = collect_held(rule, holdings, first, &None) else {
                return vec![];
            };
            vec![one_conflict(rule, user, None, held)]
        }
        ScopeMode::SameScope => {
            // Every distinct scope the first side is satisfied in anchors one candidate
            // conflict; the other sides must match the anchor's scope exactly.
            let anchors: BTreeSet<Option<String>> = holdings
                .duties
                .iter()
                .filter(|h| rule.sides[0].names_duty(&h.duty))
                .map(|h| h.scope.clone())
                .chain(
                    holdings
                        .roles
                        .iter()
                        .filter(|(r, _)| rule.sides[0].names_role(r))
                        .map(|(_, s)| s.clone()),
                )
                .collect();
            let mut out = Vec::new();
            for anchor in anchors {
                let Some(first) = satisfied_hold(0, &rule.sides[0], holdings, rule.scope, &anchor)
                else {
                    continue;
                };
                if let Some(held) = collect_held(rule, holdings, first, &anchor) {
                    out.push(one_conflict(rule, user, anchor, held));
                }
            }
            out
        }
    }
}

/// The `held` list for a conflict whose first side is held as `first`: the first side's
/// entry plus one entry per later side, when every later side is satisfied; `None` when any
/// later side is not.
fn collect_held(
    rule: &SodRule,
    holdings: &Holdings,
    first: HeldDuty,
    anchor: &Option<String>,
) -> Option<Vec<HeldDuty>> {
    let mut held = vec![first];
    for (i, side) in rule.sides.iter().enumerate().skip(1) {
        let next = satisfied_hold(i, side, holdings, rule.scope, anchor)?;
        held.push(next);
    }
    Some(held)
}

/// The conflict record for one (rule, user, scope) with its `held` list.
fn one_conflict(
    rule: &SodRule,
    user: &str,
    scope: Option<String>,
    held: Vec<HeldDuty>,
) -> Conflict {
    Conflict {
        rule_id: rule.id.clone(),
        severity: rule.severity,
        user: user.to_owned(),
        scope,
        held,
        overridden: false,
    }
}

/// The identity of a conflict for the grant-time difference: (rule, user, scope). A conflict
/// keyed the same in both analyses is the same conflict, however its sides came to be held.
type ConflictKey = (String, String, Option<String>);

/// The key of one [`Conflict`].
fn key_of(conflict: &Conflict) -> ConflictKey {
    (
        conflict.rule_id.clone(),
        conflict.user.clone(),
        conflict.scope.clone(),
    )
}

/// The analysis: every rule violated by any user's holdings, one [`Conflict`] per (rule,
/// user, scope), sorted by (user, severity descending, rule id, scope) — the output order is
/// total on the sort key, so it does not depend on the order of the input.
///
/// Overrides (see [`crate::apply_override`]) mark the conflicts they cover
/// `overridden: true` but never remove them: the access review must show them.
#[must_use]
pub fn analyze(
    rules: &RuleSet,
    catalogue: &RoleCatalogue,
    assignments: &[Assignment],
    overrides: &[SodOverride],
) -> Vec<Conflict> {
    let users: BTreeSet<&str> = assignments.iter().map(|a| a.user.as_str()).collect();
    let mut out: Vec<Conflict> = Vec::new();
    for user in users {
        let holdings = Holdings::of(assignments, user, catalogue);
        for rule in rules.as_slice() {
            out.extend(conflicts_for_user(rule, user, &holdings));
        }
    }
    for conflict in &mut out {
        if overrides
            .iter()
            .any(|ov| ov.covers(&conflict.rule_id, &conflict.user, conflict.scope.as_deref()))
        {
            conflict.overridden = true;
        }
    }
    // The documented total order: user, severity descending, rule id, scope. (The struct's
    // derived order starts with the rule id, which is NOT the report order.)
    out.sort_by(|x, y| {
        x.user
            .cmp(&y.user)
            .then_with(|| Severity::cmp(&y.severity, &x.severity))
            .then_with(|| x.rule_id.cmp(&y.rule_id))
            .then_with(|| x.scope.cmp(&y.scope))
    });
    out
}

/// What the analysis would newly blame on a proposed grant.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GrantCheck {
    /// The grant creates no conflict the assignments do not already carry.
    Clear,
    /// The conflicts the proposed grant creates, sorted; a conflict already present before
    /// the grant — same rule, user **and** scope — is never blamed on it.
    Conflicts(Vec<Conflict>),
}

/// The grant-time check (ADR-070 section 8, rule 1: rules are evaluated when an assignment
/// is created or changed, before it takes effect): would `proposed` create a conflict?
///
/// The answer is the keyed difference `analyze(existing + proposed) − analyze(existing)`:
/// conflicts whose (rule, user, scope) the assignments do not already have. A user already
/// over the line stays an `analyze` finding, not this grant's blame; a grant into a *new*
/// scope of an already-violated rule **is** new and is reported.
#[must_use]
pub fn check_grant(
    rules: &RuleSet,
    catalogue: &RoleCatalogue,
    existing: &[Assignment],
    proposed: &Assignment,
    overrides: &[SodOverride],
) -> GrantCheck {
    let mut with_proposed: Vec<Assignment> = existing.to_vec();
    with_proposed.push(proposed.clone());
    let before: Vec<ConflictKey> = analyze(rules, catalogue, existing, overrides)
        .iter()
        .map(key_of)
        .collect();
    let created: Vec<Conflict> = analyze(rules, catalogue, &with_proposed, overrides)
        .into_iter()
        .filter(|c| !before.contains(&key_of(c)))
        .collect();
    if created.is_empty() {
        GrantCheck::Clear
    } else {
        GrantCheck::Conflicts(created)
    }
}

/// One **role** whose catalogue expansion already satisfies every side of a rule (ADR-070
/// section 8, rule 1: "Reference duties are authored without internal conflicts" — the
/// Workday finding, a delivered group that itself carried a conflict). One finding per
/// (rule, role).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct InternalConflict {
    /// The rule the role offends.
    pub rule_id: String,
    /// The severity of the rule offended.
    pub severity: Severity,
    /// The role holding both sides.
    pub role: Role,
    /// The role's duties satisfying the rule's first side.
    pub first_side: Vec<Duty>,
    /// The role's duties satisfying the rule's second side.
    pub second_side: Vec<Duty>,
}

/// The reference-duty hygiene check: which single roles of `catalogue` expand to duties
/// satisfying **every** side of a rule.
#[must_use]
pub fn internal_conflicts(rules: &RuleSet, catalogue: &RoleCatalogue) -> Vec<InternalConflict> {
    let mut out: Vec<InternalConflict> = Vec::new();
    for rule in rules.as_slice() {
        for (role, duties) in &catalogue.roles {
            let mut sides_ok: Vec<Vec<Duty>> = Vec::new();
            let mut all = true;
            for side in &rule.sides {
                let hits: Vec<Duty> = duties
                    .iter()
                    .filter(|d| side.names_duty(d))
                    .cloned()
                    .collect();
                if hits.is_empty() {
                    all = false;
                    break;
                }
                sides_ok.push(hits);
            }
            if all {
                out.push(InternalConflict {
                    rule_id: rule.id.clone(),
                    severity: rule.severity,
                    role: role.clone(),
                    first_side: std::mem::take(&mut sides_ok[0]),
                    second_side: std::mem::take(&mut sides_ok[1]),
                });
            }
        }
    }
    out.sort();
    out
}

/// The per-user duty holdings after expansion, for diagnostics and the property tests:
/// (duty, scope) pairs, sorted and deduplicated.
#[must_use]
pub fn holdings_of(
    assignments: &[Assignment],
    user: &str,
    catalogue: &RoleCatalogue,
) -> Vec<(Duty, Option<String>)> {
    let mut duties: Vec<(Duty, Option<String>)> = Holdings::of(assignments, user, catalogue)
        .duties
        .into_iter()
        .map(|h| (h.duty, h.scope))
        .collect();
    duties.sort();
    duties.dedup();
    duties
}
