//! The desired-state sync shared by the rule and idp sources.
//!
//! Given, for a set of users, everything their source wants them to have, it makes the live
//! assignments of that source match: grants what is missing, replaces what has the wrong scope
//! (in place, so no gap), ends what is no longer wanted. It looks only at assignments of its
//! own source and reason prefix, never at another source's.

use std::collections::BTreeSet;

use spot_iam_roles::{Assignment, RoleName, Scope, Source};
use time::{Duration, OffsetDateTime};

use crate::ids::IdSource;
use crate::plan::{Change, EndCause, Plan, UserError};

/// One assignment a source wants a user to have.
#[derive(Clone, Debug)]
pub(crate) struct Desired {
    pub(crate) role: RoleName,
    /// `rule:<id>` or `idp:<group>`: also the key that matches it to what exists.
    pub(crate) reason: String,
    pub(crate) scope: Scope,
    pub(crate) valid_for: Option<Duration>,
}

pub(crate) struct Job<'a> {
    pub(crate) source: Source,
    /// Only assignments whose reason starts with this are this reconciler's.
    pub(crate) prefix: &'static str,
    pub(crate) cause: EndCause,
    /// The sentence recorded when an assignment ends.
    pub(crate) end_why: &'a dyn Fn(&Assignment) -> String,
}

fn live(a: &Assignment, now: OffsetDateTime) -> bool {
    a.valid_to.is_none_or(|t| t > now)
}

/// `users` pairs each reconciled user with their complete desired set. A user not listed is
/// not touched. New grants take their id from `ids`.
pub(crate) fn reconcile(
    job: &Job<'_>,
    ids: &mut dyn IdSource,
    mut users: Vec<(String, Vec<Desired>)>,
    current: &[Assignment],
    errors: Vec<UserError>,
    now: OffsetDateTime,
) -> Plan {
    users.sort_by(|a, b| a.0.cmp(&b.0));
    users.dedup_by(|a, b| a.0 == b.0);
    let mut changes = Vec::new();
    let mut plan_errors = errors;

    for (user, mut desired) in users {
        desired.sort_by(|a, b| (&a.role, &a.reason).cmp(&(&b.role, &b.reason)));
        let mut mine: Vec<&Assignment> = current
            .iter()
            .filter(|a| {
                a.user == user
                    && a.source == job.source
                    && a.reason
                        .as_deref()
                        .is_some_and(|r| r.starts_with(job.prefix))
            })
            .collect();
        mine.sort_by(|a, b| a.id.cmp(&b.id));
        let mut matched: BTreeSet<&str> = BTreeSet::new();

        for d in &desired {
            let found = mine.iter().find(|a| {
                live(a, now)
                    && a.role == d.role
                    && a.reason.as_deref() == Some(d.reason.as_str())
                    && !matched.contains(a.id.as_str())
            });
            if let Some(existing) = found {
                matched.insert(existing.id.as_str());
                if existing.scope != d.scope {
                    let mut with = (*existing).clone();
                    with.scope = d.scope.clone();
                    changes.push(Change::Replace {
                        id: existing.id.clone(),
                        with,
                    });
                }
                continue;
            }
            // A time-boxed grant that has run its course is not given again.
            let spent = d.valid_for.is_some_and(|span| {
                mine.iter().any(|a| {
                    a.role == d.role
                        && a.reason.as_deref() == Some(d.reason.as_str())
                        && a.valid_from
                            .is_some_and(|from| a.valid_to == from.checked_add(span))
                        && !live(a, now)
                })
            });
            if spent {
                continue;
            }
            let (valid_from, valid_to) = match d.valid_for {
                Some(span) => match now.checked_add(span) {
                    Some(end) => (Some(now), Some(end)),
                    None => {
                        plan_errors.push(UserError {
                            user: user.clone(),
                            subject: d.reason.clone(),
                            message: "the time box runs past the last supported date".to_owned(),
                        });
                        continue;
                    }
                },
                None => (None, None),
            };
            let id = ids.next();
            changes.push(Change::Grant(Assignment {
                id,
                user: user.clone(),
                role: d.role.clone(),
                scope: d.scope.clone(),
                valid_from,
                valid_to,
                source: job.source,
                reason: Some(d.reason.clone()),
            }));
        }

        for a in mine {
            if live(a, now) && !matched.contains(a.id.as_str()) {
                changes.push(Change::End {
                    id: a.id.clone(),
                    user: a.user.clone(),
                    role: a.role.clone(),
                    at: now,
                    reason: (job.end_why)(a),
                });
            }
        }
    }
    Plan::finish(changes, current, job.cause, plan_errors)
}
