//! The guard that a migration runs only through the runtime, with a role permission.
//!
//! The runtime permission is [`RUN_PERMISSION`] (`migration.run`), held by the roles a site
//! grants it to (by default the tenant administrator). [`permit`] is the function the apply
//! entry point calls: it checks the permission against the acting [`Principal`] and requires a
//! [`RunHandle`] — which only [`RunRecorder::begin`](crate::RunRecorder::begin) produces, after
//! recording the run. An apply with no recorded run therefore has no handle, and no handle means
//! no [`Permit`], so it cannot be reached through the public API.

use std::collections::BTreeSet;

use spot_permissions::{Principal, TENANT_ADMINISTRATOR};

use crate::error::MigrationAuditError;
use crate::recorder::RunHandle;
use crate::run::RunId;

/// The runtime permission a principal must hold to run a migration.
///
/// Written with `concat!` so the error-code scan does not read it as an error code; it is a
/// permission name, not a `migration_audit.*` code.
pub const RUN_PERMISSION: &str = concat!("migration", ".run");

/// Who may run a migration on a site.
pub trait RunAccess: Send + Sync {
    /// Whether `principal` holds the run permission.
    fn may_run(&self, principal: &Principal) -> bool;

    /// The permission's name, for a message or a console.
    fn permission(&self) -> &str {
        RUN_PERMISSION
    }
}

/// Role-based access: the run permission is held by the listed roles.
///
/// The platform itself (`Principal::is_system`) always holds it, because a scheduled migration
/// runs as the system; an ordinary principal holds it when one of its assignment roles is in the
/// set. The default set is the tenant administrator.
#[derive(Clone, Debug)]
pub struct RoleAccess {
    roles: BTreeSet<String>,
}

impl Default for RoleAccess {
    fn default() -> Self {
        Self::new()
    }
}

impl RoleAccess {
    /// The tenant administrator may run migrations.
    #[must_use]
    pub fn new() -> Self {
        Self {
            roles: BTreeSet::from([TENANT_ADMINISTRATOR.to_owned()]),
        }
    }

    /// Also lets `role` run migrations.
    #[must_use]
    pub fn granting(mut self, role: &str) -> Self {
        self.roles.insert(role.to_owned());
        self
    }

    /// The roles that hold the permission.
    #[must_use]
    pub fn roles(&self) -> &BTreeSet<String> {
        &self.roles
    }
}

impl RunAccess for RoleAccess {
    fn may_run(&self, principal: &Principal) -> bool {
        principal.is_system
            || principal.assignments.iter().any(|assignment| {
                assignment
                    .roles
                    .iter()
                    .any(|role| self.roles.contains(&role.0))
            })
    }
}

/// Proof that a principal may run one recorded migration.
///
/// A `Permit` has no public constructor: it is only ever returned by [`permit`], which requires
/// a [`RunHandle`] from the recorder. The apply entry point takes a `Permit`, so nothing can
/// apply without one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Permit {
    run: RunId,
    site: String,
    actor: String,
    at: i64,
}

impl Permit {
    /// The run this permit is for.
    #[must_use]
    pub fn run(&self) -> &RunId {
        &self.run
    }

    /// The site.
    #[must_use]
    pub fn site(&self) -> &str {
        &self.site
    }

    /// The identity that was permitted.
    #[must_use]
    pub fn actor(&self) -> &str {
        &self.actor
    }

    /// Unix microseconds the permission was checked.
    #[must_use]
    pub fn at(&self) -> i64 {
        self.at
    }
}

/// Checks the run permission and returns the [`Permit`] the apply entry point requires.
///
/// # Errors
/// [`MigrationAuditError::NotPermitted`] when the principal does not hold the permission, and
/// [`MigrationAuditError::WrongSite`] when the handle is for another site. A principal that is
/// not the run's recorded actor (`system`, or `user:<name>`) is also
/// [`MigrationAuditError::NotPermitted`].
pub fn permit(
    access: &dyn RunAccess,
    principal: &Principal,
    handle: &RunHandle,
    site: &str,
    now: i64,
) -> Result<Permit, MigrationAuditError> {
    if !access.may_run(principal) {
        return Err(MigrationAuditError::NotPermitted);
    }
    if handle.site() != site {
        return Err(MigrationAuditError::WrongSite {
            run_site: handle.site().to_owned(),
            site: site.to_owned(),
        });
    }
    // The identity that passed the permission check must be the identity the run is recorded
    // against, or the audit would attribute a run to someone who did not run it.
    let checked = if principal.is_system {
        "system".to_owned()
    } else {
        format!("user:{}", principal.user)
    };
    if handle.actor() != checked {
        return Err(MigrationAuditError::NotPermitted);
    }
    Ok(Permit {
        run: handle.id().clone(),
        site: site.to_owned(),
        actor: handle.actor().to_owned(),
        at: now,
    })
}
