//! The gate that decides whether a principal may start a Case through a trigger.
//!
//! Two rules, both from `spot-permissions`:
//!
//! - a **document event** start runs as the event's actor; the actor must be able to **read** the
//!   document (`PermissionEngine::can` with [`Action::Read`]), so a trigger never starts a Case
//!   for an event its principal could not have read;
//! - a **manual** start requires one of the roles the trigger names; an empty role list means any
//!   signed-in person. An **API** start is authorised at the API boundary and reaches here with
//!   the same role rule (empty by default).
//!
//! The system principal (a job, the scheduler) always passes. A host that checks permissions
//! elsewhere injects [`AllowAll`].

use spot_permissions::{Action, DocFacts, PermissionEngine, Principal};

use crate::spec::TriggerSpec;

/// One gate question: who, through what, on which document.
#[derive(Debug)]
pub struct StartPermission<'a> {
    /// The principal the start runs as.
    pub principal: &'a Principal,
    /// The trigger node's spec.
    pub spec: &'a TriggerSpec,
    /// The document facts, for a document-event start.
    pub doc: Option<&'a DocFacts>,
}

/// Decides whether a principal may start a Case through a trigger.
pub trait StartGate: Send + Sync {
    /// Whether the request may start.
    fn may_start(&self, request: &StartPermission<'_>) -> bool;
}

/// A gate that allows everything: the system default, and the test bed for callers that check
/// permissions elsewhere.
#[derive(Debug, Clone, Copy, Default)]
pub struct AllowAll;

impl StartGate for AllowAll {
    fn may_start(&self, _request: &StartPermission<'_>) -> bool {
        true
    }
}

/// The permission gate over `spot-permissions`.
///
/// Built with [`PermissionGate::roles_only`] (no read engine; a document start is then denied for
/// a non-system principal) or [`PermissionGate::with_read_engine`] to enforce the document read
/// rule with a live [`PermissionEngine`].
#[derive(Debug, Clone)]
pub struct PermissionGate {
    engine: Option<PermissionEngine>,
}

impl PermissionGate {
    /// A gate that enforces the manual/API role rule only.
    #[must_use]
    pub fn roles_only() -> Self {
        Self { engine: None }
    }

    /// A gate that also enforces the document read rule with `engine`.
    #[must_use]
    pub fn with_read_engine(engine: PermissionEngine) -> Self {
        Self {
            engine: Some(engine),
        }
    }
}

impl StartGate for PermissionGate {
    fn may_start(&self, request: &StartPermission<'_>) -> bool {
        if request.principal.is_system {
            return true;
        }
        if let Some(doc) = request.doc {
            return match &self.engine {
                Some(engine) => engine.can(request.principal, Action::Read, doc).allowed,
                // Without a read engine the safe answer for a document start is "no".
                None => false,
            };
        }
        // A manual or API start is made by a signed-in person: an anonymous caller never starts.
        if matches!(request.spec, TriggerSpec::Manual(_) | TriggerSpec::Api(_))
            && request.principal.user.is_empty()
        {
            return false;
        }
        let roles = request.spec.roles();
        roles.is_empty() || roles.iter().any(|role| holds_role(request.principal, role))
    }
}

/// Whether `principal` holds `role` in any of its assignments.
#[must_use]
pub fn holds_role(principal: &Principal, role: &str) -> bool {
    principal
        .assignments
        .iter()
        .any(|assignment| assignment.roles.iter().any(|held| held.as_str() == role))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::spec::ManualSpec;
    use std::sync::Arc;

    fn manual(roles: &[&str]) -> TriggerSpec {
        TriggerSpec::Manual(ManualSpec {
            roles: roles.iter().map(|r| (*r).to_owned()).collect(),
            input_schema: None,
        })
    }

    fn request<'a>(
        principal: &'a Principal,
        spec: &'a TriggerSpec,
        doc: Option<&'a DocFacts>,
    ) -> StartPermission<'a> {
        StartPermission {
            principal,
            spec,
            doc,
        }
    }

    #[test]
    fn the_system_always_passes() {
        let gate = PermissionGate::roles_only();
        let spec = manual(&["Manager"]);
        assert!(gate.may_start(&request(&Principal::system(), &spec, None)));
    }

    #[test]
    fn a_manual_start_needs_one_of_the_named_roles() {
        let gate = PermissionGate::roles_only();
        let spec = manual(&["Manager", "Sales User"]);
        let sales = Principal::with_roles("u1", &["Sales User"]);
        let other = Principal::with_roles("u2", &["Viewer"]);
        assert!(gate.may_start(&request(&sales, &spec, None)));
        assert!(!gate.may_start(&request(&other, &spec, None)));
    }

    #[test]
    fn an_empty_role_list_allows_any_person() {
        let gate = PermissionGate::roles_only();
        let spec = manual(&[]);
        let person = Principal::with_roles("u1", &[]);
        assert!(gate.may_start(&request(&person, &spec, None)));
    }

    #[test]
    fn a_document_start_without_a_read_engine_is_denied_for_a_person() {
        let gate = PermissionGate::roles_only();
        let spec = manual(&[]);
        let doc = DocFacts {
            doctype: "sales_order".into(),
            id: Some("d1".into()),
            ..DocFacts::default()
        };
        let person = Principal::with_roles("u1", &["Manager"]);
        assert!(!gate.may_start(&request(&person, &spec, Some(&doc))));
    }

    #[test]
    fn a_document_start_with_a_read_engine_denies_a_principal_that_cannot_read() {
        // An empty metadata has no grants for `sales_order`, so `can(Read)` is denied; the
        // system principal still passes. This proves the read rule is wired to the engine.
        let registry = spot_meta::Registry::new(Vec::new()).unwrap();
        let snapshot = spot_permissions::Snapshot::empty(&registry.effective()).unwrap();
        let gate = PermissionGate::with_read_engine(PermissionEngine::new(Arc::new(snapshot)));
        let spec = manual(&[]);
        let doc = DocFacts {
            doctype: "sales_order".into(),
            id: Some("d1".into()),
            ..DocFacts::default()
        };
        let person = Principal::with_roles("u1", &["Manager"]);
        assert!(!gate.may_start(&request(&person, &spec, Some(&doc))));
        assert!(gate.may_start(&request(&Principal::system(), &spec, Some(&doc))));
    }

    #[test]
    fn an_anonymous_caller_never_starts_manually_or_by_api() {
        let gate = PermissionGate::roles_only();
        let spec = manual(&[]);
        let anonymous = Principal::with_roles("", &[]);
        assert!(!gate.may_start(&request(&anonymous, &spec, None)));
        let api = TriggerSpec::Api(crate::spec::ApiSpec {
            name: "start".into(),
            roles: Vec::new(),
            input_schema: None,
        });
        assert!(!gate.may_start(&request(&anonymous, &api, None)));
    }

    #[test]
    fn an_api_start_needs_one_of_the_named_roles() {
        let gate = PermissionGate::roles_only();
        let api = TriggerSpec::Api(crate::spec::ApiSpec {
            name: "start".into(),
            roles: vec!["Integrator".into()],
            input_schema: None,
        });
        let allowed = Principal::with_roles("svc", &["Integrator"]);
        let other = Principal::with_roles("u2", &["Viewer"]);
        assert!(gate.may_start(&request(&allowed, &api, None)));
        assert!(!gate.may_start(&request(&other, &api, None)));
    }

    #[test]
    fn allow_all_allows_everything() {
        let gate = AllowAll;
        let spec = manual(&["Manager"]);
        let person = Principal::with_roles("u1", &[]);
        assert!(gate.may_start(&request(&person, &spec, None)));
    }
}
