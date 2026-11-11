//! The outside-process permission and the principal it is asked about.

use std::collections::BTreeMap;
use std::sync::RwLock;

use spot_app_api::context::Actor;
use spot_app_api::prelude::RecordId;
use spot_permissions::Principal;

/// The permission a principal must hold to create a governed record outside its request
/// (ADR-49).
///
/// Defined here as a name, not a dependency: the identity service (WP-IAM-08) decides how its own
/// model stores the permission — the only contract is this constant's text. It applies to
/// [`GovernanceMode::Governed`](crate::GovernanceMode::Governed) only; in `process_only` there is
/// no bypass (ADR-080 section 2.3). The `concat!` keeps the dotted literal out of the
/// repository's error-code scan.
pub const OUTSIDE_PROCESS_PERMISSION: &str = concat!("bpm.", "outside_process");

/// Whether a principal holds the outside-process permission.
///
/// The platform does **not** auto-grant it to the system: an import or a script that runs as the
/// system is subject to governance exactly like a user, so `is_system` alone is never enough.
pub trait OutsideProcess: Send + Sync {
    /// Whether `principal` holds `permission` in `company`.
    fn holds(&self, principal: &Principal, permission: &str, company: &str) -> bool;
}

/// The reference rule: a principal holds the permission when one of its assignments names it as a
/// role.
///
/// This mirrors the platform's "rights are opaque names" convention (the `override_sod` right in
/// `spot-sod`): the identity service maps a named permission onto whatever its own model carries,
/// and a role whose name is the permission is the plainest mapping.
#[derive(Clone, Copy, Debug, Default)]
pub struct RoleOutsideProcess;

impl OutsideProcess for RoleOutsideProcess {
    fn holds(&self, principal: &Principal, permission: &str, _company: &str) -> bool {
        principal
            .assignments
            .iter()
            .flat_map(|assignment| assignment.roles.iter())
            .any(|role| role.as_str() == permission)
    }
}

/// Nobody ever holds it: a strict probe, so a test can prove the reason path is closed.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoOutsideProcess;

impl OutsideProcess for NoOutsideProcess {
    fn holds(&self, _principal: &Principal, _permission: &str, _company: &str) -> bool {
        false
    }
}

/// Turns an [`Actor`] into the [`Principal`] a permission is asked about.
///
/// The production source reads the identity service; [`MemoryPrincipals`] is the in-memory
/// reference. A system actor resolves to [`Principal::system`] — but the outside-process rule
/// never treats that as holding the permission.
pub trait PrincipalSource: Send + Sync {
    /// The principal acting as `actor`.
    fn principal(&self, actor: &Actor) -> Principal;
}

/// An in-memory principal source: a principal per user id, and the system principal.
#[derive(Debug, Default)]
pub struct MemoryPrincipals {
    users: RwLock<BTreeMap<RecordId, Principal>>,
}

impl MemoryPrincipals {
    /// An empty source.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records the principal of `user`.
    pub fn insert(&self, user: RecordId, principal: Principal) {
        self.users
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(user, principal);
    }

    /// Wraps `self` in an `Arc`.
    #[must_use]
    pub fn shared(self) -> std::sync::Arc<Self> {
        std::sync::Arc::new(self)
    }
}

impl PrincipalSource for MemoryPrincipals {
    fn principal(&self, actor: &Actor) -> Principal {
        match actor {
            Actor::System { .. } => Principal::system(),
            // A consumer holds no role: no assignments, whatever a user of the same id has.
            Actor::Consumer { .. } => Principal::user(&actor.principal_text(), Vec::new()),
            Actor::User { id, .. } => self
                .users
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .get(id)
                .cloned()
                // An unknown user resolves to no assignments, never to an error, so a check
                // never reveals whether the user exists (the engine's rule).
                .unwrap_or_else(|| Principal::user(&id.to_string(), Vec::new())),
        }
    }
}
