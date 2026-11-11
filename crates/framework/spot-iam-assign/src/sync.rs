//! The three entry points: [`plan_rules`], [`plan_idp`] and [`expire`].

use std::collections::{BTreeMap, BTreeSet};

use spot_iam_roles::{Assignment, OutsideGuard, Source, expiry_notices};
use time::{Date, OffsetDateTime, Time};

use crate::facts::{Facts, UserId};
use crate::idp::{Group, GroupRoleMap};
use crate::ids::IdSource;
use crate::plan::{EndCause, EventPayload, Plan, UserError};
use crate::reconcile::{Desired, Job, reconcile};
use crate::rules::RuleSet;

/// Plans the rule-sourced assignments of `users`.
///
/// For each user and enabled rule whose condition holds, an assignment with `source = Rule`
/// and `reason = "rule:<id>"` and the evaluated scope exists afterwards. A `Rule` assignment
/// whose rule no longer holds, was deleted, disabled, or now gives another role is ended; one
/// whose scope changed is replaced in place. Idempotent: planning again from the applied
/// result changes nothing.
///
/// A user for whom any rule cannot be evaluated is reported in [`Plan::errors`] and **left
/// alone**: nothing is granted, ended or replaced for them, because a data glitch must not
/// strip access. Users not listed in `users` are not touched either.
///
/// Every new assignment takes its id from `ids`, the platform's record id source, so a grant
/// carries the id the persistence layer stores and the plan stays deterministic in tests.
///
/// **Fail-open for portal users.** This variant does not know who is a portal user, so a rule
/// can give one a staff role (ADR-083 Barrier 3). Use [`plan_rules_guarded`].
#[deprecated(
    note = "fail-open for portal users: it cannot refuse a staff role for an outside user; use plan_rules_guarded (ADR-083 Barrier 3)"
)]
pub fn plan_rules(
    rule_set: &RuleSet,
    ids: &mut dyn IdSource,
    current: &[Assignment],
    users: &[(UserId, Facts)],
    now: OffsetDateTime,
) -> Plan {
    plan_rules_guarded(rule_set, ids, current, users, now, &OutsideGuard::none())
}

/// [`plan_rules`] with the portal rule (ADR-083 Barrier 3): when a rule would give a portal
/// user a role `guard` refuses, nothing is granted for that rule and a [`UserError`] names the
/// rule and why (the role and the duty, never the user's data); an assignment of that role the
/// user already has from this source ends, because the desired set no longer contains it.
///
/// A user whose rules cannot be evaluated is left alone, as in [`plan_rules`]; the snapshot
/// build (`build_snapshot_input_guarded`) is the backstop for what such a user already holds.
pub fn plan_rules_guarded(
    rule_set: &RuleSet,
    ids: &mut dyn IdSource,
    current: &[Assignment],
    users: &[(UserId, Facts)],
    now: OffsetDateTime,
    guard: &OutsideGuard,
) -> Plan {
    let mut errors = Vec::new();
    let mut reconciled = Vec::new();
    let mut seen = BTreeSet::new();
    for (user, facts) in users {
        if !seen.insert(user.clone()) {
            continue;
        }
        let mut desired = Vec::new();
        let mut failed = false;
        for rule in rule_set.rules().iter().filter(|r| r.enabled) {
            match rule.holds(facts, now) {
                Err(message) => {
                    failed = true;
                    errors.push(UserError {
                        user: user.clone(),
                        subject: rule.reason(),
                        message,
                    });
                }
                Ok(false) => {}
                Ok(true) => match rule.scope_for(user, facts, now) {
                    Err(message) => {
                        failed = true;
                        errors.push(UserError {
                            user: user.clone(),
                            subject: rule.reason(),
                            message,
                        });
                    }
                    Ok(restrictions) => match guard.check(user, &rule.role) {
                        Err(refusal) => errors.push(UserError {
                            user: user.clone(),
                            subject: rule.reason(),
                            message: refusal.to_string(),
                        }),
                        Ok(()) => desired.push(Desired {
                            role: rule.role.clone(),
                            reason: rule.reason(),
                            scope: spot_iam_roles::Scope { restrictions },
                            valid_for: rule.valid_for,
                        }),
                    },
                },
            }
        }
        if !failed {
            reconciled.push((user.clone(), desired));
        }
    }
    let why = |a: &Assignment| -> String {
        let id = a
            .reason
            .as_deref()
            .and_then(|r| r.strip_prefix("rule:"))
            .unwrap_or_default();
        match rule_set.find(id) {
            None => format!("rule `{id}` no longer exists"),
            Some(r) if !r.enabled => format!("rule `{id}` is disabled"),
            Some(r) if r.role != a.role => format!("rule `{id}` now gives another role"),
            Some(_) => format!("the condition of rule `{id}` no longer holds"),
        }
    };
    let job = Job {
        source: Source::Rule,
        prefix: "rule:",
        cause: EndCause::RuleEnded,
        end_why: &why,
    };
    reconcile(&job, ids, reconciled, current, errors, now)
}

/// Plans the idp-sourced assignments from the IdP's group lists.
///
/// A user present in `user_groups` has exactly the roles their groups map to. A user **absent**
/// from it keeps their idp assignments unless `complete` says the listing covered everyone,
/// in which case they have no groups and their idp assignments end. Groups the table does not
/// know are ignored. New assignments take their id from `ids`.
///
/// **Fail-open for portal users.** This variant does not know who is a portal user, so a
/// group can give one a staff role (ADR-083 Barrier 3). Use [`plan_idp_guarded`]. It is meant
/// to carry `#[deprecated]` like [`plan_rules`]; the attribute waits for a follow-up because
/// `spot-iam-scim`'s `request.rs` still imports it and is outside this task's owned paths.
pub fn plan_idp(
    mapping: &GroupRoleMap,
    ids: &mut dyn IdSource,
    user_groups: &[(UserId, Vec<Group>)],
    complete: bool,
    current: &[Assignment],
    now: OffsetDateTime,
) -> Plan {
    plan_idp_guarded(
        mapping,
        ids,
        user_groups,
        complete,
        current,
        now,
        &OutsideGuard::none(),
    )
}

/// [`plan_idp`] with the portal rule (ADR-083 Barrier 3): a mapping row whose role `guard`
/// refuses for a portal user gives that user nothing and is reported in [`Plan::errors`]
/// (naming the group, the role and the duty, never the user's data); an idp assignment of
/// that role the user already has ends.
pub fn plan_idp_guarded(
    mapping: &GroupRoleMap,
    ids: &mut dyn IdSource,
    user_groups: &[(UserId, Vec<Group>)],
    complete: bool,
    current: &[Assignment],
    now: OffsetDateTime,
    guard: &OutsideGuard,
) -> Plan {
    let mut errors: Vec<UserError> = Vec::new();
    let mut groups_of: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for (user, groups) in user_groups {
        groups_of
            .entry(user.as_str())
            .or_default()
            .extend(groups.iter().map(String::as_str));
    }
    if complete {
        for a in current {
            if a.source == Source::Idp && a.reason.as_deref().is_some_and(|r| r.starts_with("idp:"))
            {
                groups_of.entry(a.user.as_str()).or_default();
            }
        }
    }
    let mut reconciled: Vec<(String, Vec<Desired>)> = Vec::new();
    for (user, groups) in groups_of {
        let uid = user.to_owned();
        let mut desired = Vec::new();
        for row in mapping
            .rows()
            .iter()
            .filter(|row| groups.contains(row.group.as_str()))
        {
            let reason = format!("idp:{}", row.group);
            if let Err(refusal) = guard.check(&uid, &row.role) {
                errors.push(UserError {
                    user: uid.clone(),
                    subject: reason,
                    message: refusal.to_string(),
                });
                continue;
            }
            desired.push(Desired {
                role: row.role.clone(),
                reason,
                scope: GroupRoleMap::scope_for(row, &uid),
                valid_for: None,
            });
        }
        reconciled.push((uid, desired));
    }
    let why = |a: &Assignment| -> String {
        let group = a
            .reason
            .as_deref()
            .and_then(|r| r.strip_prefix("idp:"))
            .unwrap_or_default();
        format!("the identity provider no longer maps the user's groups to `{group}`")
    };
    let job = Job {
        source: Source::Idp,
        prefix: "idp:",
        cause: EndCause::IdpRemoved,
        end_why: &why,
    };
    reconcile(&job, ids, reconciled, current, errors, now)
}

/// The expiry events due at `now`: one `identity_assignment_ended` (cause `expired`) for each
/// assignment whose `valid_to` has passed, of any source, **exactly once**.
///
/// `announced` is the marker the caller stored from earlier plans ([`Plan::ended_marker`], and
/// the `ended_marker` of earlier calls to this function). The new ids to store are in this
/// plan's `ended_marker`. No hidden state: call again with the grown set and nothing repeats.
pub fn expire(current: &[Assignment], announced: &BTreeSet<String>, now: OffsetDateTime) -> Plan {
    let beginning = OffsetDateTime::new_utc(Date::MIN, Time::MIDNIGHT);
    let mut events = Vec::new();
    let mut marker = Vec::new();
    for notice in expiry_notices(current, beginning, now) {
        if announced.contains(&notice.assignment) {
            continue;
        }
        let source = current
            .iter()
            .find(|a| a.id == notice.assignment)
            .map_or(Source::Manual, |a| a.source);
        marker.push(notice.assignment.clone());
        events.push(EventPayload::Ended {
            assignment: notice.assignment,
            user: notice.user,
            role: notice.role,
            source,
            cause: EndCause::Expired,
        });
    }
    marker.sort();
    Plan {
        changes: Vec::new(),
        events,
        errors: Vec::new(),
        ended_marker: marker,
    }
}
