//! The environment model: roles, members, the set's linking rules and its store.
//!
//! An [`EnvironmentSet`] is the platform's view of one product's ladder: a development tenant,
//! a test tenant and a production tenant, in that order, sharing one app set and one region
//! class. The members are *linked*: [`EnvironmentSet::link`] resolves each one through the
//! control store's tenant registry, so a set can never point at a tenant the registry does not
//! know or one that lives in another region.

use std::collections::BTreeSet;

use spot_tenancy::{ControlStore, Region, Tenant, TenantId, TenantState};

use crate::error::{EnvironmentError, Result, codes};

/// A member's place in the promotion ladder.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Role {
    /// The environment where changes are authored.
    Dev,
    /// The environment where they are tested.
    Test,
    /// The environment that serves users.
    Prod,
}

impl Role {
    /// Every role, in promotion order.
    pub const ALL: [Role; 3] = [Role::Dev, Role::Test, Role::Prod];

    /// The role's rank: `Dev` is 0, `Test` 1 and `Prod` 2.
    #[must_use]
    pub fn rank(self) -> u8 {
        match self {
            Self::Dev => 0,
            Self::Test => 1,
            Self::Prod => 2,
        }
    }

    /// The role's stable lower-case name, for records and messages.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Dev => "dev",
            Self::Test => "test",
            Self::Prod => "prod",
        }
    }
}

/// One app and the version it is installed at, in the set's shared app set.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct AppRef {
    /// The app's name.
    pub name: String,
    /// The app's version, as the manifest states it.
    pub version: String,
}

impl AppRef {
    /// An app reference; neither part may be empty.
    ///
    /// # Errors
    /// [`codes::INVALID_SET`] when the name or version is empty.
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Result<Self> {
        let name = name.into();
        let version = version.into();
        if name.is_empty() || version.is_empty() {
            return Err(EnvironmentError::new(
                codes::INVALID_SET,
                "an app reference needs a non-empty name and version",
            ));
        }
        Ok(Self { name, version })
    }
}

/// One environment: a tenant, its role and its position in the ladder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Environment {
    /// The tenant that holds this environment's data.
    pub tenant: TenantId,
    /// The role the tenant plays.
    pub role: Role,
    /// The promotion position; strictly increasing across the set.
    pub order: u64,
}

/// The promotion policy of a set.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Policy {
    /// Whether a promotion may skip a member (dev directly to prod). Off by default.
    pub allow_skip: bool,
}

/// Development, test and production as one linked ladder over a shared app set and region.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvironmentSet {
    name: String,
    region: Region,
    apps: Vec<AppRef>,
    members: Vec<Environment>,
    policy: Policy,
}

impl EnvironmentSet {
    /// Builds and validates a set.
    ///
    /// # Errors
    /// [`codes::INVALID_SET`] or [`codes::INVALID_MEMBER`] when a rule does not hold; see
    /// [`EnvironmentSet::validate`].
    pub fn new(
        name: impl Into<String>,
        region: Region,
        apps: Vec<AppRef>,
        members: Vec<Environment>,
        policy: Policy,
    ) -> Result<Self> {
        let set = Self {
            name: name.into(),
            region,
            apps,
            members,
            policy,
        };
        set.validate()?;
        Ok(set)
    }

    /// The set's name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The region class every member's tenant must live in.
    #[must_use]
    pub fn region(&self) -> &Region {
        &self.region
    }

    /// The app set every member must share, sorted by name.
    #[must_use]
    pub fn apps(&self) -> &[AppRef] {
        &self.apps
    }

    /// The members, sorted by order.
    #[must_use]
    pub fn members(&self) -> &[Environment] {
        &self.members
    }

    /// The promotion policy.
    #[must_use]
    pub fn policy(&self) -> Policy {
        self.policy
    }

    /// Validates the set's shape: a name and app set, unique members, no repeated role, and a
    /// strictly increasing ladder that starts at `Dev` and skips no role unless the policy
    /// allows it.
    ///
    /// # Errors
    /// [`codes::INVALID_SET`] or [`codes::INVALID_MEMBER`] naming the first broken rule.
    pub fn validate(&self) -> Result<()> {
        if self.name.is_empty() || self.name.len() > 63 {
            return Err(EnvironmentError::new(
                codes::INVALID_SET,
                "a set name has 1 to 63 characters",
            ));
        }
        if !self
            .name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        {
            return Err(EnvironmentError::new(
                codes::INVALID_SET,
                "a set name uses lower-case letters, digits, '-' and '_' only",
            ));
        }
        if self.apps.is_empty() {
            return Err(EnvironmentError::new(
                codes::INVALID_SET,
                "a set shares at least one app",
            ));
        }
        let mut app_names = BTreeSet::new();
        let mut previous_app: Option<&AppRef> = None;
        for app in &self.apps {
            if app.name.is_empty() || app.version.is_empty() {
                return Err(EnvironmentError::new(
                    codes::INVALID_SET,
                    "every app reference needs a name and a version",
                ));
            }
            if !app_names.insert(app.name.as_str()) {
                return Err(EnvironmentError::about(
                    codes::INVALID_SET,
                    &app.name,
                    "an app appears twice in the set's app set",
                ));
            }
            if let Some(previous) = previous_app
                && previous >= app
            {
                return Err(EnvironmentError::new(
                    codes::INVALID_SET,
                    "the app set must be sorted by name",
                ));
            }
            previous_app = Some(app);
        }
        if self.members.is_empty() {
            return Err(EnvironmentError::new(
                codes::INVALID_SET,
                "a set has at least one member",
            ));
        }
        let mut tenants = BTreeSet::new();
        let mut roles = BTreeSet::new();
        let mut previous: Option<&Environment> = None;
        for member in &self.members {
            if !tenants.insert(member.tenant) {
                return Err(EnvironmentError::about(
                    codes::INVALID_MEMBER,
                    member.tenant.to_string(),
                    "a tenant appears twice in the set",
                ));
            }
            if !roles.insert(member.role) {
                return Err(EnvironmentError::about(
                    codes::INVALID_MEMBER,
                    member.role.name(),
                    "a role appears twice in the set",
                ));
            }
            match previous {
                None => {
                    if member.role != Role::Dev {
                        return Err(EnvironmentError::about(
                            codes::INVALID_MEMBER,
                            member.role.name(),
                            "a ladder starts at dev",
                        ));
                    }
                }
                Some(previous) => {
                    if member.order <= previous.order {
                        return Err(EnvironmentError::about(
                            codes::INVALID_MEMBER,
                            member.tenant.to_string(),
                            "members are ordered by a strictly increasing order",
                        ));
                    }
                    if member.role.rank() <= previous.role.rank() {
                        return Err(EnvironmentError::about(
                            codes::INVALID_MEMBER,
                            member.role.name(),
                            "roles increase from dev to test to prod",
                        ));
                    }
                    let skipped = member.role.rank() > previous.role.rank() + 1;
                    if skipped && !self.policy.allow_skip {
                        return Err(EnvironmentError::about(
                            codes::INVALID_MEMBER,
                            member.role.name(),
                            "the ladder skips a role and the policy does not allow it",
                        ));
                    }
                }
            }
            previous = Some(member);
        }
        Ok(())
    }

    /// The member at this tenant, if the tenant belongs to the set.
    #[must_use]
    pub fn member(&self, tenant: &TenantId) -> Option<&Environment> {
        self.members.iter().find(|member| &member.tenant == tenant)
    }

    /// The member for a role, if the set has one.
    #[must_use]
    pub fn member_for_role(&self, role: Role) -> Option<&Environment> {
        self.members.iter().find(|member| member.role == role)
    }

    /// Resolves every member through the control store and checks its region, app set and
    /// state.
    ///
    /// # Errors
    /// [`codes::UNKNOWN_MEMBER`] when the registry does not know a tenant,
    /// [`codes::LINK_MISMATCH`] for a region or app-set difference, and
    /// [`codes::TENANT_NOT_ACTIVE`] for a tenant that cannot receive a bundle.
    pub fn link(&self, store: &dyn ControlStore, apps: &[AppRef]) -> Result<()> {
        for member in &self.members {
            let tenant = store.get(&member.tenant).map_err(|_| {
                EnvironmentError::about(
                    codes::UNKNOWN_MEMBER,
                    member.tenant.to_string(),
                    "the tenant registry does not know this member",
                )
            })?;
            self.check_link(&tenant, apps)?;
        }
        Ok(())
    }

    /// Checks one tenant against the set's linking rules: the tenant is a member, lives in the
    /// set's region, is active, and its app set is the set's.
    ///
    /// # Errors
    /// [`codes::UNKNOWN_MEMBER`], [`codes::LINK_MISMATCH`] or [`codes::TENANT_NOT_ACTIVE`].
    pub fn check_link(&self, tenant: &Tenant, apps: &[AppRef]) -> Result<()> {
        if self.member(&tenant.id).is_none() {
            return Err(EnvironmentError::about(
                codes::UNKNOWN_MEMBER,
                tenant.id.to_string(),
                "the tenant is not a member of this set",
            ));
        }
        if tenant.region != self.region {
            return Err(EnvironmentError::about(
                codes::LINK_MISMATCH,
                tenant.id.to_string(),
                "the tenant lives in another region class than the set",
            ));
        }
        if tenant.state != TenantState::Active {
            return Err(EnvironmentError::about(
                codes::TENANT_NOT_ACTIVE,
                tenant.id.to_string(),
                "the tenant is not active",
            ));
        }
        if apps != self.apps.as_slice() {
            return Err(EnvironmentError::about(
                codes::LINK_MISMATCH,
                tenant.id.to_string(),
                "the tenant's app set differs from the set's",
            ));
        }
        Ok(())
    }

    /// Checks that a promotion may go from `from` to `to`: both are members, the target is
    /// later in the ladder, and it is the next member unless the policy allows a skip.
    ///
    /// # Errors
    /// [`codes::UNKNOWN_MEMBER`], [`codes::INVALID_MEMBER`] or [`codes::SKIP_REFUSED`].
    pub fn route(&self, from: &Environment, to: &Environment) -> Result<()> {
        let Some(from_index) = self
            .members
            .iter()
            .position(|member| member.tenant == from.tenant && member.role == from.role)
        else {
            return Err(EnvironmentError::about(
                codes::UNKNOWN_MEMBER,
                from.tenant.to_string(),
                "the source environment is not a member of this set",
            ));
        };
        let Some(to_index) = self
            .members
            .iter()
            .position(|member| member.tenant == to.tenant && member.role == to.role)
        else {
            return Err(EnvironmentError::about(
                codes::UNKNOWN_MEMBER,
                to.tenant.to_string(),
                "the target environment is not a member of this set",
            ));
        };
        if to_index <= from_index {
            return Err(EnvironmentError::about(
                codes::INVALID_MEMBER,
                to.tenant.to_string(),
                "a promotion only moves forward along the ladder",
            ));
        }
        if to_index > from_index + 1 && !self.policy.allow_skip {
            return Err(EnvironmentError::about(
                codes::SKIP_REFUSED,
                to.tenant.to_string(),
                "a promotion may not skip an environment unless the policy allows it",
            ));
        }
        Ok(())
    }
}
