//! The permission-change audit (ADR-070 section 8, rule 4): one [`ChangeEvent`] per role, duty
//! or grant change, with canonical before/after snapshots, the actor (and the `via` of an
//! impersonated action) and a reason.
//!
//! A change event is emitted through the `identity` topic of the events contract:
//! [`ChangeEvent::topic`] names the topic and [`ChangeEvent::to_identity_data`] builds the
//! `data` payload, which validates against that topic's schema. The actor travels in the
//! event metadata; [`ChangeEvent::actor_json`] builds the contract's `Actor` shape, keeping
//! both identities when one user acts as another (WP-IAM-01).

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use spot_iam_roles::Source;

use crate::error::GovError;

/// A canonical snapshot of a record: its JSON, object keys sorted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Snapshot(Value);

impl Snapshot {
    /// The canonical snapshot of any serialisable value.
    ///
    /// # Errors
    ///
    /// [`GovError::SnapshotFailed`] when the value cannot be represented as JSON.
    pub fn of<T: Serialize>(value: &T) -> Result<Snapshot, GovError> {
        serde_json::to_value(value)
            .map(Snapshot)
            .map_err(|e| GovError::SnapshotFailed {
                detail: e.to_string(),
            })
    }

    /// A snapshot of an already-built JSON value.
    #[must_use]
    pub fn from_value(value: Value) -> Snapshot {
        Snapshot(value)
    }

    /// The canonical JSON text (object keys sorted, no whitespace).
    #[must_use]
    pub fn canonical(&self) -> String {
        serde_json::to_string(&self.0).unwrap_or_default()
    }

    /// The JSON value.
    #[must_use]
    pub fn value(&self) -> &Value {
        &self.0
    }
}

/// Which kind of record changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    /// A role.
    Role,
    /// A duty.
    Duty,
    /// A grant (an assignment).
    Grant,
}

/// What happened to the record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeOp {
    /// The record was created.
    Created,
    /// The record was updated.
    Updated,
    /// The record was deleted.
    Deleted,
    /// The record was copied (a custom role from a reference one).
    Copied,
    /// The record was upgraded.
    Upgraded,
    /// A grant ended.
    Ended,
}

impl ChangeOp {
    /// The token the events contract uses for this operation.
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            ChangeOp::Created => "created",
            ChangeOp::Updated => "updated",
            ChangeOp::Deleted => "deleted",
            ChangeOp::Copied => "copied",
            ChangeOp::Upgraded => "upgraded",
            ChangeOp::Ended => "ended",
        }
    }
}

/// Who caused the change: a user (with the real person when impersonating) or the platform.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeActor {
    /// A signed-in user.
    User {
        /// The user whose rights applied.
        id: String,
        /// The real person, when they acted as `id` (impersonation, ADR-070 section 9).
        #[serde(default)]
        via: Option<String>,
    },
    /// The platform itself: a job, a migration, sync apply.
    System {
        /// Which component.
        component: String,
    },
}

impl ChangeActor {
    /// A user actor with no impersonation.
    #[must_use]
    pub fn user(id: &str) -> ChangeActor {
        ChangeActor::User {
            id: id.to_owned(),
            via: None,
        }
    }

    /// A user actor impersonating on behalf of `via`.
    #[must_use]
    pub fn impersonated(id: &str, via: &str) -> ChangeActor {
        ChangeActor::User {
            id: id.to_owned(),
            via: Some(via.to_owned()),
        }
    }

    /// A system actor.
    #[must_use]
    pub fn system(component: &str) -> ChangeActor {
        ChangeActor::System {
            component: component.to_owned(),
        }
    }

    /// The events contract's `Actor` JSON: both identities survive an impersonation.
    #[must_use]
    pub fn actor_json(&self) -> Value {
        match self {
            ChangeActor::User { id, via } => {
                let mut map = Map::new();
                map.insert("kind".to_owned(), json!("user"));
                map.insert("id".to_owned(), json!(id));
                if let Some(via) = via {
                    map.insert("via".to_owned(), json!(via));
                }
                Value::Object(map)
            }
            ChangeActor::System { component } => json!({
                "kind": "system",
                "component": component,
            }),
        }
    }
}

/// One change to access: a role, a duty or a grant, with its before and after snapshots.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct ChangeEvent {
    /// Which kind of record changed.
    pub kind: ChangeKind,
    /// What happened.
    pub op: ChangeOp,
    /// The record's name (a role or duty name, or an assignment id).
    pub subject: String,
    /// Who caused it.
    pub actor: ChangeActor,
    /// Why, when a reason was given.
    #[serde(default)]
    pub reason: Option<String>,
    /// The record before the change.
    #[serde(default)]
    pub before: Option<Snapshot>,
    /// The record after the change.
    #[serde(default)]
    pub after: Option<Snapshot>,
    /// When it happened, in integer epoch seconds; the caller stamps it.
    pub occurred_at: i64,
    /// The role/duty origin (`reference` or `custom`); required for role and duty changes.
    #[serde(default)]
    pub origin: Option<String>,
    /// The role/duty version, when it has one.
    #[serde(default)]
    pub version: Option<String>,
    /// The reference role a custom role was copied from, when it was.
    #[serde(default)]
    pub copied_from: Option<String>,
    /// The grant's user.
    #[serde(default)]
    pub user: Option<String>,
    /// The grant's role.
    #[serde(default)]
    pub role: Option<String>,
    /// The grant's source.
    #[serde(default)]
    pub source: Option<Source>,
    /// The grant's scope, as JSON.
    #[serde(default)]
    pub scope: Option<Snapshot>,
    /// Why a grant ended (`removed`, `expired`, `rule_ended`, `idp_removed`, `user_disabled`).
    #[serde(default)]
    pub cause: Option<String>,
}

impl ChangeEvent {
    /// A role change.
    #[must_use]
    pub fn role(
        name: &str,
        op: ChangeOp,
        origin: &str,
        actor: ChangeActor,
        occurred_at: i64,
    ) -> ChangeEvent {
        ChangeEvent {
            kind: ChangeKind::Role,
            op,
            subject: name.to_owned(),
            actor,
            reason: None,
            before: None,
            after: None,
            occurred_at,
            origin: Some(origin.to_owned()),
            version: None,
            copied_from: None,
            user: None,
            role: None,
            source: None,
            scope: None,
            cause: None,
        }
    }

    /// A duty change.
    #[must_use]
    pub fn duty(
        name: &str,
        op: ChangeOp,
        origin: &str,
        actor: ChangeActor,
        occurred_at: i64,
    ) -> ChangeEvent {
        ChangeEvent {
            kind: ChangeKind::Duty,
            ..ChangeEvent::role(name, op, origin, actor, occurred_at)
        }
    }

    /// A grant (assignment) change.
    #[must_use]
    pub fn grant(
        assignment: &str,
        user: &str,
        role: &str,
        source: Source,
        op: ChangeOp,
        actor: ChangeActor,
        occurred_at: i64,
    ) -> ChangeEvent {
        ChangeEvent {
            kind: ChangeKind::Grant,
            op,
            subject: assignment.to_owned(),
            actor,
            reason: None,
            before: None,
            after: None,
            occurred_at,
            origin: None,
            version: None,
            copied_from: None,
            user: Some(user.to_owned()),
            role: Some(role.to_owned()),
            source: Some(source),
            scope: None,
            cause: None,
        }
    }

    /// Sets the before snapshot.
    #[must_use]
    pub fn with_before(mut self, before: Snapshot) -> ChangeEvent {
        self.before = Some(before);
        self
    }

    /// Sets the after snapshot.
    #[must_use]
    pub fn with_after(mut self, after: Snapshot) -> ChangeEvent {
        self.after = Some(after);
        self
    }

    /// Sets the reason.
    #[must_use]
    pub fn with_reason(mut self, reason: &str) -> ChangeEvent {
        self.reason = Some(reason.to_owned());
        self
    }

    /// Sets the version.
    #[must_use]
    pub fn with_version(mut self, version: &str) -> ChangeEvent {
        self.version = Some(version.to_owned());
        self
    }

    /// Sets the copied-from reference.
    #[must_use]
    pub fn with_copied_from(mut self, copied_from: &str) -> ChangeEvent {
        self.copied_from = Some(copied_from.to_owned());
        self
    }

    /// Sets the grant's scope snapshot.
    #[must_use]
    pub fn with_scope(mut self, scope: Snapshot) -> ChangeEvent {
        self.scope = Some(scope);
        self
    }

    /// Sets why a grant ended.
    #[must_use]
    pub fn with_cause(mut self, cause: &str) -> ChangeEvent {
        self.cause = Some(cause.to_owned());
        self
    }

    /// The identity topic this change is emitted through.
    #[must_use]
    pub fn topic(&self) -> &'static str {
        match self.kind {
            ChangeKind::Role => "identity_role_changed",
            ChangeKind::Duty => "identity_duty_changed",
            ChangeKind::Grant => match self.op {
                ChangeOp::Created => "identity_assignment_created",
                ChangeOp::Ended | ChangeOp::Deleted => "identity_assignment_ended",
                _ => "identity_assignment_changed",
            },
        }
    }

    /// The topic's `data` payload, valid against that topic's schema.
    ///
    /// # Errors
    ///
    /// [`GovError::SnapshotFailed`] when a snapshot cannot be turned back into JSON.
    pub fn to_identity_data(&self) -> Result<Value, GovError> {
        match self.kind {
            ChangeKind::Role | ChangeKind::Duty => self.role_or_duty_data(),
            ChangeKind::Grant => self.grant_data(),
        }
    }

    /// The `identity_role_changed` / `identity_duty_changed` payload.
    fn role_or_duty_data(&self) -> Result<Value, GovError> {
        let mut map = Map::new();
        map.insert("name".to_owned(), json!(self.subject));
        map.insert("change".to_owned(), json!(self.op.token()));
        map.insert(
            "origin".to_owned(),
            json!(self.origin.as_deref().unwrap_or("custom")),
        );
        if let Some(version) = &self.version {
            map.insert("version".to_owned(), json!(version));
        }
        if let Some(copied_from) = &self.copied_from {
            map.insert("copied_from".to_owned(), json!(copied_from));
        }
        if let Some(before) = &self.before {
            map.insert("before".to_owned(), before.value().clone());
        }
        if let Some(after) = &self.after {
            map.insert("after".to_owned(), after.value().clone());
        }
        Ok(Value::Object(map))
    }

    /// The `identity_assignment_*` payload.
    fn grant_data(&self) -> Result<Value, GovError> {
        let mut map = Map::new();
        map.insert("assignment".to_owned(), json!(self.subject));
        map.insert(
            "user".to_owned(),
            json!(self.user.clone().unwrap_or_default()),
        );
        map.insert(
            "role".to_owned(),
            json!(self.role.clone().unwrap_or_default()),
        );
        match self.op {
            ChangeOp::Created => {
                map.insert(
                    "source".to_owned(),
                    json!(source_token(self.source.unwrap_or(Source::Manual))),
                );
                map.insert(
                    "scope".to_owned(),
                    self.scope
                        .as_ref()
                        .map(|s| s.value().clone())
                        .unwrap_or_else(|| json!([])),
                );
                if let Some(reason) = &self.reason {
                    map.insert("reason".to_owned(), json!(reason));
                }
            }
            ChangeOp::Ended | ChangeOp::Deleted => {
                map.insert(
                    "source".to_owned(),
                    json!(source_token(self.source.unwrap_or(Source::Manual))),
                );
                map.insert(
                    "cause".to_owned(),
                    json!(self.cause.as_deref().unwrap_or("removed")),
                );
            }
            _ => {
                map.insert(
                    "before".to_owned(),
                    self.before
                        .as_ref()
                        .map(|s| s.value().clone())
                        .unwrap_or_else(|| json!({})),
                );
                map.insert(
                    "after".to_owned(),
                    self.after
                        .as_ref()
                        .map(|s| s.value().clone())
                        .unwrap_or_else(|| json!({})),
                );
                if let Some(reason) = &self.reason {
                    map.insert("reason".to_owned(), json!(reason));
                }
            }
        }
        Ok(Value::Object(map))
    }
}

/// The events contract's assignment-source token.
fn source_token(source: Source) -> &'static str {
    match source {
        Source::Manual => "manual",
        Source::Rule => "rule",
        Source::Idp => "idp",
        Source::Emergency => "emergency",
    }
}

/// What changed in one user's access between two instants.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct AccessDiff {
    /// The user the diff is about.
    pub user: String,
    /// The window's start (inclusive), in integer epoch seconds.
    pub from: i64,
    /// The window's end (inclusive), in integer epoch seconds.
    pub to: i64,
    /// Grants created in the window.
    pub granted: Vec<ChangeEvent>,
    /// Grants removed in the window.
    pub removed: Vec<ChangeEvent>,
    /// Grants changed in the window.
    pub changed: Vec<ChangeEvent>,
}

impl AccessDiff {
    /// Whether nothing changed in the window.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.granted.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }
}

/// The answer to "what changed in this user's access between two dates": the grant changes in
/// `[from, to]`, split into what was granted, removed and changed.
///
/// Deterministic: the three lists are sorted by (occurred_at, subject).
#[must_use]
pub fn diff_access(events: &[ChangeEvent], user: &str, from: i64, to: i64) -> AccessDiff {
    let mut granted = Vec::new();
    let mut removed = Vec::new();
    let mut changed = Vec::new();
    for event in events {
        if event.kind != ChangeKind::Grant {
            continue;
        }
        if event.user.as_deref() != Some(user) {
            continue;
        }
        if event.occurred_at < from || event.occurred_at > to {
            continue;
        }
        match event.op {
            ChangeOp::Created => granted.push(event.clone()),
            ChangeOp::Ended | ChangeOp::Deleted => removed.push(event.clone()),
            ChangeOp::Updated | ChangeOp::Copied | ChangeOp::Upgraded => {
                changed.push(event.clone())
            }
        }
    }
    for list in [&mut granted, &mut removed, &mut changed] {
        list.sort_by(|a, b| {
            a.occurred_at
                .cmp(&b.occurred_at)
                .then_with(|| a.subject.cmp(&b.subject))
        });
    }
    AccessDiff {
        user: user.to_owned(),
        from,
        to,
        granted,
        removed,
        changed,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn a_role_change_payload_names_its_origin_and_snapshots() {
        let event = ChangeEvent::role(
            "Finance Approver",
            ChangeOp::Updated,
            "reference",
            ChangeActor::user("ana"),
            10,
        )
        .with_before(Snapshot::from_value(json!({"duties": ["a"]})))
        .with_after(Snapshot::from_value(json!({"duties": ["a", "b"]})))
        .with_reason("added a duty");
        let data = event.to_identity_data().expect("data");
        assert_eq!(data["name"], "Finance Approver");
        assert_eq!(data["change"], "updated");
        assert_eq!(data["origin"], "reference");
        assert_eq!(data["after"]["duties"][1], "b");
    }

    #[test]
    fn an_impersonated_actor_keeps_both_identities() {
        let actor = ChangeActor::impersonated("support-1", "ana");
        let json = actor.actor_json();
        assert_eq!(json["kind"], "user");
        assert_eq!(json["id"], "support-1");
        assert_eq!(json["via"], "ana");
    }

    #[test]
    fn a_snapshot_is_canonical_whatever_the_key_order() {
        let a = Snapshot::from_value(json!({"b": 1, "a": 2}));
        let b = Snapshot::from_value(json!({"a": 2, "b": 1}));
        assert_eq!(a, b);
        assert_eq!(a.canonical(), r#"{"a":2,"b":1}"#);
    }

    #[test]
    fn a_diff_splits_grants_by_operation_and_window() {
        let events = vec![
            ChangeEvent::grant(
                "grant:1",
                "ben",
                "Finance Approver",
                Source::Manual,
                ChangeOp::Created,
                ChangeActor::user("ana"),
                10,
            ),
            ChangeEvent::grant(
                "grant:2",
                "ben",
                "Accounts Payable Clerk",
                Source::Manual,
                ChangeOp::Ended,
                ChangeActor::user("ana"),
                20,
            ),
            ChangeEvent::grant(
                "grant:3",
                "cara",
                "Accounts Payable Clerk",
                Source::Manual,
                ChangeOp::Created,
                ChangeActor::user("ana"),
                15,
            ),
        ];
        let diff = diff_access(&events, "ben", 0, 100);
        assert_eq!(diff.granted.len(), 1);
        assert_eq!(diff.removed.len(), 1);
        assert!(diff.changed.is_empty());
        assert!(diff_access(&events, "ben", 0, 5).is_empty());
    }
}
