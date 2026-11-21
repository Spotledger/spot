//! The unit of work SCIM runs on (WP-IAM-06.T2): the core's synchronous [`ScimStore`] over one
//! tenant's directory loaded into memory for one request. `services/scim.rs` loads it from storage
//! (`scim_store.rs`), the core mutates it, and the difference from the snapshot is written back in
//! the same transaction. Every method answers only for the unit's own tenant (fail closed).
//!
//! Stored roles carry no portal kind, so every role record here is a desk role and a portal user
//! is refused it. Sessions, refresh tokens and invitations are ended by the sign-in path itself
//! (it re-reads the user's state on every request), so their counts are reported as zero; the
//! user's API keys are counted from the snapshot and revoked on persist.

use std::collections::BTreeMap;

use serde_json::{Value as Json, json};
use spot_iam_assign::{Change, EndCause, EventPayload, Plan, RoleDirectory};
use spot_iam_roles::{Assignment, Role, RoleName, Source};
use spot_iam_scim::{
    AuditEntry, GroupMapping, MappingStore, ScimError, ScimEvent, ScimResult, ScimStore,
    StoredGroup, StoredUser, TenantId, validate_mapping,
};
use spot_storage_contract::RecordId;
use time::OffsetDateTime;

// ---------------------------------------------------------------------------------------------
// The unit of work: the core's store over a snapshot
// ---------------------------------------------------------------------------------------------

/// One tenant's directory for one request. The core mutates it; `scim_store::persist` writes
/// the difference from the snapshot it was loaded as.
#[derive(Clone)]
pub(crate) struct Unit {
    pub(crate) tenant: TenantId,
    pub(crate) users: Vec<StoredUser>,
    pub(crate) groups: Vec<StoredGroup>,
    pub(crate) mappings: Vec<GroupMapping>,
    pub(crate) assignments: Vec<Assignment>,
    pub(crate) roles: Vec<String>,
    pub(crate) events: Vec<ScimEvent>,
    pub(crate) audits: Vec<AuditEntry>,
    pub(crate) live_keys: BTreeMap<RecordId, usize>,
    pub(crate) keys_to_disable: Vec<RecordId>,
}

impl Unit {
    fn mine(&self, t: &TenantId) -> bool {
        t == &self.tenant
    }
}

impl MappingStore for Unit {
    fn mappings(&self, t: &TenantId) -> Vec<GroupMapping> {
        let mut out = if self.mine(t) {
            self.mappings.clone()
        } else {
            Vec::new()
        };
        out.sort_by(|a, b| (&a.idp_group, &a.role).cmp(&(&b.idp_group, &b.role)));
        out
    }

    fn upsert_mapping(&mut self, t: &TenantId, m: GroupMapping) -> ScimResult<()> {
        if !self.mine(t) {
            return Err(ScimError::forbidden("another tenant"));
        }
        validate_mapping(&m, &self.role_directory())?;
        self.mappings
            .retain(|x| !x.idp_group.eq_ignore_ascii_case(&m.idp_group));
        self.mappings.push(m);
        Ok(())
    }

    fn remove_mapping(&mut self, t: &TenantId, group: &str) -> bool {
        let before = self.mappings.len();
        if self.mine(t) {
            self.mappings
                .retain(|x| !x.idp_group.eq_ignore_ascii_case(group));
        }
        self.mappings.len() != before
    }

    fn replace_mappings(&mut self, t: &TenantId, rows: Vec<GroupMapping>) -> ScimResult<()> {
        if !self.mine(t) {
            return Err(ScimError::forbidden("another tenant"));
        }
        for m in &rows {
            validate_mapping(m, &self.role_directory())?;
        }
        self.mappings = rows;
        Ok(())
    }

    fn role_directory(&self) -> RoleDirectory {
        RoleDirectory::new(self.roles.iter().map(|r| RoleName::from(r.as_str())))
    }

    /// Stored roles carry no portal kind, so every one is a desk role: a portal user is refused
    /// them (fail closed).
    fn role_records(&self) -> Vec<Role> {
        self.roles.iter().map(|r| Role::custom(r)).collect()
    }
}

impl ScimStore for Unit {
    fn find_user_by_name(&self, t: &TenantId, name: &str) -> Option<StoredUser> {
        let found = |u: &&StoredUser| u.profile.user_name.eq_ignore_ascii_case(name);
        self.mine(t)
            .then(|| self.users.iter().find(found).cloned())?
    }

    fn get_user(&self, t: &TenantId, id: &RecordId) -> Option<StoredUser> {
        let found = |u: &&StoredUser| &u.user.id == id;
        self.mine(t)
            .then(|| self.users.iter().find(found).cloned())?
    }

    fn list_users(&self, t: &TenantId) -> Vec<StoredUser> {
        let mut out = if self.mine(t) {
            self.users.clone()
        } else {
            vec![]
        };
        out.sort_by_key(|u| u.user.id);
        out
    }

    fn insert_user(&mut self, user: StoredUser) -> ScimResult<()> {
        if !self.mine(&user.tenant) || self.users.iter().any(|u| u.user.id == user.user.id) {
            return Err(ScimError::internal("a user with this id already exists"));
        }
        self.users.push(user);
        Ok(())
    }

    fn update_user(&mut self, user: StoredUser) -> ScimResult<()> {
        let slot = self.users.iter_mut().find(|u| u.user.id == user.user.id);
        let slot = slot.ok_or_else(|| ScimError::not_found("no such user"))?;
        *slot = user;
        Ok(())
    }

    fn get_group(&self, t: &TenantId, id: &RecordId) -> Option<StoredGroup> {
        let found = |g: &&StoredGroup| &g.id == id;
        self.mine(t)
            .then(|| self.groups.iter().find(found).cloned())?
    }

    fn find_group_by_name(&self, t: &TenantId, name: &str) -> Option<StoredGroup> {
        let found = |g: &&StoredGroup| g.display_name.eq_ignore_ascii_case(name);
        self.mine(t)
            .then(|| self.groups.iter().find(found).cloned())?
    }

    fn list_groups(&self, t: &TenantId) -> Vec<StoredGroup> {
        let mut out = if self.mine(t) {
            self.groups.clone()
        } else {
            vec![]
        };
        out.sort_by_key(|g| g.id);
        out
    }

    fn insert_group(&mut self, group: StoredGroup) -> ScimResult<()> {
        if !self.mine(&group.tenant) || self.groups.iter().any(|g| g.id == group.id) {
            return Err(ScimError::internal("a group with this id already exists"));
        }
        self.groups.push(group);
        Ok(())
    }

    fn update_group(&mut self, group: StoredGroup) -> ScimResult<()> {
        let slot = self.groups.iter_mut().find(|g| g.id == group.id);
        *slot.ok_or_else(|| ScimError::not_found("no such group"))? = group;
        Ok(())
    }

    fn delete_group(&mut self, t: &TenantId, id: &RecordId) -> bool {
        let before = self.groups.len();
        if self.mine(t) {
            self.groups.retain(|g| &g.id != id);
        }
        self.groups.len() != before
    }

    fn assignments(&self, t: &TenantId) -> Vec<Assignment> {
        if self.mine(t) {
            self.assignments.clone()
        } else {
            vec![]
        }
    }

    fn apply_assignments(
        &mut self,
        t: &TenantId,
        plan: &Plan,
        _now: OffsetDateTime,
    ) -> ScimResult<()> {
        if !self.mine(t) {
            return Err(ScimError::forbidden("another tenant"));
        }
        let current = self.assignments.clone();
        let mut next = current.clone();
        let mut events: Vec<(&str, Json)> = Vec::new();
        for change in &plan.changes {
            match change {
                Change::Grant(a) => {
                    // The reconciler's id is its deterministic key; the stored record has a real id.
                    let mut granted = a.clone();
                    granted.id = RecordId::new_v7().to_string();
                    events.push((
                        "identity_assignment_created",
                        assignment_json(&granted, None),
                    ));
                    next.push(granted);
                }
                Change::End {
                    id, at, user, role, ..
                } => {
                    next.iter_mut()
                        .filter(|a| &a.id == id)
                        .for_each(|a| a.valid_to = Some(*at));
                    let source = current
                        .iter()
                        .find(|a| &a.id == id)
                        .map_or(Source::Manual, |a| a.source);
                    let cause = plan
                        .events
                        .iter()
                        .find_map(|p| match p {
                            EventPayload::Ended {
                                assignment, cause, ..
                            } if assignment == id => Some(*cause),
                            _ => None,
                        })
                        .unwrap_or(EndCause::IdpRemoved);
                    events.push((
                        "identity_assignment_ended",
                        json!({"assignment": id, "user": user, "role": role.as_str(),
                               "source": source_name(source), "cause": cause.as_str()}),
                    ));
                }
                Change::Replace { id, with } => {
                    let mut after = with.clone();
                    after.id.clone_from(id);
                    if let Some(before) = current.iter().find(|a| &a.id == id) {
                        events.push((
                            "identity_assignment_changed",
                            assignment_json(&after, Some(before)),
                        ));
                    }
                    next.iter_mut()
                        .filter(|a| &a.id == id)
                        .for_each(|a| *a = after.clone());
                }
            }
        }
        self.assignments = next;
        for (topic, data) in events {
            self.emit(ScimEvent {
                tenant: t.clone(),
                topic: topic.to_owned(),
                data,
            })?;
        }
        Ok(())
    }

    fn emit(&mut self, event: ScimEvent) -> ScimResult<()> {
        spot_events_contract::identity::validate(&event.topic, &event.data).map_err(|errors| {
            let detail: Vec<_> = errors
                .iter()
                .map(|e| format!("{}: {}", e.path, e.message))
                .collect();
            ScimError::internal(format!(
                "an emitted event is invalid: {}",
                detail.join("; ")
            ))
        })?;
        self.events.push(event);
        Ok(())
    }

    fn audit(&mut self, entry: AuditEntry) {
        self.audits.push(entry);
    }

    fn new_user_id(&mut self) -> RecordId {
        RecordId::new_v7()
    }

    fn new_group_id(&mut self) -> RecordId {
        RecordId::new_v7()
    }

    /// Sessions are ended by the sign-in path itself: it re-reads the user's state on every
    /// request, so a disabled user's session is dead at commit. There is no count to report.
    fn end_sessions(&mut self, _t: &TenantId, _u: &RecordId) -> usize {
        0
    }

    fn revoke_refresh_tokens(&mut self, _t: &TenantId, _u: &RecordId) -> usize {
        0
    }

    fn disable_api_keys(&mut self, _t: &TenantId, user: &RecordId) -> usize {
        self.keys_to_disable.push(*user);
        self.live_keys.get(user).copied().unwrap_or(0)
    }

    fn cancel_invitations(&mut self, _t: &TenantId, _u: &RecordId) -> usize {
        0
    }

    fn reassign_open_tasks(
        &mut self,
        _t: &TenantId,
        _u: &RecordId,
        _to: Option<RecordId>,
    ) -> usize {
        0
    }

    /// The service counts requests per credential before the unit exists.
    fn rate_check(
        &mut self,
        _k: &RecordId,
        _limit: u32,
        _now: OffsetDateTime,
    ) -> std::result::Result<(), u64> {
        Ok(())
    }
}

pub(crate) fn source_name(source: Source) -> &'static str {
    match source {
        Source::Manual => "manual",
        Source::Rule => "rule",
        Source::Idp => "idp",
        Source::Emergency => "emergency",
    }
}

pub(crate) fn source_of(text: &str) -> Source {
    match text {
        "rule" => Source::Rule,
        "idp" => Source::Idp,
        "emergency" => Source::Emergency,
        _ => Source::Manual,
    }
}

/// `identity_assignment_created` (no `before`) or `identity_assignment_changed`.
pub(crate) fn assignment_json(after: &Assignment, before: Option<&Assignment>) -> Json {
    let mut map = json!({"assignment": after.id, "user": after.user, "role": after.role.as_str()});
    let extra = match before {
        None => json!({"source": source_name(after.source),
                       "scope": serde_json::to_value(&after.scope).unwrap_or(Json::Null)}),
        Some(b) => json!({"before": serde_json::to_value(b).unwrap_or(Json::Null),
                          "after": serde_json::to_value(after).unwrap_or(Json::Null)}),
    };
    if let (Some(m), Some(e)) = (map.as_object_mut(), extra.as_object()) {
        m.extend(e.clone());
    }
    if before.is_none()
        && let (Some(m), Some(reason)) = (map.as_object_mut(), &after.reason)
    {
        m.insert("reason".to_owned(), Json::String(reason.clone()));
    }
    map
}
