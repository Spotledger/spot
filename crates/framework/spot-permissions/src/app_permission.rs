//! Named app permissions (WP-PLT-03.T3): a permission an installed app declares by name
//! (`approve_note`), a role is granted, and a whitelisted method can require
//! (`right = permission("approve_note")`).
//!
//! # Shape
//!
//! - [`AppPermissionName`]: a validated name. Case-sensitive, never trimmed or normalised.
//! - [`AppPermissionCatalogue`]: the permissions the installed apps of **one site** declare. A
//!   name belongs to exactly one app ([`AppPermissionCatalogue::insert`] refuses a duplicate).
//! - [`AppPermissionGrant`]: a role holds a permission. Granting is a role edit; it is stored
//!   per site like every other grant.
//! - [`PermissionEngine::app_permission`]: the decision, a pure function with a reason
//!   ([`AppDecision`], [`AppReason`]).
//!
//! # Rules
//!
//! - **Fail closed.** An invalid name, a name no installed app declares, a principal without
//!   a user, a principal with no assignment that holds the permission: all denied. Only the
//!   platform acting as itself (`is_system`) is allowed without a grant.
//! - **Declared names only.** A grant naming a permission nobody declares (the app is
//!   uninstalled or not yet installed) is kept, never allows anything, and is listed by
//!   [`Snapshot::orphan_app_grants`](crate::snapshot::Snapshot::orphan_app_grants).
//! - **Scope does not apply.** A named permission is not about a record, so an assignment's
//!   user-permission restrictions are not consulted. A permission that must be company-scoped
//!   should be an ordinary DocType right.
//! - **A denial never says who holds the permission.** The reasons of a denial name no role.
//! - **No cache, no clock, no I/O.** Every call reads the snapshot it is given.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::engine::PermissionEngine;
use crate::model::{Principal, Role};

/// The longest permission name, in bytes.
pub const MAX_APP_PERMISSION_NAME: usize = 64;

/// A permission name is not valid: the stable reason, never the offending text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("an app permission name {reason}")]
pub struct AppNameError {
    /// What is wrong, in words.
    pub reason: &'static str,
}

/// A validated app permission name: non-empty, at most 64 bytes, lowercase ASCII letters,
/// digits and `_`, starting with a letter (the alphabet of `spot_app_api::Name`, which
/// `MethodRight::Permission` carries). Compared exactly: `approve_note`, `approve_notes` and
/// `Approve_Note` are three names, and the first is not the second's prefix in any sense.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct AppPermissionName(String);

impl AppPermissionName {
    /// Validates `text` as a permission name.
    ///
    /// # Errors
    ///
    /// [`AppNameError`] for an empty name, one over 64 bytes, one starting with anything but a
    /// lowercase letter, or one holding anything but lowercase letters, digits and `_`.
    pub fn parse(text: &str) -> Result<AppPermissionName, AppNameError> {
        if text.is_empty() {
            return Err(AppNameError { reason: "is empty" });
        }
        if text.len() > MAX_APP_PERMISSION_NAME {
            return Err(AppNameError {
                reason: "is longer than 64 bytes",
            });
        }
        let mut bytes = text.bytes();
        if !bytes.next().is_some_and(|b| b.is_ascii_lowercase()) {
            return Err(AppNameError {
                reason: "does not start with a lowercase letter",
            });
        }
        if !bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_') {
            return Err(AppNameError {
                reason: "holds a character other than a lowercase letter, a digit or `_`",
            });
        }
        Ok(AppPermissionName(text.to_owned()))
    }

    /// The name as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AppPermissionName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for AppPermissionName {
    type Error = AppNameError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        AppPermissionName::parse(&text)
    }
}

impl From<AppPermissionName> for String {
    fn from(name: AppPermissionName) -> String {
        name.0
    }
}

/// The stable code of an invalid permission name at load.
pub const INVALID_NAME_CODE: &str = "perm.app_permission_invalid_name";
/// The stable code of a permission declared twice.
pub const DUPLICATE_CODE: &str = "perm.app_permission_duplicate";

/// Something failed while declaring or loading app permissions.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AppPermissionError {
    /// A name is not a valid [`AppPermissionName`].
    #[error("{at}: {source}")]
    InvalidName {
        /// Where the name came from.
        at: String,
        /// What is wrong with it.
        source: AppNameError,
    },
    /// Two apps declare the same permission name: one flat namespace per site.
    #[error("app permission `{name}` is declared by app `{first}` and again by app `{second}`")]
    Duplicate {
        /// The permission.
        name: String,
        /// The app that declared it first.
        first: String,
        /// The app that declared it again.
        second: String,
    },
    /// The document is not valid TOML or has a key the model does not accept.
    #[error("cannot read {at}: {detail}")]
    Parse {
        /// The document.
        at: String,
        /// What the parser objected to.
        detail: String,
    },
    /// A required value is missing.
    #[error("{at}: {detail}")]
    Missing {
        /// The row.
        at: String,
        /// What is missing.
        detail: String,
    },
}

impl AppPermissionError {
    /// The stable machine-readable code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidName { .. } => INVALID_NAME_CODE,
            Self::Duplicate { .. } => DUPLICATE_CODE,
            Self::Parse { .. } => crate::error::PARSE,
            Self::Missing { .. } => crate::error::MISSING,
        }
    }
}

/// What an app says about one permission it declares.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppPermissionDecl {
    /// The app that declares it.
    pub app: String,
    /// A short title for the role editor.
    pub title: String,
    /// What holding it lets a user do.
    pub description: String,
}

/// The permissions the installed apps of one site declare, by name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AppPermissionCatalogue {
    declared: BTreeMap<AppPermissionName, AppPermissionDecl>,
}

impl AppPermissionCatalogue {
    /// An empty catalogue: every app permission is unknown.
    #[must_use]
    pub fn new() -> AppPermissionCatalogue {
        AppPermissionCatalogue::default()
    }

    /// Declares `name`.
    ///
    /// # Errors
    ///
    /// [`AppPermissionError::Duplicate`], naming both apps, when `name` is already declared.
    /// The catalogue is unchanged.
    pub fn insert(
        &mut self,
        name: AppPermissionName,
        decl: AppPermissionDecl,
    ) -> Result<(), AppPermissionError> {
        if let Some(existing) = self.declared.get(&name) {
            return Err(AppPermissionError::Duplicate {
                name: name.0,
                first: existing.app.clone(),
                second: decl.app,
            });
        }
        self.declared.insert(name, decl);
        Ok(())
    }

    /// Whether `name` is declared.
    #[must_use]
    pub fn contains(&self, name: &AppPermissionName) -> bool {
        self.declared.contains_key(name)
    }

    /// What the declaring app said about `name`.
    #[must_use]
    pub fn get(&self, name: &AppPermissionName) -> Option<&AppPermissionDecl> {
        self.declared.get(name)
    }

    /// Every declaration, in name order.
    pub fn iter(&self) -> impl Iterator<Item = (&AppPermissionName, &AppPermissionDecl)> {
        self.declared.iter()
    }

    /// How many permissions are declared.
    #[must_use]
    pub fn len(&self) -> usize {
        self.declared.len()
    }

    /// Whether nothing is declared.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.declared.is_empty()
    }
}

/// A role holds an app permission. The grant is stored whether or not an app declares the
/// permission; only a declared one is ever effective.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct AppPermissionGrant {
    /// The role that holds it.
    pub role: Role,
    /// The permission.
    pub permission: AppPermissionName,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantFile {
    entries: Vec<GrantRow>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
struct GrantRow {
    role: String,
    permission: String,
}

/// Loads the app permission grants of a site from TOML (`[[entries]] role, permission`),
/// reporting every problem at once. An empty text is no grants. A name that is not a valid
/// [`AppPermissionName`] is an error (`perm.app_permission_invalid_name`), never a skipped
/// row. The rows come back sorted and deduplicated.
///
/// This is the loader of the `app_permissions` document of the site's permission model; it
/// is separate from [`crate::model::load`] so that [`crate::model::Model`] and
/// [`crate::model::ModelTexts`] keep their shape for every crate that builds them.
///
/// # Errors
///
/// Every [`AppPermissionError`] found, in row order.
pub fn load_app_grants(text: &str) -> Result<Vec<AppPermissionGrant>, Vec<AppPermissionError>> {
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    let file: GrantFile = toml::from_str(text).map_err(|e| {
        vec![AppPermissionError::Parse {
            at: "app_permissions".to_owned(),
            detail: e.to_string(),
        }]
    })?;
    let mut errors = Vec::new();
    let mut grants = BTreeSet::new();
    for (i, row) in file.entries.into_iter().enumerate() {
        let at = format!("app_permissions.entries[{i}]");
        let role = row.role.trim().to_owned();
        if role.is_empty() {
            errors.push(AppPermissionError::Missing {
                at: at.clone(),
                detail: "`role` is required".to_owned(),
            });
        }
        match AppPermissionName::parse(&row.permission) {
            Ok(permission) if !role.is_empty() => {
                grants.insert(AppPermissionGrant {
                    role: Role(role),
                    permission,
                });
            }
            Ok(_) => {}
            Err(source) => errors.push(AppPermissionError::InvalidName { at, source }),
        }
    }
    if errors.is_empty() {
        Ok(grants.into_iter().collect())
    } else {
        Err(errors)
    }
}

/// What a snapshot keeps about app permissions: the catalogue, who holds what (declared names
/// only) and the grants nobody declares.
#[derive(Clone, Debug, Default)]
pub(crate) struct AppIndex {
    catalogue: AppPermissionCatalogue,
    by_role: BTreeMap<Role, BTreeSet<AppPermissionName>>,
    orphans: Vec<(Role, AppPermissionName)>,
}

impl AppIndex {
    pub(crate) fn new(catalogue: &AppPermissionCatalogue, grants: &[AppPermissionGrant]) -> Self {
        let mut by_role: BTreeMap<Role, BTreeSet<AppPermissionName>> = BTreeMap::new();
        let mut orphans = BTreeSet::new();
        for grant in grants {
            if catalogue.contains(&grant.permission) {
                by_role
                    .entry(grant.role.clone())
                    .or_default()
                    .insert(grant.permission.clone());
            } else {
                orphans.insert((grant.role.clone(), grant.permission.clone()));
            }
        }
        AppIndex {
            catalogue: catalogue.clone(),
            by_role,
            orphans: orphans.into_iter().collect(),
        }
    }

    pub(crate) fn catalogue(&self) -> &AppPermissionCatalogue {
        &self.catalogue
    }

    pub(crate) fn orphans(&self) -> &[(Role, AppPermissionName)] {
        &self.orphans
    }

    pub(crate) fn role_holds(&self, role: &Role, name: &AppPermissionName) -> bool {
        self.by_role.get(role).is_some_and(|set| set.contains(name))
    }
}

/// Why an app permission was allowed or denied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AppReason {
    /// The platform acting as itself.
    System,
    /// A role the principal's assignment gives holds the permission.
    GrantedByRole {
        /// The role (the first match in assignment-id order).
        role: Role,
        /// The assignment that gives the role.
        assignment: String,
    },
    /// The permission is declared but none of the principal's roles holds it (also when the
    /// principal has no assignments or no user).
    NotGranted,
    /// No installed app declares the permission.
    Unknown,
    /// The name is not a valid permission name; nothing was looked up.
    InvalidName,
}

impl AppReason {
    /// The stable machine-readable code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::System => "perm.app.system",
            Self::GrantedByRole { .. } => "perm.app.granted",
            Self::NotGranted => "perm.app.not_granted",
            Self::Unknown => "perm.app.unknown",
            Self::InvalidName => "perm.app.invalid_name",
        }
    }

    /// The sentence for people. A denial never says which roles hold the permission.
    #[must_use]
    pub fn sentence(&self) -> String {
        match self {
            Self::System => "the platform acting as itself holds every app permission".to_owned(),
            Self::GrantedByRole { role, assignment } => format!(
                "granted by role `{}` through assignment `{assignment}`",
                role.as_str()
            ),
            Self::NotGranted => {
                "none of the principal's assignments gives a role that holds the permission"
                    .to_owned()
            }
            Self::Unknown => "no installed app declares this permission".to_owned(),
            Self::InvalidName => "the name is not a valid app permission name".to_owned(),
        }
    }
}

/// The outcome of one app permission question.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppDecision {
    /// Whether the principal holds the permission.
    pub allowed: bool,
    /// Why.
    pub reason: AppReason,
}

impl AppDecision {
    fn deny(reason: AppReason) -> AppDecision {
        AppDecision {
            allowed: false,
            reason,
        }
    }
}

impl PermissionEngine {
    /// Does `principal` hold the app permission `name`? Pure and synchronous: no I/O, no
    /// clock, no cache; the answer is a function of the snapshot and the arguments.
    ///
    /// 1. A system principal is allowed (`System`), whatever the name.
    /// 2. A name that is not a valid [`AppPermissionName`] is denied (`InvalidName`).
    /// 3. A name no installed app declares is denied (`Unknown`).
    /// 4. A principal with no user is denied (`NotGranted`).
    /// 5. Otherwise allowed iff **any** assignment gives a role that holds the permission
    ///    (`GrantedByRole`, the first match in assignment-id then role order), else denied
    ///    (`NotGranted`). The assignment's scope is not consulted.
    #[must_use]
    pub fn app_permission(&self, principal: &Principal, name: &str) -> AppDecision {
        if principal.is_system {
            return AppDecision {
                allowed: true,
                reason: AppReason::System,
            };
        }
        let Ok(name) = AppPermissionName::parse(name) else {
            return AppDecision::deny(AppReason::InvalidName);
        };
        let index = self.snapshot().app_index();
        if !index.catalogue().contains(&name) {
            return AppDecision::deny(AppReason::Unknown);
        }
        if principal.user.is_empty() {
            return AppDecision::deny(AppReason::NotGranted);
        }
        let mut first: Option<(&str, &Role)> = None;
        for assignment in &principal.assignments {
            for role in &assignment.roles {
                if !index.role_holds(role, &name) {
                    continue;
                }
                let candidate = (assignment.id.as_str(), role);
                if first.is_none_or(|best| candidate < best) {
                    first = Some(candidate);
                }
            }
        }
        match first {
            Some((assignment, role)) => AppDecision {
                allowed: true,
                reason: AppReason::GrantedByRole {
                    role: role.clone(),
                    assignment: assignment.to_owned(),
                },
            },
            None => AppDecision::deny(AppReason::NotGranted),
        }
    }

    /// The support view of one app permission decision: "why can this user approve notes"
    /// names the assignment and role; "why not" says unknown, invalid or not granted.
    #[must_use]
    pub fn explain_app_permission(&self, principal: &Principal, name: &str) -> String {
        let d = self.app_permission(principal, name);
        format!(
            "app permission `{name}` by {}: {}\n  - {}: {}\n",
            if principal.is_system {
                "system"
            } else {
                &principal.user
            },
            if d.allowed { "ALLOWED" } else { "DENIED" },
            d.reason.code(),
            d.reason.sentence()
        )
    }
}
