//! The real guards behind the admin: the permission engine answers the discard permission
//! ([`EngineCloseGuard`]) and the field guard answers what a principal may read
//! ([`FieldGuardScope`]). Both **fail closed**: no principal for the actor, an unknown name or a
//! guard error is a refusal, and the reason code is logged (a code, never a value).

use std::sync::Arc;

use spot_field_guard::FieldGuard;
use spot_permissions::{Action, DocFacts, PermissionEngine, Principal};

use super::{CloseGuard, DISCARD_PERMISSION, ReadScope};

/// Who an actor is, for the engine: the principal (user and assignments) the session resolved.
/// `None` means the actor is not known: every check then refuses.
pub trait PrincipalSource: Send + Sync {
    /// The principal of `actor`.
    fn principal(&self, actor: &str) -> Option<Principal>;
}

/// [`CloseGuard`] over the permission engine's **named app permission** check
/// (`PermissionEngine::app_permission`), so `sync.quarantine.discard` is granted by a role and
/// nothing else. An unknown permission name, an actor with no principal and a name the installed
/// apps do not declare are all refusals (`Err`), never an allow.
pub struct EngineCloseGuard {
    engine: Arc<PermissionEngine>,
    principals: Arc<dyn PrincipalSource>,
}

impl EngineCloseGuard {
    /// A guard over `engine`.
    pub fn new(engine: Arc<PermissionEngine>, principals: Arc<dyn PrincipalSource>) -> Self {
        Self { engine, principals }
    }
}

impl CloseGuard for EngineCloseGuard {
    fn allows(&self, actor: &str, permission: &str) -> Result<bool, String> {
        if permission != DISCARD_PERMISSION {
            return Err("the close guard knows no such permission".to_owned());
        }
        let principal = self
            .principals
            .principal(actor)
            .ok_or_else(|| "no principal is known for the actor".to_owned())?;
        // The engine names a permission with lowercase letters, digits and `_` only: the dotted
        // name the platform documents is the same name with `_` for `.` (to be declared among the
        // platform permissions; until then the engine answers Unknown and the guard refuses).
        let name = permission.replace('.', "_");
        let decision = self.engine.app_permission(&principal, &name);
        if !decision.allowed {
            tracing::info!(reason = ?decision.reason, "quarantine: the discard permission was refused");
        }
        Ok(decision.allowed)
    }
}

/// [`ReadScope`] over the field guard: a DocType is readable when the engine allows `read` on
/// it, a field when the guard's `visible_names` lists it (a hidden field's name is not told).
pub struct FieldGuardScope {
    guard: FieldGuard,
    principals: Arc<dyn PrincipalSource>,
}

impl FieldGuardScope {
    /// A scope over `guard`.
    pub fn new(guard: FieldGuard, principals: Arc<dyn PrincipalSource>) -> Self {
        Self { guard, principals }
    }

    fn facts(doctype: &str) -> DocFacts {
        DocFacts {
            doctype: doctype.to_owned(),
            ..DocFacts::default()
        }
    }
}

impl ReadScope for FieldGuardScope {
    fn may_read(&self, actor: &str, doctype: &str) -> bool {
        let Some(principal) = self.principals.principal(actor) else {
            return false;
        };
        let Some(read) = Action::parse("read") else {
            return false;
        };
        self.guard
            .engine()
            .can(&principal, read, &Self::facts(doctype))
            .allowed
    }

    fn may_read_field(&self, actor: &str, doctype: &str, field: &str) -> bool {
        let Some(principal) = self.principals.principal(actor) else {
            return false;
        };
        match self.guard.visible_names(
            &principal,
            doctype,
            &Self::facts(doctype),
            &[field.to_owned()],
        ) {
            Ok(visible) => visible.iter().any(|name| name == field),
            Err(error) => {
                // Fail closed: a guard error hides the field. The code is kept; no value is.
                tracing::info!(
                    code = error.code(),
                    "quarantine: a field was hidden after a guard error"
                );
                false
            }
        }
    }
}
