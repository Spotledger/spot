//! Registry validation: cycles, depth, references, read-only reference roles.
#![allow(missing_docs)]

mod common;

use common::registry;
use spot_iam_roles::{
    Assignment, Duty, DutyName, DutyRef, MAX_DEPTH, Origin, Registry, Role, RoleName, Version,
    codes, validate_assignments,
};
use spot_permissions::model::{Right, Rights, Role as GrantRole, RoleGrant};

fn chain(n: usize) -> Vec<Role> {
    (0..n)
        .map(|i| {
            let mut role = Role::custom(&format!("r{i}"));
            if i > 0 {
                role.parents = vec![RoleName::from(format!("r{}", i - 1).as_str())];
            }
            role
        })
        .collect()
}

fn codes_of(r: spot_iam_roles::Result<Registry>) -> Vec<&'static str> {
    r.err().map(|e| e.codes()).unwrap_or_default()
}

#[test]
fn the_fixture_registry_is_valid() {
    let r = registry();
    assert_eq!(r.roles().count(), 7);
    assert_eq!(r.duties().count(), 5);
}

#[test]
fn a_cycle_is_refused_with_its_path_once() {
    let mut roles = chain(3);
    roles[0].parents = vec![RoleName::from("r2")];
    let e = Registry::new(vec![], roles)
        .err()
        .unwrap_or_else(|| panic!("must fail"));
    assert_eq!(e.codes(), [codes::ROLE_CYCLE]);
    assert!(
        e.errors()[0].message.contains("r0 -> r2 -> r1 -> r0"),
        "{}",
        e
    );
}

#[test]
fn a_role_that_inherits_from_itself_is_a_cycle() {
    let mut role = Role::custom("loop");
    role.parents = vec![RoleName::from("loop")];
    let e = Registry::new(vec![], vec![role])
        .err()
        .unwrap_or_else(|| panic!("must fail"));
    assert_eq!(e.codes(), [codes::ROLE_CYCLE]);
    assert!(e.errors()[0].message.contains("loop -> loop"), "{e}");
}

#[test]
fn two_separate_cycles_are_both_reported() {
    let mut roles = chain(2);
    roles[0].parents = vec![RoleName::from("r1")];
    let mut other = chain(2);
    for (i, r) in other.iter_mut().enumerate() {
        r.name = RoleName::from(format!("s{i}").as_str());
        r.parents = vec![RoleName::from(format!("s{}", 1 - i).as_str())];
    }
    roles.extend(other);
    let e = Registry::new(vec![], roles)
        .err()
        .unwrap_or_else(|| panic!("must fail"));
    assert_eq!(e.codes(), [codes::ROLE_CYCLE, codes::ROLE_CYCLE]);
}

#[test]
fn depth_eight_is_allowed_and_nine_is_refused_with_the_chain() {
    assert!(Registry::new(vec![], chain(MAX_DEPTH)).is_ok());
    let e = Registry::new(vec![], chain(MAX_DEPTH + 1))
        .err()
        .unwrap_or_else(|| panic!("must fail"));
    assert_eq!(e.codes(), [codes::ROLE_DEPTH]);
    let message = &e.errors()[0].message;
    assert!(message.contains("r8 -> r7"), "{message}");
    assert!(message.contains("r0"), "{message}");
    // Longer chains are still one error: the first role past the limit.
    let e = Registry::new(vec![], chain(MAX_DEPTH + 4))
        .err()
        .unwrap_or_else(|| panic!("must fail"));
    assert_eq!(e.codes(), [codes::ROLE_DEPTH]);
}

#[test]
fn a_diamond_is_not_a_cycle() {
    let mut top = Role::custom("top");
    top.parents = vec![RoleName::from("left"), RoleName::from("right")];
    let mut left = Role::custom("left");
    left.parents = vec![RoleName::from("base")];
    let mut right = Role::custom("right");
    right.parents = vec![RoleName::from("base")];
    assert!(Registry::new(vec![], vec![top, left, right, Role::custom("base")]).is_ok());
}

#[test]
fn unknown_duties_parents_and_pinned_versions_are_all_reported_at_once() {
    let mut role = Role::custom("r");
    role.duties = vec![
        DutyRef::named("ghost"),
        DutyRef {
            name: DutyName::from("known"),
            version: Some(Version(9)),
        },
    ];
    role.parents = vec![RoleName::from("nobody")];
    let duty = Duty {
        name: DutyName::from("known"),
        version: Version(1),
        grants: vec![],
        field_grants: vec![],
        app_permissions: vec![],
        outside_exposed: false,
    };
    let e = Registry::new(vec![duty], vec![role])
        .err()
        .unwrap_or_else(|| panic!("must fail"));
    assert_eq!(
        e.codes(),
        [
            codes::UNKNOWN_DUTY,
            codes::DUTY_VERSION,
            codes::UNKNOWN_PARENT
        ]
    );
}

#[test]
fn a_grant_must_name_its_owner() {
    let mut role = Role::custom("mine");
    role.direct_grants = vec![RoleGrant {
        doctype: "customer".into(),
        role: GrantRole("someone else".into()),
        rights: Rights::new([Right::Read]),
        if_owner: false,
    }];
    assert_eq!(
        codes_of(Registry::new(vec![], vec![role])),
        [codes::GRANT_ROLE]
    );
}

#[test]
fn duplicate_names_and_bad_names_are_refused() {
    let e = Registry::new(vec![], vec![Role::custom("a"), Role::custom("a")])
        .err()
        .unwrap_or_else(|| panic!("must fail"));
    assert_eq!(e.codes(), [codes::DUPLICATE_NAME]);
    for bad in ["", " lead", "trail ", "tab\tname"] {
        assert_eq!(
            codes_of(Registry::new(vec![], vec![Role::custom(bad)])),
            [codes::INVALID_NAME],
            "`{bad:?}`"
        );
    }
}

#[test]
fn a_reference_role_is_read_only() {
    let r = registry();
    let core = RoleName::from("Core Viewer");
    // Not replaceable, not removable, and not addable except by a pack.
    let mut changed = r.role(&core).cloned().unwrap_or_else(|| panic!("role"));
    changed.parents.clear();
    assert_eq!(
        codes_of(r.with_role(changed.clone())),
        [codes::REFERENCE_READONLY]
    );
    changed.name = RoleName::from("New Reference");
    assert_eq!(codes_of(r.with_role(changed)), [codes::REFERENCE_READONLY]);
    assert_eq!(codes_of(r.without_role(&core)), [codes::REFERENCE_READONLY]);
    // A custom role is editable and removable.
    let mut clerk = r
        .role(&RoleName::from("Clerk"))
        .cloned()
        .unwrap_or_else(|| panic!("clerk"));
    clerk.parents = vec![RoleName::from("Accounting Viewer")];
    assert!(r.with_role(clerk).is_ok());
    assert!(r.without_role(&RoleName::from("Clerk")).is_ok());
}

#[test]
fn removing_a_role_that_others_inherit_from_is_refused() {
    let r = registry();
    assert_eq!(
        codes_of(r.without_role(&RoleName::from("Accounting Viewer"))),
        [codes::UNKNOWN_PARENT, codes::UNKNOWN_PARENT]
    );
}

#[test]
fn a_pack_installs_reference_roles_and_leaves_copies_alone() {
    let r = registry();
    let new_core = Role {
        origin: Origin::Reference {
            pack: "core".into(),
            version: Version(4),
        },
        ..r.role(&RoleName::from("Core Viewer"))
            .cloned()
            .unwrap_or_else(|| panic!("role"))
    };
    let upgraded = r
        .install_pack(vec![], vec![new_core.clone()])
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        upgraded.role(&RoleName::from("Core Viewer")),
        Some(&new_core)
    );
    // A pack cannot carry a custom role, nor take a custom role's name.
    assert_eq!(
        codes_of(r.install_pack(vec![], vec![Role::custom("x")])),
        [codes::REFERENCE_READONLY]
    );
    let clash = Role {
        origin: Origin::Reference {
            pack: "core".into(),
            version: Version(4),
        },
        ..Role::custom("Clerk")
    };
    assert_eq!(
        codes_of(r.install_pack(vec![], vec![clash])),
        [codes::DUPLICATE_NAME]
    );
}

#[test]
fn the_registry_file_refuses_unknown_keys() {
    let bad = "[[role]]\nname = \"x\"\norigin = { kind = \"custom\" }\nscope = []\n";
    assert!(Registry::from_toml(bad).is_err());
    assert!(
        Registry::from_toml("[[duty]]\nname = \"d\"\n").is_err(),
        "a duty needs a version"
    );
}

#[test]
fn assignments_are_checked_against_the_registry() {
    let r = registry();
    let ok = Assignment::new("a1", "u1", "Clerk");
    assert!(validate_assignments(&r, std::slice::from_ref(&ok)).is_ok());

    let unknown = Assignment::new("a2", "u1", "No Such Role");
    let dup_id = Assignment::new("a1", "u2", "Clerk");
    let same = Assignment::new("a3", "u1", "Clerk");
    let e = validate_assignments(&r, &[ok.clone(), unknown, dup_id, same])
        .err()
        .unwrap_or_else(|| panic!("must fail"));
    assert_eq!(
        e.codes(),
        [
            codes::UNKNOWN_ROLE,
            codes::DUPLICATE_ASSIGNMENT,
            codes::DUPLICATE_ASSIGNMENT
        ]
    );
}

#[test]
fn a_window_must_end_after_it_starts() {
    let mut a = Assignment::new("a1", "u1", "Clerk");
    a.valid_from = Some(common::at(100));
    a.valid_to = Some(common::at(100));
    assert_eq!(
        a.validate().err().map(|e| e.codes()),
        Some(vec![codes::ASSIGNMENT_WINDOW])
    );
    a.valid_to = Some(common::at(99));
    assert!(a.validate().is_err());
    a.valid_to = Some(common::at(101));
    assert!(a.validate().is_ok());
}
