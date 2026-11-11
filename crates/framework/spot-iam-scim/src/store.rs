//! The persistence contract the SCIM core works against, and an in-memory implementation.
//!
//! The core is sans-I/O: it never opens a database, reads a clock or publishes an event by
//! itself. It calls [`ScimStore`], which the platform fills with real storage and a real event
//! outbox; [`MemoryStore`] is the same contract held in memory, so the conformance transcripts
//! and the unit tests run the identical code paths the server does.
//!
//! Every method is scoped by tenant. A SCIM call can only ever see and change the records of
//! the tenant its credential belongs to.

use std::collections::BTreeMap;

use serde_json::Value as Json;
use spot_events_contract::Actor;
use spot_iam_assign::RoleDirectory;
use spot_iam_roles::{Assignment, Role};
use spot_storage_contract::RecordId;
use time::OffsetDateTime;

use crate::TenantId;
use crate::error::{ScimError, ScimResult};
use crate::mapping::{GroupMapping, MappingStore, validate_mapping};
use crate::resource::{StoredGroup, StoredUser};

/// An identity event a mutation produced: its topic and its validated payload.
#[derive(Clone, Debug, PartialEq)]
pub struct ScimEvent {
    /// The tenant the event belongs to.
    pub tenant: TenantId,
    /// The `identity_*` topic.
    pub topic: String,
    /// The event's `data` document.
    pub data: Json,
}

/// One audit entry written with a mutation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditEntry {
    /// The tenant.
    pub tenant: TenantId,
    /// When.
    pub at: OffsetDateTime,
    /// Who acted (the service user that owns the credential).
    pub actor: Actor,
    /// What happened, as a stable action name (`scim.user.create`, ...).
    pub action: String,
    /// The record it is about.
    pub subject: String,
    /// A sentence naming no secret.
    pub detail: String,
}

/// The persistence the SCIM core needs: users, groups, the mapping table, the assignments the
/// group mapping decides, the events and audit a mutation emits, and the leaver side effects.
pub trait ScimStore: MappingStore {
    /// The user with `user_name`, compared case-insensitively.
    fn find_user_by_name(&self, tenant: &TenantId, user_name: &str) -> Option<StoredUser>;
    /// The user with `id`.
    fn get_user(&self, tenant: &TenantId, id: &RecordId) -> Option<StoredUser>;
    /// Every user of the tenant, ordered by id.
    fn list_users(&self, tenant: &TenantId) -> Vec<StoredUser>;
    /// Inserts a new user.
    fn insert_user(&mut self, user: StoredUser) -> ScimResult<()>;
    /// Replaces an existing user.
    fn update_user(&mut self, user: StoredUser) -> ScimResult<()>;

    /// The group with `id`.
    fn get_group(&self, tenant: &TenantId, id: &RecordId) -> Option<StoredGroup>;
    /// The group with `display_name`, compared case-insensitively.
    fn find_group_by_name(&self, tenant: &TenantId, display_name: &str) -> Option<StoredGroup>;
    /// Every group of the tenant, ordered by id.
    fn list_groups(&self, tenant: &TenantId) -> Vec<StoredGroup>;
    /// Inserts a new group.
    fn insert_group(&mut self, group: StoredGroup) -> ScimResult<()>;
    /// Replaces an existing group.
    fn update_group(&mut self, group: StoredGroup) -> ScimResult<()>;
    /// Removes a group. Groups, unlike users, may be deleted.
    fn delete_group(&mut self, tenant: &TenantId, id: &RecordId) -> bool;

    /// The tenant's live assignments.
    fn assignments(&self, tenant: &TenantId) -> Vec<Assignment>;
    /// Applies a reconciliation plan: the changes, then the events it raises.
    fn apply_assignments(
        &mut self,
        tenant: &TenantId,
        plan: &spot_iam_assign::Plan,
        now: OffsetDateTime,
    ) -> ScimResult<()>;

    /// Emits one identity event, validating it against the contract first.
    fn emit(&mut self, event: ScimEvent) -> ScimResult<()>;
    /// Writes one audit entry.
    fn audit(&mut self, entry: AuditEntry);

    /// A fresh user id.
    fn new_user_id(&mut self) -> RecordId;
    /// A fresh group id.
    fn new_group_id(&mut self) -> RecordId;
    /// A fresh role-assignment id: what a granted idp assignment is stored under (WP-IAM-04.T2).
    fn new_assignment_id(&mut self) -> RecordId {
        RecordId::new_v7()
    }

    /// Ends every session of the user; the count ended.
    fn end_sessions(&mut self, tenant: &TenantId, user: &RecordId) -> usize;
    /// Revokes every refresh token of the user; the count revoked.
    fn revoke_refresh_tokens(&mut self, tenant: &TenantId, user: &RecordId) -> usize;
    /// Disables every API key of the user; the count disabled.
    fn disable_api_keys(&mut self, tenant: &TenantId, user: &RecordId) -> usize;
    /// Cancels the pending invitations of the user; the count cancelled.
    fn cancel_invitations(&mut self, tenant: &TenantId, user: &RecordId) -> usize;
    /// Reassigns or flags the open tasks of the user; the count moved.
    fn reassign_open_tasks(
        &mut self,
        tenant: &TenantId,
        user: &RecordId,
        to: Option<RecordId>,
    ) -> usize;

    /// Counts one request against the credential's window. `Err(retry_after_secs)` when the
    /// window is full.
    fn rate_check(&mut self, key: &RecordId, limit: u32, now: OffsetDateTime) -> Result<(), u64>;
}

/// One tenant's records.
#[derive(Clone, Debug, Default)]
struct TenantData {
    users: Vec<StoredUser>,
    groups: Vec<StoredGroup>,
    mappings: Vec<GroupMapping>,
    assignments: Vec<Assignment>,
    sessions: BTreeMap<String, usize>,
    tokens: BTreeMap<String, usize>,
    keys: BTreeMap<String, usize>,
    invitations: BTreeMap<String, usize>,
    tasks: BTreeMap<String, usize>,
}

/// The in-memory store: the whole contract in one struct, so the core's behaviour is testable
/// without a database, a network or a clock.
#[derive(Clone, Debug, Default)]
pub struct MemoryStore {
    tenants: BTreeMap<TenantId, TenantData>,
    roles: Vec<Role>,
    events: Vec<ScimEvent>,
    audit: Vec<AuditEntry>,
    rate: BTreeMap<String, (i64, u32)>,
}

impl MemoryStore {
    /// An empty store with no roles.
    pub fn new() -> Self {
        Self::default()
    }

    /// A store whose tenant has these role records (the roles a mapping may name).
    pub fn with_roles(roles: Vec<Role>) -> Self {
        Self {
            roles,
            ..Self::default()
        }
    }

    fn tenant(&self, tenant: &TenantId) -> Option<&TenantData> {
        self.tenants.get(tenant)
    }

    fn tenant_mut(&mut self, tenant: &TenantId) -> &mut TenantData {
        self.tenants.entry(tenant.clone()).or_default()
    }

    /// The events emitted so far.
    pub fn events(&self) -> &[ScimEvent] {
        &self.events
    }

    /// The audit entries written so far.
    pub fn audit_entries(&self) -> &[AuditEntry] {
        &self.audit
    }

    /// The tenant's live assignments.
    pub fn assignments_of(&self, tenant: &TenantId) -> Vec<Assignment> {
        self.tenant(tenant)
            .map(|t| t.assignments.clone())
            .unwrap_or_default()
    }

    /// Seeds `n` sessions for a user, so a deactivation has something to end.
    pub fn seed_sessions(&mut self, tenant: &TenantId, user: &RecordId, n: usize) {
        self.tenant_mut(tenant).sessions.insert(user.to_string(), n);
    }

    /// Seeds `n` refresh tokens for a user.
    pub fn seed_refresh_tokens(&mut self, tenant: &TenantId, user: &RecordId, n: usize) {
        self.tenant_mut(tenant).tokens.insert(user.to_string(), n);
    }

    /// Seeds `n` API keys for a user.
    pub fn seed_api_keys(&mut self, tenant: &TenantId, user: &RecordId, n: usize) {
        self.tenant_mut(tenant).keys.insert(user.to_string(), n);
    }

    /// Seeds `n` pending invitations for a user.
    pub fn seed_invitations(&mut self, tenant: &TenantId, user: &RecordId, n: usize) {
        self.tenant_mut(tenant)
            .invitations
            .insert(user.to_string(), n);
    }

    /// Seeds `n` open tasks for a user.
    pub fn seed_open_tasks(&mut self, tenant: &TenantId, user: &RecordId, n: usize) {
        self.tenant_mut(tenant).tasks.insert(user.to_string(), n);
    }

    /// The sessions left for a user.
    pub fn sessions_of(&self, tenant: &TenantId, user: &RecordId) -> usize {
        self.count(tenant, |t| t.sessions.get(&user.to_string()).copied())
    }

    /// The refresh tokens left for a user.
    pub fn refresh_tokens_of(&self, tenant: &TenantId, user: &RecordId) -> usize {
        self.count(tenant, |t| t.tokens.get(&user.to_string()).copied())
    }

    /// The enabled API keys left for a user.
    pub fn api_keys_of(&self, tenant: &TenantId, user: &RecordId) -> usize {
        self.count(tenant, |t| t.keys.get(&user.to_string()).copied())
    }

    /// The pending invitations left for a user.
    pub fn invitations_of(&self, tenant: &TenantId, user: &RecordId) -> usize {
        self.count(tenant, |t| t.invitations.get(&user.to_string()).copied())
    }

    fn count(&self, tenant: &TenantId, f: impl Fn(&TenantData) -> Option<usize>) -> usize {
        self.tenant(tenant).and_then(&f).unwrap_or(0)
    }

    fn take(store: &mut BTreeMap<String, usize>, user: &RecordId) -> usize {
        let key = user.to_string();
        let n = store.get(&key).copied().unwrap_or(0);
        store.insert(key, 0);
        n
    }
}

impl MappingStore for MemoryStore {
    fn mappings(&self, tenant: &TenantId) -> Vec<GroupMapping> {
        self.tenant(tenant)
            .map(|t| t.mappings.clone())
            .unwrap_or_default()
    }

    fn upsert_mapping(&mut self, tenant: &TenantId, mapping: GroupMapping) -> ScimResult<()> {
        validate_mapping(&mapping, &self.role_directory())?;
        let data = self.tenant_mut(tenant);
        data.mappings
            .retain(|m| !m.idp_group.eq_ignore_ascii_case(&mapping.idp_group));
        data.mappings.push(mapping);
        Ok(())
    }

    fn remove_mapping(&mut self, tenant: &TenantId, idp_group: &str) -> bool {
        let data = self.tenant_mut(tenant);
        let before = data.mappings.len();
        data.mappings
            .retain(|m| !m.idp_group.eq_ignore_ascii_case(idp_group));
        data.mappings.len() != before
    }

    fn replace_mappings(
        &mut self,
        tenant: &TenantId,
        mappings: Vec<GroupMapping>,
    ) -> ScimResult<()> {
        let roles = self.role_directory();
        for mapping in &mappings {
            validate_mapping(mapping, &roles)?;
        }
        self.tenant_mut(tenant).mappings = mappings;
        Ok(())
    }

    fn role_directory(&self) -> RoleDirectory {
        RoleDirectory::new(self.roles.iter().map(|r| r.name.clone()))
    }

    fn role_records(&self) -> Vec<Role> {
        self.roles.clone()
    }
}

impl ScimStore for MemoryStore {
    fn find_user_by_name(&self, tenant: &TenantId, user_name: &str) -> Option<StoredUser> {
        self.tenant(tenant).and_then(|t| {
            t.users
                .iter()
                .find(|u| u.profile.user_name.eq_ignore_ascii_case(user_name))
                .cloned()
        })
    }

    fn get_user(&self, tenant: &TenantId, id: &RecordId) -> Option<StoredUser> {
        self.tenant(tenant)
            .and_then(|t| t.users.iter().find(|u| &u.user.id == id).cloned())
    }

    fn list_users(&self, tenant: &TenantId) -> Vec<StoredUser> {
        let mut users = self
            .tenant(tenant)
            .map(|t| t.users.clone())
            .unwrap_or_default();
        users.sort_by_key(|u| u.user.id);
        users
    }

    fn insert_user(&mut self, user: StoredUser) -> ScimResult<()> {
        if self.get_user(&user.tenant, &user.user.id).is_some() {
            return Err(ScimError::internal("a user with this id already exists"));
        }
        self.tenant_mut(&user.tenant).users.push(user);
        Ok(())
    }

    fn update_user(&mut self, user: StoredUser) -> ScimResult<()> {
        let data = self.tenant_mut(&user.tenant);
        let Some(slot) = data.users.iter_mut().find(|u| u.user.id == user.user.id) else {
            return Err(ScimError::not_found("no such user"));
        };
        *slot = user;
        Ok(())
    }

    fn get_group(&self, tenant: &TenantId, id: &RecordId) -> Option<StoredGroup> {
        self.tenant(tenant)
            .and_then(|t| t.groups.iter().find(|g| &g.id == id).cloned())
    }

    fn find_group_by_name(&self, tenant: &TenantId, display_name: &str) -> Option<StoredGroup> {
        self.tenant(tenant).and_then(|t| {
            t.groups
                .iter()
                .find(|g| g.display_name.eq_ignore_ascii_case(display_name))
                .cloned()
        })
    }

    fn list_groups(&self, tenant: &TenantId) -> Vec<StoredGroup> {
        let mut groups = self
            .tenant(tenant)
            .map(|t| t.groups.clone())
            .unwrap_or_default();
        groups.sort_by_key(|g| g.id);
        groups
    }

    fn insert_group(&mut self, group: StoredGroup) -> ScimResult<()> {
        if self.get_group(&group.tenant, &group.id).is_some() {
            return Err(ScimError::internal("a group with this id already exists"));
        }
        self.tenant_mut(&group.tenant).groups.push(group);
        Ok(())
    }

    fn update_group(&mut self, group: StoredGroup) -> ScimResult<()> {
        let data = self.tenant_mut(&group.tenant);
        let Some(slot) = data.groups.iter_mut().find(|g| g.id == group.id) else {
            return Err(ScimError::not_found("no such group"));
        };
        *slot = group;
        Ok(())
    }

    fn delete_group(&mut self, tenant: &TenantId, id: &RecordId) -> bool {
        let data = self.tenant_mut(tenant);
        let before = data.groups.len();
        data.groups.retain(|g| &g.id != id);
        data.groups.len() != before
    }

    fn assignments(&self, tenant: &TenantId) -> Vec<Assignment> {
        self.assignments_of(tenant)
    }

    fn apply_assignments(
        &mut self,
        tenant: &TenantId,
        plan: &spot_iam_assign::Plan,
        now: OffsetDateTime,
    ) -> ScimResult<()> {
        use spot_iam_assign::{Change, EndCause, EventPayload};
        let current = self.assignments_of(tenant);
        let mut next = current.clone();
        let mut emitted: Vec<(String, Json)> = Vec::new();
        for change in &plan.changes {
            match change {
                Change::Grant(assignment) => {
                    // The reconciler's id (`idp:<group>/<user>`) is its deterministic key; the
                    // stored assignment — and therefore the event — carries a record id.
                    let mut granted = assignment.clone();
                    granted.id = RecordId::new_v7().to_string();
                    emitted.push(created_event(&granted));
                    next.push(granted);
                }
                Change::End {
                    id, at, user, role, ..
                } => {
                    for stored in next.iter_mut().filter(|a| &a.id == id) {
                        stored.valid_to = Some(*at);
                    }
                    let source = current
                        .iter()
                        .find(|a| &a.id == id)
                        .map_or(spot_iam_roles::Source::Manual, |a| a.source);
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
                    emitted.push(ended_event(id, user, role, source, cause));
                }
                Change::Replace { id, with } => {
                    let before = current.iter().find(|a| &a.id == id).cloned();
                    for stored in next.iter_mut().filter(|a| &a.id == id) {
                        *stored = with.clone();
                    }
                    if let Some(before) = before {
                        emitted.push(changed_event(&before, with));
                    }
                }
            }
        }
        let _ = now;
        self.tenant_mut(tenant).assignments = next;
        for (topic, data) in emitted {
            self.emit(ScimEvent {
                tenant: tenant.clone(),
                topic,
                data,
            })?;
        }
        Ok(())
    }

    fn emit(&mut self, event: ScimEvent) -> ScimResult<()> {
        spot_events_contract::identity::validate(&event.topic, &event.data).map_err(|errors| {
            let detail = errors
                .iter()
                .map(|e| format!("{}: {}", e.path, e.message))
                .collect::<Vec<_>>()
                .join("; ");
            ScimError::internal(format!("an emitted event is invalid: {detail}"))
        })?;
        self.events.push(event);
        Ok(())
    }

    fn audit(&mut self, entry: AuditEntry) {
        self.audit.push(entry);
    }

    fn new_user_id(&mut self) -> RecordId {
        RecordId::new_v7()
    }

    fn new_group_id(&mut self) -> RecordId {
        RecordId::new_v7()
    }

    fn end_sessions(&mut self, tenant: &TenantId, user: &RecordId) -> usize {
        let store = &mut self.tenant_mut(tenant).sessions;
        Self::take(store, user)
    }

    fn revoke_refresh_tokens(&mut self, tenant: &TenantId, user: &RecordId) -> usize {
        let store = &mut self.tenant_mut(tenant).tokens;
        Self::take(store, user)
    }

    fn disable_api_keys(&mut self, tenant: &TenantId, user: &RecordId) -> usize {
        let store = &mut self.tenant_mut(tenant).keys;
        Self::take(store, user)
    }

    fn cancel_invitations(&mut self, tenant: &TenantId, user: &RecordId) -> usize {
        let store = &mut self.tenant_mut(tenant).invitations;
        Self::take(store, user)
    }

    fn reassign_open_tasks(
        &mut self,
        tenant: &TenantId,
        user: &RecordId,
        _to: Option<RecordId>,
    ) -> usize {
        let store = &mut self.tenant_mut(tenant).tasks;
        Self::take(store, user)
    }

    fn rate_check(&mut self, key: &RecordId, limit: u32, now: OffsetDateTime) -> Result<(), u64> {
        let window = 60_i64;
        let now_secs = now.unix_timestamp();
        let entry = self.rate.entry(key.to_string()).or_insert((now_secs, 0));
        if now_secs - entry.0 >= window {
            *entry = (now_secs, 0);
        }
        entry.1 += 1;
        if entry.1 > limit {
            let retry = (entry.0 + window - now_secs).max(0);
            return Err(u64::try_from(retry).unwrap_or(0));
        }
        Ok(())
    }
}

/// `identity_assignment_created`.
fn created_event(assignment: &Assignment) -> (String, Json) {
    let mut map = serde_json::Map::new();
    map.insert("assignment".to_owned(), Json::String(assignment.id.clone()));
    map.insert("user".to_owned(), Json::String(assignment.user.clone()));
    map.insert(
        "role".to_owned(),
        Json::String(assignment.role.as_str().to_owned()),
    );
    map.insert(
        "source".to_owned(),
        Json::String(source_name(assignment.source).to_owned()),
    );
    map.insert(
        "scope".to_owned(),
        serde_json::to_value(&assignment.scope).unwrap_or(Json::Null),
    );
    if let Some(reason) = &assignment.reason {
        map.insert("reason".to_owned(), Json::String(reason.clone()));
    }
    ("identity_assignment_created".to_owned(), Json::Object(map))
}

/// `identity_assignment_changed`.
fn changed_event(before: &Assignment, after: &Assignment) -> (String, Json) {
    let mut map = serde_json::Map::new();
    map.insert("assignment".to_owned(), Json::String(after.id.clone()));
    map.insert("user".to_owned(), Json::String(after.user.clone()));
    map.insert(
        "role".to_owned(),
        Json::String(after.role.as_str().to_owned()),
    );
    map.insert(
        "before".to_owned(),
        serde_json::to_value(before).unwrap_or(Json::Null),
    );
    map.insert(
        "after".to_owned(),
        serde_json::to_value(after).unwrap_or(Json::Null),
    );
    ("identity_assignment_changed".to_owned(), Json::Object(map))
}

/// `identity_assignment_ended`.
fn ended_event(
    id: &str,
    user: &str,
    role: &spot_iam_roles::RoleName,
    source: spot_iam_roles::Source,
    cause: spot_iam_assign::EndCause,
) -> (String, Json) {
    let mut map = serde_json::Map::new();
    map.insert("assignment".to_owned(), Json::String(id.to_owned()));
    map.insert("user".to_owned(), Json::String(user.to_owned()));
    map.insert("role".to_owned(), Json::String(role.as_str().to_owned()));
    map.insert(
        "source".to_owned(),
        Json::String(source_name(source).to_owned()),
    );
    map.insert("cause".to_owned(), Json::String(cause.as_str().to_owned()));
    ("identity_assignment_ended".to_owned(), Json::Object(map))
}

fn source_name(source: spot_iam_roles::Source) -> &'static str {
    match source {
        spot_iam_roles::Source::Manual => "manual",
        spot_iam_roles::Source::Rule => "rule",
        spot_iam_roles::Source::Idp => "idp",
        spot_iam_roles::Source::Emergency => "emergency",
    }
}
