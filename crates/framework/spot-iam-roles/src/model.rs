//! The records: duties, roles and assignments (ADR-070 sections 2 and 3).
//!
//! **Scope belongs to the assignment, never to the user** and never to a role: a [`Scope`]
//! exists only inside an [`Assignment`], so "Accounting Manager in Company A" and "Accounting
//! Viewer in Company B" are two assignments that the permission engine evaluates separately.
//!
//! Every type is `serde` with `deny_unknown_fields`.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use spot_permissions::{AppPermissionName, FieldGrant, RoleGrant, UserPermission};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::error::{self as e, IamError, IamErrors, Result};

/// The longest role or duty name, in characters.
pub const MAX_NAME: usize = 100;

/// A version of a duty or of a reference pack: a whole number that only grows.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Version(pub u32);

/// A role's name. Free text, so a site's own roles need no code change.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RoleName(pub String);

impl RoleName {
    /// The name as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for RoleName {
    fn from(name: &str) -> Self {
        Self(name.to_owned())
    }
}

/// A duty's name.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DutyName(pub String);

impl DutyName {
    /// The name as text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for DutyName {
    fn from(name: &str) -> Self {
        Self(name.to_owned())
    }
}

pub(crate) fn check_name(code_subject: &str, name: &str, errors: &mut Vec<IamError>) {
    if name.trim().is_empty()
        || name != name.trim()
        || name.chars().count() > MAX_NAME
        || name.chars().any(char::is_control)
    {
        errors.push(IamError::about(
            e::INVALID_NAME,
            code_subject,
            format!(
                "a name has text without leading or trailing spaces or control characters, \
                 at most {MAX_NAME} characters"
            ),
        ));
    }
}

/// A named, versioned bundle of rights matching a part of a business process, for example
/// "post sales invoices". Its grants name the duty itself as their role; a role that includes
/// the duty gets them re-keyed to its own name when the role is resolved.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Duty {
    /// The duty's name.
    pub name: DutyName,
    /// Its version; a role may pin it.
    pub version: Version,
    /// DocType rights. Each grant's `role` is the duty's own name.
    #[serde(default)]
    pub grants: Vec<RoleGrant>,
    /// Field rights. Each grant's `role` is the duty's own name.
    #[serde(default)]
    pub field_grants: Vec<FieldGrant>,
    /// The named app permissions the duty gives (WP-PLT-03.T3). A role that includes the duty
    /// holds them. Absent in a stored duty means none.
    #[serde(default)]
    pub app_permissions: Vec<AppPermissionName>,
    /// Whether the duty may be held by an outside (portal) user (ADR-083 section 2.3,
    /// Barrier 3). **Closed by default**: absent in a stored duty means `false`, so no
    /// existing duty becomes reachable from outside by this field being introduced.
    #[serde(default)]
    pub outside_exposed: bool,
}

/// A role's use of a duty.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DutyRef {
    /// The duty.
    pub name: DutyName,
    /// The version the role was built against; when set, the registry must hold exactly it.
    #[serde(default)]
    pub version: Option<Version>,
}

impl DutyRef {
    /// A reference that takes whatever version the registry has.
    pub fn named(name: &str) -> Self {
        Self {
            name: DutyName::from(name),
            version: None,
        }
    }
}

/// The role a custom role was copied from, and the version it had then.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CopiedFrom {
    /// The source role's name.
    pub role: RoleName,
    /// The source's version when the copy was made.
    pub version: Version,
}

/// Where a role comes from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Origin {
    /// Shipped in a pack: read-only. To change one, copy it.
    Reference {
        /// The pack.
        pack: String,
        /// The pack's version.
        version: Version,
    },
    /// Made by the tenant, from nothing or as a copy.
    Custom {
        /// The reference role this was copied from, if any.
        #[serde(default)]
        copied_from: Option<CopiedFrom>,
    },
}

/// The kind of a role, as far as Barrier 3 of ADR-083 cares.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum RoleKind {
    /// A role for people at the desk. The default: an old stored role stays a desk role.
    #[default]
    Desk,
    /// A portal role: the only kind a portal user may hold, and only when every duty it
    /// reaches is marked outside-exposed.
    Portal,
}

/// A role: duties, direct grants and parents.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Role {
    /// The role's name.
    pub name: RoleName,
    /// Where it comes from.
    pub origin: Origin,
    /// The duties it includes.
    #[serde(default)]
    pub duties: Vec<DutyRef>,
    /// DocType rights it grants itself. Each grant's `role` is the role's own name.
    #[serde(default)]
    pub direct_grants: Vec<RoleGrant>,
    /// Field rights it grants itself. Each grant's `role` is the role's own name.
    #[serde(default)]
    pub direct_field_grants: Vec<FieldGrant>,
    /// The named app permissions it holds itself (WP-PLT-03.T3). Absent in a stored role
    /// means none.
    #[serde(default)]
    pub direct_app_permissions: Vec<AppPermissionName>,
    /// The roles it inherits from. Acyclic, at most eight deep.
    #[serde(default)]
    pub parents: Vec<RoleName>,
    /// Desk or portal. Absent in a stored role means `desk` (closed).
    #[serde(default)]
    pub kind: RoleKind,
}

impl Role {
    /// A custom role with nothing in it.
    pub fn custom(name: &str) -> Self {
        Self {
            name: RoleName::from(name),
            origin: Origin::Custom { copied_from: None },
            duties: Vec::new(),
            direct_grants: Vec::new(),
            direct_field_grants: Vec::new(),
            direct_app_permissions: Vec::new(),
            parents: Vec::new(),
            kind: RoleKind::Desk,
        }
    }

    /// The role's kind.
    pub fn kind(&self) -> RoleKind {
        self.kind
    }

    /// Whether the role is a reference role (read-only).
    pub fn is_reference(&self) -> bool {
        matches!(self.origin, Origin::Reference { .. })
    }
}

/// Where an assignment came from (ADR-070 section 4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// An administrator gave it.
    Manual,
    /// An assignment rule gave it.
    Rule,
    /// The identity provider's group mapping gave it.
    Idp,
    /// An emergency-access grant gave it (always time-boxed).
    Emergency,
}

/// The value restrictions of one assignment: the `UserPermission`s of `spot-permissions`,
/// including the include-descendants option. Nothing else carries scope.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scope {
    /// The restrictions; empty means the role applies everywhere.
    #[serde(default)]
    pub restrictions: Vec<UserPermission>,
}

/// One role given to one user, with its own scope and validity (ADR-070 section 3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Assignment {
    /// The assignment's id: what every reason and event names.
    pub id: String,
    /// The user it is for, as the permission engine knows them.
    pub user: String,
    /// The role it gives.
    pub role: RoleName,
    /// Where the role applies.
    #[serde(default)]
    pub scope: Scope,
    /// From this instant (inclusive); `None` means from the start.
    #[serde(default, with = "opt_utc")]
    pub valid_from: Option<OffsetDateTime>,
    /// Until this instant (exclusive); `None` means no end.
    #[serde(default, with = "opt_utc")]
    pub valid_to: Option<OffsetDateTime>,
    /// Where it came from.
    pub source: Source,
    /// Why it exists.
    #[serde(default)]
    pub reason: Option<String>,
}

impl Assignment {
    /// An unscoped, unlimited manual assignment.
    pub fn new(id: &str, user: &str, role: &str) -> Self {
        Self {
            id: id.to_owned(),
            user: user.to_owned(),
            role: RoleName::from(role),
            scope: Scope::default(),
            valid_from: None,
            valid_to: None,
            source: Source::Manual,
            reason: None,
        }
    }

    /// Checks the record on its own, reporting every problem at once.
    pub fn validate(&self) -> Result<()> {
        let mut problems = Vec::new();
        if self.id.trim().is_empty() {
            problems.push(IamError::new(e::INVALID_NAME, "an assignment needs an id"));
        }
        if self.user.trim().is_empty() {
            problems.push(IamError::about(
                e::INVALID_NAME,
                &self.id,
                "an assignment needs a user",
            ));
        }
        if let (Some(from), Some(to)) = (self.valid_from, self.valid_to)
            && to <= from
        {
            problems.push(IamError::about(
                e::ASSIGNMENT_WINDOW,
                &self.id,
                "`valid_to` must be after `valid_from`",
            ));
        }
        match IamErrors::from_list(problems) {
            Some(errors) => Err(errors),
            None => Ok(()),
        }
    }

    /// Whether the assignment is in force at `now`: `valid_from` inclusive, `valid_to`
    /// exclusive.
    pub fn active_at(&self, now: OffsetDateTime) -> bool {
        self.valid_from.is_none_or(|from| from <= now) && self.valid_to.is_none_or(|to| now < to)
    }
}

/// Whether `assignment` is in force at `now`.
pub fn active_at(assignment: &Assignment, now: OffsetDateTime) -> bool {
    assignment.active_at(now)
}

mod opt_utc {
    use super::*;

    pub fn serialize<S: Serializer>(t: &Option<OffsetDateTime>, s: S) -> Result2<S> {
        match t {
            Some(t) => s.serialize_str(
                &t.to_offset(time::UtcOffset::UTC)
                    .format(&Rfc3339)
                    .map_err(serde::ser::Error::custom)?,
            ),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        d: D,
    ) -> std::result::Result<Option<OffsetDateTime>, D::Error> {
        match Option::<String>::deserialize(d)? {
            Some(text) => OffsetDateTime::parse(&text, &Rfc3339)
                .map(|t| Some(t.to_offset(time::UtcOffset::UTC)))
                .map_err(serde::de::Error::custom),
            None => Ok(None),
        }
    }

    type Result2<S> = std::result::Result<<S as Serializer>::Ok, <S as Serializer>::Error>;
}
