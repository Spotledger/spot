//! Access change events feed the model (WP-IAM-08.T15).
//!
//! A role, an assignment or a user's type is a stored document, so its change already leaves the
//! database as a published outbox event (`doc_created`, `doc_updated`, `doc_deleted` of the
//! entities `role`, `role_assignment` and `user`), after the transaction commits, on the channel
//! every consumer of the site reads. This module turns those records into [`AccessChange`]s (ids
//! and the kind of change, nothing else), tells the model which stored records to read again, and
//! writes one [`ChangeEvent`] per change to the access-change journal (ids, role and user names of
//! the cached directory, no snapshot and no reason text).
//!
//! # Applying a change
//!
//! [`Touched`] is the set of ids a batch names. The live source re-reads only those records'
//! child rows (a role's duties, an assignment's scope) and keeps the rest of its cached directory,
//! then assembles again. A change it cannot place (an unknown id) is not guessed at: the next
//! question re-reads everything.
//!
//! # Gaps
//!
//! A feed can miss changes: another node held the lease, the node was down, the stream restarted
//! (a new generation), a read failed. The consumer never counts positions to find out; it asks for
//! a full rebuild on every event that says its view is broken, and the source rebuilds from the
//! database on a timer anyway ([`live::DEFAULT_REPAIR_SECONDS`]), so a missed event is
//! repaired within that interval.

use std::collections::BTreeSet;

use spot_iam_governance::{ChangeActor, ChangeEvent, ChangeOp};
use spot_iam_roles::Source;
use spot_storage_contract::OutboxRecord;

use super::assemble::{RawAssignment, RawDirectory, RawRole};

/// The outbox consumer name (the lease and the position belong to it).
pub const CONSUMER: &str = "access_questions";

/// The component named as the actor of an event this consumer journals.
pub const COMPONENT: &str = "access_questions";

/// The entity of a stored role.
pub const ENTITY_ROLE: &str = "role";
/// The entity of a stored assignment.
pub const ENTITY_ASSIGNMENT: &str = "role_assignment";
/// The entity of a stored user (the portal mark lives on its type).
pub const ENTITY_USER: &str = "user";

/// What kind of record changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Entity {
    /// A `role` document.
    Role,
    /// A `role_assignment` document.
    Assignment,
    /// A `user` document.
    User,
}

/// What happened to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// Created.
    Created,
    /// Updated.
    Updated,
    /// Deleted.
    Deleted,
}

/// One change after commit: the kind of record, what happened, and the record's id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccessChange {
    /// The kind of record.
    pub entity: Entity,
    /// What happened.
    pub op: Op,
    /// The record id (text).
    pub id: String,
}

/// The access change an outbox record carries, or `None` when it is about anything else.
#[must_use]
pub fn classify(record: &OutboxRecord) -> Option<AccessChange> {
    let event = &record.event;
    let op = match event.topic.as_str() {
        "doc_created" => Op::Created,
        "doc_updated" => Op::Updated,
        "doc_deleted" => Op::Deleted,
        _ => return None,
    };
    let entity = match event.subject.entity.as_str() {
        ENTITY_ROLE => Entity::Role,
        ENTITY_ASSIGNMENT => Entity::Assignment,
        ENTITY_USER => Entity::User,
        _ => return None,
    };
    Some(AccessChange {
        entity,
        op,
        id: event.subject.id.to_string(),
    })
}

/// The ids a batch of changes names, by kind.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Touched {
    /// Role ids.
    pub roles: BTreeSet<String>,
    /// Assignment ids.
    pub assignments: BTreeSet<String>,
    /// User ids.
    pub users: BTreeSet<String>,
}

impl Touched {
    /// Adds the record `change` names.
    pub fn add(&mut self, change: &AccessChange) {
        let set = match change.entity {
            Entity::Role => &mut self.roles,
            Entity::Assignment => &mut self.assignments,
            Entity::User => &mut self.users,
        };
        set.insert(change.id.clone());
    }

    /// Adds everything `other` names.
    pub fn merge(&mut self, other: Touched) {
        self.roles.extend(other.roles);
        self.assignments.extend(other.assignments);
        self.users.extend(other.users);
    }

    /// Whether nothing is named.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.roles.is_empty() && self.assignments.is_empty() && self.users.is_empty()
    }

    /// How many ids are named.
    #[must_use]
    pub fn len(&self) -> usize {
        self.roles.len() + self.assignments.len() + self.users.len()
    }
}

fn source_of(text: &str) -> Source {
    match text {
        "rule" => Source::Rule,
        "idp" => Source::Idp,
        "emergency" => Source::Emergency,
        _ => Source::Manual,
    }
}

fn origin_of(role: &RawRole) -> &str {
    match role.origin.as_deref() {
        Some("reference") => "reference",
        Some("custom") => "custom",
        _ if role.is_system => "reference",
        _ => "custom",
    }
}

fn find_role<'a>(id: &str, dirs: [&'a RawDirectory; 2]) -> Option<&'a RawRole> {
    dirs.iter()
        .find_map(|d| d.roles.iter().find(|r| r.id == id))
}

fn find_assignment<'a>(id: &str, dirs: [&'a RawDirectory; 2]) -> Option<&'a RawAssignment> {
    dirs.iter()
        .find_map(|d| d.assignments.iter().find(|a| a.id == id))
}

/// The change events of `changes`, from the directory before and after the refresh: the role's
/// name and origin, the assignment's user, role name and source. A user's type change is not an
/// access change event (it only moves the portal mark, which the model re-reads). A record that
/// neither directory holds (created and deleted between two reads) is skipped.
///
/// Only ids, names and tokens are in an event: no snapshot, no scope, no reason.
#[must_use]
pub fn change_events(
    before: &RawDirectory,
    after: &RawDirectory,
    changes: &[AccessChange],
    at: i64,
) -> Vec<ChangeEvent> {
    let actor = || ChangeActor::system(COMPONENT);
    let mut out = Vec::new();
    for change in changes {
        match change.entity {
            Entity::User => {}
            Entity::Role => {
                // A deleted role is only in the directory before; the others are in the one after.
                let order = if change.op == Op::Deleted {
                    [before, after]
                } else {
                    [after, before]
                };
                let Some(role) = find_role(&change.id, order) else {
                    continue;
                };
                let op = match change.op {
                    Op::Created => ChangeOp::Created,
                    Op::Updated => ChangeOp::Updated,
                    Op::Deleted => ChangeOp::Deleted,
                };
                out.push(ChangeEvent::role(
                    &role.name,
                    op,
                    origin_of(role),
                    actor(),
                    at,
                ));
            }
            Entity::Assignment => {
                let order = if change.op == Op::Deleted {
                    [before, after]
                } else {
                    [after, before]
                };
                let Some(assignment) = find_assignment(&change.id, order) else {
                    continue;
                };
                let role_name = find_role(&assignment.role_id, [after, before])
                    .map_or(assignment.role_id.as_str(), |r| r.name.as_str());
                let ended = matches!(assignment.state.as_str(), "ended" | "expired");
                let (op, cause) = match change.op {
                    Op::Created => (ChangeOp::Created, None),
                    Op::Deleted => (ChangeOp::Deleted, None),
                    Op::Updated if ended => (
                        ChangeOp::Ended,
                        Some(if assignment.state == "expired" {
                            "expired"
                        } else {
                            "removed"
                        }),
                    ),
                    Op::Updated => (ChangeOp::Updated, None),
                };
                let mut event = ChangeEvent::grant(
                    &assignment.id,
                    &assignment.user,
                    role_name,
                    source_of(&assignment.source),
                    op,
                    actor(),
                    at,
                );
                if let Some(cause) = cause {
                    event = event.with_cause(cause);
                }
                out.push(event);
            }
        }
    }
    out
}
