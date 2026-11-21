//! Named app permissions carried by roles and duties (WP-PLT-03.T3): flattening through
//! parents and duties with provenance, copy and diff, read-only reference roles, stored roles
//! without the field, and the whole chain proved against the permission engine.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::{at, meta};
use spot_iam_roles::{
    Assignment, Duty, DutyName, DutyRef, Origin, Registry, Role, RoleName, Version,
    build_snapshot_input, codes, copy_role, diff_reference, diff_reference_from_base, effective,
};
use spot_permissions::model::Model;
use spot_permissions::{
    AppPermissionCatalogue, AppPermissionDecl, AppPermissionName, AppReason, PermissionEngine,
    Snapshot,
};

fn pname(text: &str) -> AppPermissionName {
    AppPermissionName::parse(text).unwrap()
}

fn duty(name: &str, perms: &[&str]) -> Duty {
    Duty {
        name: DutyName::from(name),
        version: Version(1),
        grants: vec![],
        field_grants: vec![],
        app_permissions: perms.iter().map(|p| pname(p)).collect(),
        outside_exposed: false,
    }
}

fn role(name: &str, parents: &[&str], duties: &[&str], perms: &[&str]) -> Role {
    Role {
        duties: duties.iter().map(|d| DutyRef::named(d)).collect(),
        direct_app_permissions: perms.iter().map(|p| pname(p)).collect(),
        parents: parents.iter().map(|p| RoleName::from(*p)).collect(),
        ..Role::custom(name)
    }
}

/// `leaf` < `l2` < `l1` < `top` (depth 3 of parents); `l2` includes a duty; `top` holds one
/// directly.
fn registry() -> Registry {
    Registry::new(
        vec![duty("approving", &["approve_note"])],
        vec![
            role("top", &[], &[], &["release_note"]),
            role("l1", &["top"], &[], &[]),
            role("l2", &["l1"], &["approving"], &[]),
            role("leaf", &["l2"], &[], &[]),
            role("plain", &[], &[], &[]),
        ],
    )
    .unwrap()
}

fn catalogue(names: &[&str]) -> AppPermissionCatalogue {
    let mut c = AppPermissionCatalogue::new();
    for n in names {
        c.insert(
            pname(n),
            AppPermissionDecl {
                app: "notes".into(),
                title: "t".into(),
                description: "d".into(),
            },
        )
        .unwrap();
    }
    c
}

fn engine_for(
    registry: &Registry,
    assignments: &[Assignment],
    declared: &[&str],
    now: i64,
) -> (PermissionEngine, spot_iam_roles::SnapshotInput) {
    let input = build_snapshot_input(registry, assignments, at(now)).unwrap();
    let snapshot = Snapshot::build_with_apps(
        &meta(),
        &Model::default(),
        &catalogue(declared),
        &input.app_grants,
    )
    .unwrap();
    (PermissionEngine::new(Arc::new(snapshot)), input)
}

#[test]
fn permissions_flatten_through_parents_and_duties_with_provenance() {
    let r = registry();
    let leaf = effective(r.role(&RoleName::from("leaf")).unwrap(), &r);
    let names: Vec<&str> = leaf
        .app_permissions
        .iter()
        .map(|p| p.permission.as_str())
        .collect();
    assert_eq!(names, ["approve_note", "release_note"]);

    let via_duty = leaf.sources_of_app_permission(&pname("approve_note"));
    assert_eq!(via_duty.len(), 1);
    assert_eq!(via_duty[0].duty, Some(DutyName::from("approving")));
    assert_eq!(via_duty[0].role_path, ["leaf", "l2"].map(RoleName::from));

    let inherited = leaf.sources_of_app_permission(&pname("release_note"));
    assert_eq!(inherited[0].duty, None);
    assert_eq!(
        inherited[0].role_path,
        ["leaf", "l2", "l1", "top"].map(RoleName::from)
    );
    assert!(leaf.sources_of_app_permission(&pname("other")).is_empty());

    let plain = effective(r.role(&RoleName::from("plain")).unwrap(), &r);
    assert!(plain.app_permissions.is_empty());
}

#[test]
fn the_snapshot_input_lists_every_roles_permissions_sorted() {
    let input = build_snapshot_input(&registry(), &[], at(0)).unwrap();
    let rows: Vec<(String, String)> = input
        .app_grants
        .iter()
        .map(|g| (g.role.as_str().to_owned(), g.permission.as_str().to_owned()))
        .collect();
    let mut sorted = rows.clone();
    sorted.sort();
    assert_eq!(rows, sorted);
    assert!(rows.contains(&("leaf".to_owned(), "approve_note".to_owned())));
    assert!(rows.contains(&("l1".to_owned(), "release_note".to_owned())));
    assert!(!rows.iter().any(|(role, _)| role == "plain"));
}

#[test]
fn the_engine_allows_through_inheritance_and_the_duty_and_denies_the_rest() {
    let assignments = [
        Assignment::new("a1", "ann", "leaf"),
        Assignment::new("a2", "bob", "plain"),
        Assignment::new("a3", "cy", "l1"),
    ];
    let (engine, input) = engine_for(
        &registry(),
        &assignments,
        &["approve_note", "release_note"],
        0,
    );
    let ann = input.principal("ann");
    assert!(engine.app_permission(&ann, "approve_note").allowed);
    assert!(engine.app_permission(&ann, "release_note").allowed);
    let bob = input.principal("bob");
    assert!(!engine.app_permission(&bob, "approve_note").allowed);
    // `l1` inherits `top` (release) but does not include the duty (approve).
    let cy = input.principal("cy");
    assert!(engine.app_permission(&cy, "release_note").allowed);
    assert!(!engine.app_permission(&cy, "approve_note").allowed);
    // A user nobody assigned holds nothing.
    assert!(
        !engine
            .app_permission(&input.principal("nobody"), "approve_note")
            .allowed
    );
}

#[test]
fn an_expired_or_future_assignment_holds_nothing() {
    let mut expired = Assignment::new("a1", "ann", "leaf");
    expired.valid_to = Some(at(100));
    let mut future = Assignment::new("a2", "bob", "leaf");
    future.valid_from = Some(at(500));
    let mut live = Assignment::new("a3", "cy", "leaf");
    live.valid_from = Some(at(0));
    live.valid_to = Some(at(1000));
    let assignments = [expired, future, live];
    let (engine, input) = engine_for(&registry(), &assignments, &["approve_note"], 200);
    assert!(
        !engine
            .app_permission(&input.principal("ann"), "approve_note")
            .allowed
    );
    assert!(
        !engine
            .app_permission(&input.principal("bob"), "approve_note")
            .allowed
    );
    let d = engine.app_permission(&input.principal("cy"), "approve_note");
    assert!(d.allowed);
    assert!(
        matches!(d.reason, AppReason::GrantedByRole { ref assignment, .. } if assignment == "a3")
    );
}

#[test]
fn a_permission_no_app_declares_is_an_orphan_and_allows_nothing() {
    let assignments = [Assignment::new("a1", "ann", "leaf")];
    // Only `release_note` is declared: `approve_note` is the orphan.
    let (engine, input) = engine_for(&registry(), &assignments, &["release_note"], 0);
    let ann = input.principal("ann");
    assert!(engine.app_permission(&ann, "release_note").allowed);
    let d = engine.app_permission(&ann, "approve_note");
    assert!(!d.allowed);
    assert_eq!(d.reason.code(), "perm.app.unknown");
    assert!(
        engine
            .snapshot()
            .orphan_app_grants()
            .iter()
            .any(|(role, name)| role.as_str() == "leaf" && name.as_str() == "approve_note")
    );
}

#[test]
fn a_copy_carries_the_permissions_and_the_diff_reports_a_change() {
    let reference = Role {
        origin: Origin::Reference {
            pack: "core".into(),
            version: Version(1),
        },
        direct_app_permissions: vec![pname("approve_note"), pname("old_one")],
        ..Role::custom("Ref")
    };
    let copy = copy_role(&reference, "Mine");
    assert_eq!(
        copy.direct_app_permissions,
        reference.direct_app_permissions
    );

    let mut upgraded = reference.clone();
    upgraded.direct_app_permissions = vec![pname("approve_note"), pname("new_one")];
    // Against the copy (no base): added and removed relative to the copy.
    let diff = diff_reference(&copy, &upgraded);
    assert_eq!(diff.app_permissions_added, [pname("new_one")]);
    assert_eq!(diff.app_permissions_removed, [pname("old_one")]);
    assert!(!diff.is_empty());
    // Against the base: what the upgrade changes.
    let three = diff_reference_from_base(&reference, &copy, &upgraded);
    assert_eq!(three.app_permissions_added, [pname("new_one")]);
    assert_eq!(three.app_permissions_removed, [pname("old_one")]);
    // Nothing changed: the diff is empty.
    assert!(diff_reference(&copy, &reference).is_empty());
}

#[test]
fn a_reference_role_with_permissions_stays_read_only() {
    let reference = Role {
        origin: Origin::Reference {
            pack: "core".into(),
            version: Version(1),
        },
        direct_app_permissions: vec![pname("approve_note")],
        ..Role::custom("Ref")
    };
    let empty = Registry::new(vec![], vec![]).unwrap();
    let err = empty.with_role(reference.clone()).expect_err("read-only");
    assert!(err.has(codes::REFERENCE_READONLY));
    let installed = empty.install_pack(vec![], vec![reference]).unwrap();
    let again = installed
        .with_role(Role::custom("Ref"))
        .expect_err("replace");
    assert!(again.has(codes::REFERENCE_READONLY));
    let removal = installed
        .without_role(&RoleName::from("Ref"))
        .expect_err("remove");
    assert!(removal.has(codes::REFERENCE_READONLY));
}

#[test]
fn a_stored_role_without_the_field_still_loads_and_a_bad_name_does_not() {
    let old = "[[duty]]\nname = \"d\"\nversion = 1\n\n[[role]]\nname = \"r\"\norigin = { kind = \"custom\" }\nduties = [{ name = \"d\" }]\n";
    let registry = Registry::from_toml(old).unwrap();
    let r = registry.role(&RoleName::from("r")).unwrap();
    assert!(r.direct_app_permissions.is_empty());
    assert!(
        registry
            .duty(&DutyName::from("d"))
            .unwrap()
            .app_permissions
            .is_empty()
    );

    let new = "[[duty]]\nname = \"d\"\nversion = 1\napp_permissions = [\"approve_note\"]\n\n[[role]]\nname = \"r\"\norigin = { kind = \"custom\" }\ndirect_app_permissions = [\"release_note\"]\nduties = [{ name = \"d\" }]\n";
    let registry = Registry::from_toml(new).unwrap();
    let resolved = effective(registry.role(&RoleName::from("r")).unwrap(), &registry);
    assert_eq!(resolved.app_permissions.len(), 2);

    for bad in ["Approve_Note", "", "a b", "../x"] {
        let text = new.replace("release_note", bad);
        assert!(Registry::from_toml(&text).is_err(), "{bad:?}");
    }
}
