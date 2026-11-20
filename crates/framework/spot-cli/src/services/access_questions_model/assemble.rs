//! The pure assembly of the model (WP-IAM-08.T13): stored directory records and the installed apps'
//! grants in, the registry, the assignments and the protection facts out. Nothing here reads storage,
//! the clock or a file; every function is a function of its arguments, so the agreement test can drive
//! it from a seeded matrix.
//!
//! What the engine of this site enforces is the model of `permissions.rs`: grants keyed by **role
//! name** (the installed apps' `permissions/grants.toml` entries, plus the default grants of customised
//! DocTypes), and per request the assignments of the stored `role_assignment` records whose `state` is
//! `active` and whose window holds now. The registry built here mirrors exactly that: a role named by
//! a grant entry carries it as a **direct grant**, so the questions' decision core (the same
//! `PermissionEngine`) gives the answer the live engine gives, and the reason says which role and app
//! granted it.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use spot_iam_questions::ShareRecord;
use spot_iam_roles::{
    Assignment, CopiedFrom, Duty, DutyName, DutyRef, MAX_NAME, Origin, Registry, Role, RoleKind,
    RoleName, Scope, Source, Version,
};
use spot_meta::EffectiveMeta;
use spot_permissions::{
    AppPermissionGrant, FieldFacts, FieldGrant, GroupName, MaskFormat, ProtectionGroup, RoleGrant,
    Share, Tag, UserPermission,
};
use spot_sod::RuleSet;
use time::OffsetDateTime;

use super::{
    CODE_DUTIES_NOT_ENFORCED, CODE_DUTIES_UNDEFINED, CODE_RECORDS_SKIPPED, CODE_REGISTRY_INVALID,
    ModelError, Note,
};

/// The pack name a reference role gets when no installed app names it in a grant.
pub const DEFAULT_PACK: &str = "platform";

/// One stored `role` document with its child rows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RawRole {
    /// The record id (text).
    pub id: String,
    /// `role_name`.
    pub name: String,
    /// `origin`: `reference` or `custom`; absent is read from `is_system`.
    pub origin: Option<String>,
    /// `is_system`.
    pub is_system: bool,
    /// `copied_from`: the record id of the role this one was copied from.
    pub copied_from: Option<String>,
    /// The `duties` child rows (`role_duty`): the duty name and its pinned version, if any.
    pub duties: Vec<(String, Option<u32>)>,
    /// The role names of an optional `parents` child table; the role DocType declares none today,
    /// so a site has none until it does.
    pub parents: Vec<String>,
    /// An optional `kind` field (`portal` or `desk`); absent is a desk role (closed).
    pub kind: Option<String>,
}

/// One stored `role_assignment` document with its scope rows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RawAssignment {
    /// The record id (text): what every reason names.
    pub id: String,
    /// The user's record id (text), as the engine's principal names them.
    pub user: String,
    /// The role's record id (text).
    pub role_id: String,
    /// `source`: `manual`, `rule`, `idp` or `emergency`.
    pub source: String,
    /// `state`: `active`, `ended` or `expired`.
    pub state: String,
    /// `valid_from`.
    pub valid_from: Option<OffsetDateTime>,
    /// `valid_to`.
    pub valid_to: Option<OffsetDateTime>,
    /// `reason`.
    pub reason: Option<String>,
    /// The `scope` child rows, decoded as the engine decodes them.
    pub scope: Vec<UserPermission>,
}

/// One stored `user` document, reduced to what the portal rule needs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RawUser {
    /// The record id (text).
    pub id: String,
    /// `user_type`: `internal`, `portal`, `service` or `agent`.
    pub user_type: String,
}

/// Everything the directory reader returns, in one consistent read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RawDirectory {
    /// Every stored role.
    pub roles: Vec<RawRole>,
    /// Every stored assignment, active or not, of every user type.
    pub assignments: Vec<RawAssignment>,
    /// Every stored user.
    pub users: Vec<RawUser>,
}

/// The grants the installed apps and the live site give to roles, each with the app that ships it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GrantInputs {
    /// DocType grants, with the app each came from (`platform` for the default grants of customised
    /// DocTypes, which a runtime change adds).
    pub grants: Vec<(String, RoleGrant)>,
    /// Field grants, with their app.
    pub field_grants: Vec<(String, FieldGrant)>,
    /// The protection groups the model declares.
    pub groups: Vec<ProtectionGroup>,
    /// The named app permissions granted to roles.
    pub app_permissions: Vec<AppPermissionGrant>,
    /// The shares the installed apps declare.
    pub shares: Vec<Share>,
}

/// What one assembly was built from beyond the directory.
pub struct AssembleInput<'a> {
    /// The stored records.
    pub directory: &'a RawDirectory,
    /// The apps' grants.
    pub grants: &'a GrantInputs,
    /// The duty definitions the apps ship (`<app>/duties.toml`).
    pub duties: &'a [Duty],
    /// The SoD rules.
    pub rules: &'a RuleSet,
    /// The effective metadata of the serving snapshot.
    pub meta: Arc<EffectiveMeta>,
}

/// The assembled, immutable part of a model: shared by every question of one snapshot version.
#[derive(Debug)]
pub struct ModelCore {
    /// Duties and roles.
    pub registry: Registry,
    /// Every assignment, active or not (an inactive one carries a collapsed window).
    pub assignments: Vec<Assignment>,
    /// The record ids of the portal users.
    pub portal: Vec<String>,
    /// The protection groups.
    pub groups: Vec<ProtectionGroup>,
    /// The shares of the model.
    pub shares: Vec<ShareRecord>,
    /// The protection facts of each DocType's fields.
    pub field_facts: BTreeMap<String, Vec<FieldFacts>>,
    /// The SoD rules.
    pub rules: RuleSet,
    /// The effective metadata.
    pub meta: Arc<EffectiveMeta>,
    /// What the assembly noticed and did not fail on.
    pub notes: Vec<Note>,
}

/// Whether `name` is a role name `spot-iam-roles` accepts (it refuses a registry with one that is
/// not, so a stored role with a bad name is skipped and counted here instead).
fn valid_name(name: &str) -> bool {
    !name.trim().is_empty()
        && name == name.trim()
        && name.chars().count() <= MAX_NAME
        && !name.chars().any(char::is_control)
}

fn source_of(text: &str) -> Source {
    match text {
        "rule" => Source::Rule,
        "idp" => Source::Idp,
        "emergency" => Source::Emergency,
        _ => Source::Manual,
    }
}

/// The protection facts of every field of every DocType, as the site's engine builds them
/// (`permissions::protection_facts`): the metadata's field protection, grouped by DocType.
#[must_use]
pub fn field_facts_of(meta: &EffectiveMeta) -> BTreeMap<String, Vec<FieldFacts>> {
    let mut out = BTreeMap::new();
    for view in meta.doctypes() {
        let name = view.name.as_str().to_owned();
        let prefix = format!("{name}.");
        let facts: Vec<FieldFacts> = meta
            .field_protection()
            .iter()
            .filter_map(|p| {
                let path = p.path.strip_prefix(&prefix)?;
                Some(FieldFacts {
                    path: path.to_owned(),
                    groups: p.groups.iter().cloned().map(GroupName).collect(),
                    tags: p.tags.iter().cloned().map(Tag).collect(),
                    mask: p.mask.as_deref().map(|text| {
                        text.strip_prefix("last_chars(")
                            .and_then(|rest| rest.strip_suffix(')'))
                            .and_then(|n| n.parse::<u8>().ok())
                            .map_or(MaskFormat::Placeholder, MaskFormat::LastChars)
                    }),
                })
            })
            .collect();
        out.insert(name, facts);
    }
    out
}

/// The groups the model declares plus every group the metadata names and the model does not (the
/// site's engine declares them closed, so nothing opens them).
#[must_use]
pub fn groups_of(declared: &[ProtectionGroup], meta: &EffectiveMeta) -> Vec<ProtectionGroup> {
    let mut groups = declared.to_vec();
    for protection in meta.field_protection() {
        for group in &protection.groups {
            if !groups.iter().any(|g| &g.name.0 == group) {
                groups.push(ProtectionGroup {
                    name: GroupName(group.clone()),
                    trace: false,
                });
            }
        }
    }
    groups
}

/// The role the shape of an assignment compares by: two assignments that give one user one role
/// with one scope and one window are a duplicate the roles crate refuses.
type Shape = (
    Vec<UserPermission>,
    Option<OffsetDateTime>,
    Option<OffsetDateTime>,
);

/// Assembles the model core.
///
/// # Errors
///
/// [`ModelError::RegistryInvalid`] when the stored roles cannot form a registry (an inheritance
/// cycle or a role deeper than the limit): the questions then refuse, never answer from part of it.
pub fn assemble(input: &AssembleInput<'_>) -> Result<ModelCore, ModelError> {
    let mut skipped: u64 = 0;
    let mut notes = Vec::new();
    let grants = input.grants;

    // The app that ships each role's first grant: the pack a reference role is said to come from.
    let mut pack_of: BTreeMap<&str, &str> = BTreeMap::new();
    for (app, grant) in &grants.grants {
        pack_of.entry(grant.role.as_str()).or_insert(app.as_str());
    }
    for (app, grant) in &grants.field_grants {
        pack_of.entry(grant.role.as_str()).or_insert(app.as_str());
    }

    // Stored roles, kept by name; a bad name or a repeated name is skipped and counted.
    let mut kept: Vec<&super::RawRole> = Vec::new();
    let mut names: BTreeSet<&str> = BTreeSet::new();
    for raw in &input.directory.roles {
        if !valid_name(&raw.name) || !names.insert(raw.name.as_str()) {
            skipped += 1;
            continue;
        }
        kept.push(raw);
    }
    let name_by_id: BTreeMap<&str, &str> = kept
        .iter()
        .map(|r| (r.id.as_str(), r.name.as_str()))
        .collect();

    // Duty definitions: the shipped ones, plus an empty definition for every duty a stored role
    // names that no app defines (it grants nothing, and the registry needs it to exist).
    // The engine of this site decides by role-named grants only and never reads a duty's rights, so
    // the duties of the registry keep their names (the questions look for a duty by name and the
    // portal rule reads its outside mark) and lose their rights: an answer must not say "allowed"
    // where the engine says "no". A shipped name repeated by two apps keeps the first definition.
    let defined: BTreeSet<&str> = input.duties.iter().map(|d| d.name.as_str()).collect();
    let mut duties: Vec<Duty> = Vec::new();
    let mut shipped: BTreeSet<&str> = BTreeSet::new();
    for duty in input.duties {
        if !shipped.insert(duty.name.as_str()) {
            skipped += 1;
            continue;
        }
        let mut bare = duty.clone();
        bare.grants.clear();
        bare.field_grants.clear();
        bare.app_permissions.clear();
        duties.push(bare);
    }
    let mut undefined: BTreeSet<&str> = BTreeSet::new();
    let mut not_enforced: BTreeSet<&str> = BTreeSet::new();
    for raw in &kept {
        for (duty, _) in &raw.duties {
            if duty.is_empty() {
                continue;
            }
            if defined.contains(duty.as_str()) {
                let has_rights = input.duties.iter().any(|d| {
                    d.name.as_str() == duty
                        && (!d.grants.is_empty()
                            || !d.field_grants.is_empty()
                            || !d.app_permissions.is_empty())
                });
                if has_rights {
                    not_enforced.insert(duty.as_str());
                }
            } else if undefined.insert(duty.as_str()) {
                duties.push(Duty {
                    name: DutyName::from(duty.as_str()),
                    version: Version(1),
                    grants: Vec::new(),
                    field_grants: Vec::new(),
                    app_permissions: Vec::new(),
                    outside_exposed: false,
                });
            }
        }
    }
    let duty_version: BTreeMap<&str, Version> = duties
        .iter()
        .map(|d| (d.name.as_str(), d.version))
        .collect();

    // The roles of the registry: every stored role, then one reference role for every role a grant
    // names that has no stored record.
    let mut roles: Vec<Role> = Vec::new();
    for raw in &kept {
        let reference = match raw.origin.as_deref() {
            Some("reference") => true,
            Some(_) => false,
            None => raw.is_system,
        };
        let mut role = Role::custom(&raw.name);
        role.origin = if reference {
            Origin::Reference {
                pack: pack_of
                    .get(raw.name.as_str())
                    .copied()
                    .unwrap_or(DEFAULT_PACK)
                    .to_owned(),
                version: Version(1),
            }
        } else {
            Origin::Custom {
                copied_from: raw
                    .copied_from
                    .as_deref()
                    .and_then(|id| name_by_id.get(id))
                    .map(|from| CopiedFrom {
                        role: RoleName::from(*from),
                        version: Version(1),
                    }),
            }
        };
        let mut seen_duty: BTreeSet<&str> = BTreeSet::new();
        for (duty, pin) in &raw.duties {
            if duty.is_empty() || !seen_duty.insert(duty.as_str()) {
                continue;
            }
            // A pin is kept only when it matches the duty the registry holds: a stale pin would
            // make the registry refuse the whole model, and the engine does not read duties.
            let version = pin
                .map(Version)
                .filter(|v| duty_version.get(duty.as_str()) == Some(v));
            if pin.is_some() && version.is_none() {
                skipped += 1;
            }
            role.duties.push(DutyRef {
                name: DutyName::from(duty.as_str()),
                version,
            });
        }
        for parent in &raw.parents {
            if parent != &raw.name && names.contains(parent.as_str()) {
                role.parents.push(RoleName::from(parent.as_str()));
            } else {
                skipped += 1;
            }
        }
        if raw.kind.as_deref() == Some("portal") {
            role.kind = RoleKind::Portal;
        }
        roles.push(role);
    }
    let mut from_grants: BTreeSet<&str> = BTreeSet::new();
    for role in grants
        .grants
        .iter()
        .map(|(_, g)| g.role.as_str())
        .chain(grants.field_grants.iter().map(|(_, g)| g.role.as_str()))
        .chain(grants.app_permissions.iter().map(|g| g.role.as_str()))
    {
        if !names.contains(role) && valid_name(role) && from_grants.insert(role) {
            let mut synth = Role::custom(role);
            synth.origin = Origin::Reference {
                pack: pack_of
                    .get(role)
                    .copied()
                    .unwrap_or(DEFAULT_PACK)
                    .to_owned(),
                version: Version(1),
            };
            roles.push(synth);
        }
    }
    // The grants of each role, as direct grants: the registry's own rule (a grant names its owner)
    // holds because the key is the role's name.
    for role in &mut roles {
        let name = role.name.as_str().to_owned();
        role.direct_grants = grants
            .grants
            .iter()
            .filter(|(_, g)| g.role.as_str() == name)
            .map(|(_, g)| g.clone())
            .collect();
        role.direct_field_grants = grants
            .field_grants
            .iter()
            .filter(|(_, g)| g.role.as_str() == name)
            .map(|(_, g)| g.clone())
            .collect();
        role.direct_app_permissions = grants
            .app_permissions
            .iter()
            .filter(|g| g.role.as_str() == name)
            .map(|g| g.permission.clone())
            .collect();
    }

    let registry = Registry::new(duties, roles).map_err(|errors| {
        tracing::warn!(
            code = CODE_REGISTRY_INVALID,
            problems = %errors.codes().join(", "),
            "the stored roles do not form a registry"
        );
        ModelError::RegistryInvalid
    })?;

    // Assignments: the engine's `active()` is `state == active` and the window holds. An assignment
    // that is not active is kept, with a window that has closed for good, so a question never counts
    // it and the "who held this" reading still sees the record.
    let mut assignments: Vec<Assignment> = Vec::new();
    let mut ids: BTreeSet<&str> = BTreeSet::new();
    let mut shapes: BTreeMap<(&str, &str), Vec<Shape>> = BTreeMap::new();
    let mut closed: i64 = 0;
    for raw in &input.directory.assignments {
        let Some(role) = name_by_id.get(raw.role_id.as_str()) else {
            skipped += 1;
            continue;
        };
        if raw.id.is_empty() || raw.user.is_empty() || !ids.insert(raw.id.as_str()) {
            skipped += 1;
            continue;
        }
        let active = raw.state == "active";
        let (valid_from, valid_to) = if active {
            (raw.valid_from, raw.valid_to)
        } else {
            closed += 1;
            (
                Some(OffsetDateTime::UNIX_EPOCH),
                Some(OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(closed)),
            )
        };
        if let (Some(from), Some(to)) = (valid_from, valid_to)
            && to <= from
        {
            skipped += 1;
            continue;
        }
        let shape: Shape = (raw.scope.clone(), valid_from, valid_to);
        let seen = shapes.entry((raw.user.as_str(), *role)).or_default();
        if seen.contains(&shape) {
            skipped += 1;
            continue;
        }
        seen.push(shape);
        assignments.push(Assignment {
            id: raw.id.clone(),
            user: raw.user.clone(),
            role: RoleName::from(*role),
            scope: Scope {
                restrictions: raw.scope.clone(),
            },
            valid_from,
            valid_to,
            source: source_of(&raw.source),
            reason: raw.reason.clone(),
        });
    }
    assignments.sort_by(|a, b| (&a.user, &a.id).cmp(&(&b.user, &b.id)));

    let mut portal: Vec<String> = input
        .directory
        .users
        .iter()
        .filter(|u| u.user_type == "portal")
        .map(|u| u.id.clone())
        .collect();
    portal.sort();
    portal.dedup();

    if skipped > 0 {
        notes.push(Note {
            code: CODE_RECORDS_SKIPPED,
            count: skipped,
        });
    }
    if !undefined.is_empty() {
        notes.push(Note {
            code: CODE_DUTIES_UNDEFINED,
            count: undefined.len() as u64,
        });
    }
    if !not_enforced.is_empty() {
        notes.push(Note {
            code: CODE_DUTIES_NOT_ENFORCED,
            count: not_enforced.len() as u64,
        });
    }
    Ok(ModelCore {
        registry,
        assignments,
        portal,
        groups: groups_of(&grants.groups, &input.meta),
        shares: grants
            .shares
            .iter()
            .map(|s| ShareRecord {
                doc: s.doc,
                user: s.user.clone(),
                rights: s.rights.clone(),
            })
            .collect(),
        field_facts: field_facts_of(&input.meta),
        rules: input.rules.clone(),
        meta: Arc::clone(&input.meta),
        notes,
    })
}
