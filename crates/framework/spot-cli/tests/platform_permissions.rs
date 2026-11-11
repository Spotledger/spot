//! The platform's own process permissions are in every site's app permission catalogue, an
//! installed app cannot shadow them, and a grant of one is effective (WP-PRD-01.T31).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use spot_cli::services::controllers::{
    DeclareError, PLATFORM_APP, declare_app_permission, declare_platform_permissions,
    platform_permissions,
};
use spot_meta::{Definition, FieldSpec, FieldType, Registry};
use spot_permissions::model::{AssignmentContext, Model, Principal, Role};
use spot_permissions::{
    AppPermissionCatalogue, AppPermissionDecl, AppPermissionGrant, AppPermissionName, AppReason,
    PermissionEngine, Snapshot,
};

const NAMES: [&str; 5] = [
    "process_publish",
    "process_read_versions",
    "process_migrate",
    "process_supervise",
    "process_publish_service_principal",
];

fn name(text: &str) -> AppPermissionName {
    AppPermissionName::parse(text).unwrap()
}

fn platform_catalogue() -> AppPermissionCatalogue {
    let mut catalogue = AppPermissionCatalogue::new();
    declare_platform_permissions(&mut catalogue);
    catalogue
}

fn engine(grants: &[AppPermissionGrant]) -> PermissionEngine {
    let mut note = Definition::new("note");
    note.fields = vec![FieldSpec::new("text", FieldType::Text)];
    let meta = Registry::new(vec![note]).unwrap().effective();
    let snapshot =
        Snapshot::build_with_apps(&meta, &Model::default(), &platform_catalogue(), grants).unwrap();
    PermissionEngine::new(Arc::new(snapshot))
}

fn user(roles: &[&str]) -> Principal {
    Principal::user("u1", vec![AssignmentContext::unscoped("a1", roles)])
}

#[test]
fn the_catalogue_holds_the_five_names_with_no_app_installed() {
    let catalogue = platform_catalogue();
    assert_eq!(catalogue.len(), 5);
    for n in NAMES {
        let decl = catalogue.get(&name(n)).expect(n);
        assert_eq!(decl.app, PLATFORM_APP);
        assert!(!decl.title.is_empty() && !decl.description.is_empty());
    }
    let listed: Vec<String> = platform_permissions()
        .iter()
        .map(|(n, _)| n.as_str().to_owned())
        .collect();
    assert_eq!(listed.len(), 5);
    for n in NAMES {
        assert!(listed.iter().any(|l| l == n), "{n}");
    }
}

#[test]
fn an_app_cannot_shadow_a_platform_permission() {
    for n in NAMES {
        let mut catalogue = platform_catalogue();
        let decl = AppPermissionDecl {
            app: "notes".to_owned(),
            title: "t".to_owned(),
            description: "d".to_owned(),
        };
        let Err(DeclareError::Duplicate(message)) =
            declare_app_permission(&mut catalogue, "main", n, decl)
        else {
            panic!("{n} was not refused");
        };
        assert!(message.contains(n), "{message}");
        assert!(message.contains("notes") && message.contains(PLATFORM_APP));
        assert_eq!(catalogue.len(), 5);
    }
    // A different name is still taken.
    let mut catalogue = platform_catalogue();
    let decl = AppPermissionDecl {
        app: "notes".to_owned(),
        title: "t".to_owned(),
        description: "d".to_owned(),
    };
    declare_app_permission(&mut catalogue, "main", "approve_note", decl).unwrap();
    assert_eq!(catalogue.len(), 6);
}

#[test]
fn a_granted_role_is_allowed_an_ungranted_or_unknown_role_is_denied() {
    let grant = AppPermissionGrant {
        role: Role("publisher".to_owned()),
        permission: name("process_publish_service_principal"),
    };
    let e = engine(&[grant]);
    // spot_bpm_documents::engine_permission_name("process.publish_service_principal") is this
    // name (spot-bpm-documents is not a dependency of spot-cli).
    let asked = "process_publish_service_principal";
    let allowed = e.app_permission(&user(&["publisher"]), asked);
    assert!(allowed.allowed);
    for roles in [&["clerk"][..], &["ghost"][..], &[][..]] {
        let denied = e.app_permission(&user(roles), asked);
        assert!(!denied.allowed);
        assert_eq!(denied.reason, AppReason::NotGranted);
        // The reason is a bare code: it names neither role nor permission value.
        let text = format!("{:?} {}", denied.reason, denied.reason.code());
        assert!(!text.contains("clerk") && !text.contains("ghost"));
    }
    // The grant opens that permission only.
    assert!(
        !e.app_permission(&user(&["publisher"]), "process_publish")
            .allowed
    );
}

#[test]
fn a_grant_of_a_platform_permission_is_not_an_orphan() {
    let grants = [
        AppPermissionGrant {
            role: Role("publisher".to_owned()),
            permission: name("process_publish_service_principal"),
        },
        AppPermissionGrant {
            role: Role("publisher".to_owned()),
            permission: name("not_declared_anywhere"),
        },
    ];
    let mut note = Definition::new("note");
    note.fields = vec![FieldSpec::new("text", FieldType::Text)];
    let meta = Registry::new(vec![note]).unwrap().effective();
    let snapshot =
        Snapshot::build_with_apps(&meta, &Model::default(), &platform_catalogue(), &grants)
            .unwrap();
    let orphans = snapshot.orphan_app_grants();
    assert_eq!(orphans.len(), 1);
    assert_eq!(orphans[0].1.as_str(), "not_declared_anywhere");
}
