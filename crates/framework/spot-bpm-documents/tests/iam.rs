//! The IAM-backed service-principal seams and the publish hook, over a fake directory and a real
//! permission engine.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use std::sync::{Arc, Mutex};

use common::site;
use serde_json::json;
use spot_app_api::prelude::SiteId;
use spot_bpm_documents::service_principal::PUBLISH_PERMISSION;
use spot_bpm_documents::{
    AuthorPrincipals, IamPublishPermission, IamServicePrincipals, ProcessAuthor, PublishGuard,
    PublishPermissions, ServicePrincipalDirectory, ServicePrincipalEntry, ServicePrincipals,
    Unavailable, codes, engine_permission_name, validate_process_version,
};
use spot_dag::{CompiledDag, DagDefinition, JoinRule, Limits, Node, NodeId};
use spot_meta::{Definition, FieldSpec, FieldType, Registry};
use spot_permissions::model::{AssignmentContext, Model, Principal, Role};
use spot_permissions::{
    AppPermissionCatalogue, AppPermissionDecl, AppPermissionGrant, AppPermissionName,
    PermissionEngine, Snapshot,
};

#[derive(Default)]
struct FakeDirectory {
    entries: Mutex<Vec<(SiteId, ServicePrincipalEntry)>>,
    broken: Mutex<bool>,
}

impl ServicePrincipalDirectory for FakeDirectory {
    fn service_principals(
        &self,
        tenant: &SiteId,
    ) -> Result<Vec<ServicePrincipalEntry>, Unavailable> {
        if *self.broken.lock().unwrap() {
            return Err(Unavailable);
        }
        Ok(self
            .entries
            .lock()
            .unwrap()
            .iter()
            .filter(|(t, _)| t == tenant)
            .map(|(_, e)| e.clone())
            .collect())
    }
}

struct FakeAuthors {
    broken: bool,
}

impl AuthorPrincipals for FakeAuthors {
    fn principal_of(
        &self,
        _tenant: &SiteId,
        author: &ProcessAuthor,
    ) -> Result<Option<Principal>, Unavailable> {
        if self.broken {
            return Err(Unavailable);
        }
        Ok(match author {
            ProcessAuthor::User(id) => Some(Principal::user(
                id,
                vec![AssignmentContext::unscoped(
                    "a1",
                    if id == "alice" {
                        &["publisher"]
                    } else {
                        &["clerk"]
                    },
                )],
            )),
            _ => None,
        })
    }
}

fn alice() -> ProcessAuthor {
    ProcessAuthor::User("alice".to_owned())
}
fn bob() -> ProcessAuthor {
    ProcessAuthor::User("bob".to_owned())
}

fn engine() -> Arc<PermissionEngine> {
    let mut definition = Definition::new("note");
    definition.fields = vec![FieldSpec::new("text", FieldType::Text)];
    let meta = Registry::new(vec![definition]).unwrap().effective();
    let mut catalogue = AppPermissionCatalogue::new();
    catalogue
        .insert(
            AppPermissionName::parse(&engine_permission_name(PUBLISH_PERMISSION)).unwrap(),
            AppPermissionDecl {
                app: "bpm".to_owned(),
                title: "Publish".to_owned(),
                description: "Publish a service-principal process".to_owned(),
            },
        )
        .unwrap();
    let grants = [AppPermissionGrant {
        role: Role("publisher".to_owned()),
        permission: AppPermissionName::parse(&engine_permission_name(PUBLISH_PERMISSION)).unwrap(),
    }];
    let snapshot =
        Snapshot::build_with_apps(&meta, &Model::default(), &catalogue, &grants).unwrap();
    Arc::new(PermissionEngine::new(Arc::new(snapshot)))
}

fn directory() -> Arc<FakeDirectory> {
    let directory = FakeDirectory::default();
    directory.entries.lock().unwrap().push((
        site(),
        ServicePrincipalEntry {
            name: "svc_ledger".to_owned(),
            grants: vec![alice(), bob()],
        },
    ));
    Arc::new(directory)
}

fn guard(directory: Arc<FakeDirectory>, authors_broken: bool) -> PublishGuard {
    PublishGuard::new(
        Arc::new(IamServicePrincipals::new(directory)),
        Arc::new(IamPublishPermission::new(
            site(),
            engine(),
            Arc::new(FakeAuthors {
                broken: authors_broken,
            }),
        )),
    )
}

fn dag(principals: &[(&str, Option<&str>)]) -> CompiledDag {
    let nodes = principals
        .iter()
        .map(|(id, principal)| {
            let config = principal.map_or(json!({}), |p| json!({ "principal": p }));
            Node::new(NodeId::new(id).unwrap(), "document", JoinRule::All, config).unwrap()
        })
        .collect();
    CompiledDag::compile(
        &DagDefinition::new("order", 1, nodes, vec![]),
        &Limits::default(),
    )
    .unwrap()
}

#[test]
fn a_registered_granted_published_principal_passes() {
    let g = guard(directory(), false);
    let v = validate_process_version(&g, &site(), &alice(), &dag(&[("a", Some("svc_ledger"))]));
    assert!(v.is_empty(), "{v:?}");
}

#[test]
fn an_unregistered_principal_is_refused() {
    let g = guard(directory(), false);
    let v = validate_process_version(&g, &site(), &alice(), &dag(&[("a", Some("svc_other"))]));
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].code, codes::PRINCIPAL_NOT_REGISTERED);
}

#[test]
fn an_author_without_the_permission_is_refused() {
    let g = guard(directory(), false);
    let v = validate_process_version(&g, &site(), &bob(), &dag(&[("a", Some("svc_ledger"))]));
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].code, codes::PRINCIPAL_NOT_PERMITTED);
}

#[test]
fn an_author_missing_from_the_grant_list_is_refused() {
    let directory = Arc::new(FakeDirectory::default());
    directory.entries.lock().unwrap().push((
        site(),
        ServicePrincipalEntry {
            name: "svc_ledger".to_owned(),
            grants: vec![bob()],
        },
    ));
    let g = guard(directory, false);
    let v = validate_process_version(&g, &site(), &alice(), &dag(&[("a", Some("svc_ledger"))]));
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].code, codes::PRINCIPAL_NOT_PERMITTED);
}

#[test]
fn a_directory_failure_refuses_every_node() {
    let directory = directory();
    *directory.broken.lock().unwrap() = true;
    let g = guard(directory, false);
    let v = validate_process_version(&g, &site(), &alice(), &dag(&[("a", Some("svc_ledger"))]));
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].code, codes::PRINCIPAL_NOT_REGISTERED);
}

#[test]
fn an_identity_failure_refuses_and_an_unknown_author_holds_nothing() {
    let g = guard(directory(), true);
    let v = validate_process_version(&g, &site(), &alice(), &dag(&[("a", Some("svc_ledger"))]));
    assert!(v.iter().any(|x| x.code == codes::PRINCIPAL_NOT_PERMITTED));
    let permission =
        IamPublishPermission::new(site(), engine(), Arc::new(FakeAuthors { broken: false }));
    let process = ProcessAuthor::Process("p".to_owned());
    assert_eq!(
        permission.holds(&site(), &process, PUBLISH_PERMISSION),
        Ok(false)
    );
    assert_eq!(
        permission.holds(&site(), &alice(), PUBLISH_PERMISSION),
        Ok(true)
    );
}

#[test]
fn another_tenants_question_is_refused() {
    let permission =
        IamPublishPermission::new(site(), engine(), Arc::new(FakeAuthors { broken: false }));
    let other = SiteId::new("other.site").unwrap();
    assert_eq!(
        permission.holds(&other, &alice(), PUBLISH_PERMISSION),
        Ok(false)
    );
    let registry = IamServicePrincipals::new(directory());
    assert_eq!(registry.is_registered(&other, "svc_ledger"), Ok(false));
    assert_eq!(registry.may_use(&site(), &alice(), "svc_ledger"), Ok(true));
}

#[test]
fn every_violation_is_reported_in_node_order() {
    let g = guard(directory(), false);
    let v = validate_process_version(
        &g,
        &site(),
        &bob(),
        &dag(&[
            ("a", Some("svc_ledger")),
            ("b", Some("svc_missing")),
            ("c", None),
            ("d", Some("svc_ledger")),
        ]),
    );
    let nodes: Vec<(&str, &str)> = v.iter().map(|x| (x.node.as_str(), x.code)).collect();
    assert!(nodes.contains(&("b", codes::PRINCIPAL_NOT_REGISTERED)));
    assert!(nodes.contains(&("a", codes::PRINCIPAL_NOT_PERMITTED)));
    assert!(nodes.contains(&("d", codes::PRINCIPAL_NOT_PERMITTED)));
    assert!(v.iter().all(|x| x.node.as_str() != "c"));
}
