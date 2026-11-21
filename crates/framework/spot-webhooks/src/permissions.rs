//! The webhook owner's read rights, computed with the permission core at delivery time.
//!
//! A delivery includes a field only when [`FieldVisibility::may_read`] says the webhook's owner
//! may read it. The [`PermissionFieldVisibility`] adapter answers that from a
//! [`spot_permissions::PermissionEngine`] snapshot; a protected field nobody granted read is
//! omitted, and a field the owner may only see masked is omitted too (the real value never
//! leaves the server).

use std::collections::BTreeMap;

use spot_permissions::{DocFacts, FieldLevel, PermissionEngine, Principal};

/// Whether a webhook's owner may read a field of a DocType.
pub trait FieldVisibility: Send + Sync {
    /// Whether the webhook owner may read `field` on `doctype`.
    fn may_read(&self, doctype: &str, field: &str) -> bool;
}

/// A visibility that reads every field: the system principal and tests.
#[derive(Debug, Default, Clone, Copy)]
pub struct AllFieldsReadable;

impl FieldVisibility for AllFieldsReadable {
    fn may_read(&self, _doctype: &str, _field: &str) -> bool {
        true
    }
}

/// The real adapter: the webhook owner's field access from the permission engine.
#[derive(Clone, Debug)]
pub struct PermissionFieldVisibility {
    engine: PermissionEngine,
    principal: Principal,
}

impl PermissionFieldVisibility {
    /// A visibility for `principal` over `engine`'s snapshot.
    pub fn new(engine: PermissionEngine, principal: Principal) -> Self {
        Self { engine, principal }
    }

    /// A visibility for the platform acting as itself (every field readable).
    pub fn system(engine: PermissionEngine) -> Self {
        Self {
            engine,
            principal: Principal::system(),
        }
    }
}

impl FieldVisibility for PermissionFieldVisibility {
    fn may_read(&self, doctype: &str, field: &str) -> bool {
        let Some(facts) = self.engine.snapshot().facts(doctype) else {
            return false;
        };
        let Some(field_facts) = facts.field(field) else {
            return false;
        };
        let doc = DocFacts {
            doctype: doctype.to_owned(),
            values: BTreeMap::new(),
            ..DocFacts::default()
        };
        let decision = self
            .engine
            .field_access(&self.principal, doctype, field_facts, &doc);
        matches!(decision.level, FieldLevel::Read | FieldLevel::ReadReveal)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::sync::Arc;

    use spot_meta::{Definition, FieldSpec, FieldType, ProtectionGroupSpec, Registry};
    use spot_permissions::{
        FieldFacts, FieldGrant, FieldRight, FieldTarget, GroupName, Model, ProtectionGroup, Right,
        Rights, Role, RoleGrant, Snapshot,
    };

    fn field(name: &str, ty: FieldType) -> FieldSpec {
        FieldSpec::new(name, ty)
    }

    fn definition() -> Definition {
        let mut d = Definition::new("task");
        let mut salary = field("salary", FieldType::Decimal);
        salary.scale = Some(2);
        salary.protection_group = Some("financial".into());
        d.fields = vec![field("status", FieldType::Text), salary];
        d.protection_groups = vec![ProtectionGroupSpec::new("financial", "Financial")];
        d
    }

    fn snapshot() -> Snapshot {
        let meta = Registry::new(vec![definition()]).unwrap().effective();
        let model = Model {
            grants: vec![
                RoleGrant {
                    doctype: "task".into(),
                    role: Role("Reader".into()),
                    rights: Rights::new([Right::Read]),
                    if_owner: false,
                },
                RoleGrant {
                    doctype: "task".into(),
                    role: Role("Viewer".into()),
                    rights: Rights::new([Right::Read]),
                    if_owner: false,
                },
            ],
            field_grants: vec![FieldGrant {
                doctype: "task".into(),
                role: Role("Reader".into()),
                target: FieldTarget::Field("salary".into()),
                access: BTreeSet::from([FieldRight::Read]),
                deny: false,
                condition: None,
            }],
            groups: vec![ProtectionGroup {
                name: GroupName("financial".into()),
                trace: false,
            }],
            shares: vec![],
        };
        let snap = Snapshot::build(&meta, &model).unwrap();
        snap.with_field_facts(
            "task",
            vec![
                FieldFacts::plain("status"),
                FieldFacts {
                    path: "salary".into(),
                    groups: vec![GroupName("financial".into())],
                    ..FieldFacts::default()
                },
            ],
        )
        .unwrap()
    }

    fn engine() -> PermissionEngine {
        PermissionEngine::new(Arc::new(snapshot()))
    }

    #[test]
    fn a_granted_protected_field_is_readable() {
        let visibility =
            PermissionFieldVisibility::new(engine(), Principal::with_roles("u1", &["Reader"]));
        assert!(visibility.may_read("task", "status"));
        assert!(visibility.may_read("task", "salary"));
    }

    /// A role with the DocType read right but no grant on the protection group sees the plain
    /// field only: the protected one is closed by default.
    #[test]
    fn an_ungranted_protected_field_is_hidden() {
        let visibility =
            PermissionFieldVisibility::new(engine(), Principal::with_roles("u2", &["Viewer"]));
        assert!(visibility.may_read("task", "status"));
        assert!(!visibility.may_read("task", "salary"));
    }

    #[test]
    fn a_principal_without_read_sees_nothing() {
        let visibility = PermissionFieldVisibility::new(engine(), Principal::with_roles("u3", &[]));
        assert!(!visibility.may_read("task", "status"));
        assert!(!visibility.may_read("task", "salary"));
    }

    #[test]
    fn the_system_principal_sees_everything() {
        let visibility = PermissionFieldVisibility::system(engine());
        assert!(visibility.may_read("task", "salary"));
    }
}
