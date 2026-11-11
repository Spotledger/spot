//! Named app permissions (WP-PLT-03.T3): the decision matrix, the loader, the catalogue, the
//! orphan grants, the per-site isolation, and the properties.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::sync::Arc;

use proptest::prelude::*;
use spot_meta::{Definition, FieldSpec, FieldType, Registry};
use spot_permissions::model::{AssignmentContext, Model, Principal, Role};
use spot_permissions::{
    AppDecision, AppPermissionCatalogue, AppPermissionDecl, AppPermissionError, AppPermissionGrant,
    AppPermissionName, AppReason, PermissionEngine, Snapshot, load_app_grants,
};

fn name(text: &str) -> AppPermissionName {
    AppPermissionName::parse(text).unwrap()
}

fn decl(app: &str) -> AppPermissionDecl {
    AppPermissionDecl {
        app: app.to_owned(),
        title: "Title".to_owned(),
        description: "Description".to_owned(),
    }
}

fn catalogue(names: &[&str]) -> AppPermissionCatalogue {
    let mut c = AppPermissionCatalogue::new();
    for n in names {
        c.insert(name(n), decl("notes")).unwrap();
    }
    c
}

fn grant(role: &str, permission: &str) -> AppPermissionGrant {
    AppPermissionGrant {
        role: Role(role.to_owned()),
        permission: name(permission),
    }
}

fn meta() -> Arc<spot_meta::EffectiveMeta> {
    let mut note = Definition::new("note");
    note.fields = vec![FieldSpec::new("text", FieldType::Text)];
    Registry::new(vec![note]).unwrap().effective()
}

fn engine(names: &[&str], grants: &[AppPermissionGrant]) -> PermissionEngine {
    let snapshot =
        Snapshot::build_with_apps(&meta(), &Model::default(), &catalogue(names), grants).unwrap();
    PermissionEngine::new(Arc::new(snapshot))
}

fn code(d: &AppDecision) -> &'static str {
    d.reason.code()
}

fn user(id: &str, roles: &[&str]) -> Principal {
    Principal::user(id, vec![AssignmentContext::unscoped("a1", roles)])
}

// ---------------------------------------------------------------- the matrix

struct Case {
    label: &'static str,
    principal: Principal,
    asked: &'static str,
    allowed: bool,
    code: &'static str,
}

#[test]
fn the_decision_matrix_holds() {
    let e = engine(
        &["approve_note", "approve_notes", "release_note"],
        &[
            grant("approver", "approve_note"),
            grant("releaser", "release_note"),
            grant("ghost_role", "uninstalled_perm"),
        ],
    );
    let cases = vec![
        Case {
            label: "role granted the declared permission",
            principal: user("ann", &["approver"]),
            asked: "approve_note",
            allowed: true,
            code: "perm.app.granted",
        },
        Case {
            label: "neighbour: approve_notes is another permission",
            principal: user("ann", &["approver"]),
            asked: "approve_notes",
            allowed: false,
            code: "perm.app.not_granted",
        },
        Case {
            label: "neighbour: a different case is an invalid name",
            principal: user("ann", &["approver"]),
            asked: "Approve_Note",
            allowed: false,
            code: "perm.app.invalid_name",
        },
        Case {
            label: "declared, nobody granted",
            principal: user("ann", &["approver"]),
            asked: "release_note",
            allowed: false,
            code: "perm.app.not_granted",
        },
        Case {
            label: "orphan grant: the role lists a permission nobody declares",
            principal: user("ann", &["ghost_role"]),
            asked: "uninstalled_perm",
            allowed: false,
            code: "perm.app.unknown",
        },
        Case {
            label: "no assignments",
            principal: Principal::user("ann", vec![]),
            asked: "approve_note",
            allowed: false,
            code: "perm.app.not_granted",
        },
        Case {
            label: "unknown user (empty name) with the right role",
            principal: user("", &["approver"]),
            asked: "approve_note",
            allowed: false,
            code: "perm.app.not_granted",
        },
        Case {
            label: "empty name",
            principal: user("ann", &["approver"]),
            asked: "",
            allowed: false,
            code: "perm.app.invalid_name",
        },
        Case {
            label: "name with a space",
            principal: user("ann", &["approver"]),
            asked: "a b",
            allowed: false,
            code: "perm.app.invalid_name",
        },
        Case {
            label: "path-like name",
            principal: user("ann", &["approver"]),
            asked: "../x",
            allowed: false,
            code: "perm.app.invalid_name",
        },
        Case {
            label: "non-ASCII name",
            principal: user("ann", &["approver"]),
            asked: "approve_nöte",
            allowed: false,
            code: "perm.app.invalid_name",
        },
        Case {
            label: "name padded with a space is not trimmed",
            principal: user("ann", &["approver"]),
            asked: "approve_note ",
            allowed: false,
            code: "perm.app.invalid_name",
        },
        Case {
            label: "system principal",
            principal: Principal::system(),
            asked: "approve_note",
            allowed: true,
            code: "perm.app.system",
        },
        Case {
            label: "system principal, unknown name",
            principal: Principal::system(),
            asked: "never_declared",
            allowed: true,
            code: "perm.app.system",
        },
        Case {
            label: "not system and no user",
            principal: Principal {
                user: String::new(),
                assignments: vec![],
                is_system: false,
            },
            asked: "approve_note",
            allowed: false,
            code: "perm.app.not_granted",
        },
    ];
    for case in cases {
        let d = e.app_permission(&case.principal, case.asked);
        assert_eq!(d.allowed, case.allowed, "{}: {d:?}", case.label);
        assert_eq!(code(&d), case.code, "{}", case.label);
    }
}

#[test]
fn a_granted_decision_names_the_role_and_the_assignment() {
    let e = engine(&["approve_note"], &[grant("approver", "approve_note")]);
    let d = e.app_permission(&user("ann", &["approver"]), "approve_note");
    assert_eq!(
        d.reason,
        AppReason::GrantedByRole {
            role: Role("approver".into()),
            assignment: "a1".into()
        }
    );
    let text = e.explain_app_permission(&user("ann", &["approver"]), "approve_note");
    assert!(text.contains("ALLOWED") && text.contains("approver") && text.contains("a1"));
}

#[test]
fn an_unknown_name_with_an_empty_catalogue_is_denied() {
    // `Snapshot::build` means an empty catalogue: every app permission is unknown.
    let snapshot = Snapshot::build(&meta(), &Model::default()).unwrap();
    let e = PermissionEngine::new(Arc::new(snapshot));
    let d = e.app_permission(&user("ann", &["approver"]), "approve_note");
    assert!(!d.allowed);
    assert_eq!(code(&d), "perm.app.unknown");
    // ... but the platform is not blocked.
    assert!(
        e.app_permission(&Principal::system(), "approve_note")
            .allowed
    );
}

#[test]
fn a_65_byte_name_is_invalid_and_a_64_byte_name_is_valid() {
    let e = engine(&["approve_note"], &[]);
    let long = "a".repeat(65);
    let d = e.app_permission(&user("ann", &["x"]), &long);
    assert_eq!(code(&d), "perm.app.invalid_name");
    assert!(AppPermissionName::parse(&"a".repeat(64)).is_ok());
    assert!(AppPermissionName::parse(&long).is_err());
    for bad in ["", "1abc", "_abc", "Abc", "a-b", "a.b", "a b", "é"] {
        assert!(AppPermissionName::parse(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn the_first_match_in_assignment_id_order_is_named_and_scope_is_not_consulted() {
    let e = engine(&["approve_note"], &[grant("approver", "approve_note")]);
    // Two assignments, only the second grants it.
    let p = Principal::user(
        "ann",
        vec![
            AssignmentContext::unscoped("a1", &["reader"]),
            AssignmentContext::unscoped("a2", &["approver"]),
        ],
    );
    let d = e.app_permission(&p, "approve_note");
    assert!(d.allowed);
    assert!(matches!(&d.reason, AppReason::GrantedByRole { assignment, .. } if assignment == "a2"));

    // Both grant it: the lowest assignment id is named whatever the order of the list.
    let both = |order: [&str; 2]| {
        Principal::user(
            "ann",
            order
                .iter()
                .map(|id| AssignmentContext::unscoped(id, &["approver"]))
                .collect(),
        )
    };
    assert_eq!(
        e.app_permission(&both(["a9", "a3"]), "approve_note"),
        e.app_permission(&both(["a3", "a9"]), "approve_note")
    );

    // A scoped assignment (restricting to a company) still holds the permission: documented.
    let mut scoped = AssignmentContext::unscoped("a1", &["approver"]);
    scoped.scope = vec![spot_permissions::UserPermission {
        user: "ann".into(),
        doctype: None,
        field_or_link_doctype: "company".into(),
        allowed: vec!["01890000-0000-7000-8000-000000000001".into()],
        applies_to: Default::default(),
        include_descendants: false,
    }];
    let d = e.app_permission(&Principal::user("ann", vec![scoped]), "approve_note");
    assert!(d.allowed, "scope is not consulted for a named permission");
}

#[test]
fn a_denial_never_says_which_roles_hold_the_permission() {
    let e = engine(
        &["approve_note"],
        &[grant("secret_approver_role", "approve_note")],
    );
    for (who, asked) in [
        (user("ann", &["reader"]), "approve_note"),
        (user("ann", &["reader"]), "nope"),
        (user("ann", &["reader"]), "BAD NAME"),
        (Principal::user("ann", vec![]), "approve_note"),
    ] {
        let d = e.app_permission(&who, asked);
        assert!(!d.allowed);
        let said = format!("{} {}", d.reason.sentence(), d.reason.code());
        assert!(!said.contains("secret_approver_role"), "{said}");
        let explained = e.explain_app_permission(&who, asked);
        assert!(!explained.contains("secret_approver_role"), "{explained}");
    }
}

// ------------------------------------------------------ orphans and the catalogue

#[test]
fn orphan_grants_are_listed_sorted_and_allow_nothing() {
    let grants = [
        grant("z_role", "gone_two"),
        grant("a_role", "gone_one"),
        grant("a_role", "gone_one"),
        grant("a_role", "approve_note"),
    ];
    let e = engine(&["approve_note"], &grants);
    let orphans: Vec<(String, String)> = e
        .snapshot()
        .orphan_app_grants()
        .iter()
        .map(|(r, n)| (r.as_str().to_owned(), n.as_str().to_owned()))
        .collect();
    assert_eq!(
        orphans,
        [
            ("a_role".to_owned(), "gone_one".to_owned()),
            ("z_role".to_owned(), "gone_two".to_owned())
        ]
    );
    // The declared grant is unaffected by the orphans next to it.
    assert!(
        e.app_permission(&user("ann", &["a_role"]), "approve_note")
            .allowed
    );
    assert!(
        !e.app_permission(&user("ann", &["z_role"]), "gone_two")
            .allowed
    );
}

#[test]
fn a_duplicate_declaration_names_both_apps_and_leaves_the_catalogue_unchanged() {
    let mut c = AppPermissionCatalogue::new();
    c.insert(name("approve_note"), decl("notes")).unwrap();
    let err = c
        .insert(name("approve_note"), decl("sales"))
        .expect_err("a duplicate");
    assert_eq!(err.code(), "perm.app_permission_duplicate");
    let text = err.to_string();
    assert!(text.contains("notes") && text.contains("sales"), "{text}");
    assert_eq!(c.len(), 1);
    assert_eq!(c.get(&name("approve_note")).unwrap().app, "notes");
}

#[test]
fn the_loader_reads_grants_and_reports_every_problem_at_once() {
    let ok = load_app_grants(
        "[[entries]]\nrole = \"b\"\npermission = \"p_two\"\n\n[[entries]]\nrole = \"a\"\npermission = \"p_one\"\n\n[[entries]]\nrole = \"a\"\npermission = \"p_one\"\n",
    )
    .unwrap();
    assert_eq!(ok, [grant("a", "p_one"), grant("b", "p_two")]);
    assert!(load_app_grants("").unwrap().is_empty());

    let errors = load_app_grants(
        "[[entries]]\nrole = \"a\"\npermission = \"Bad Name\"\n\n[[entries]]\nrole = \"b\"\npermission = \"\"\n\n[[entries]]\nrole = \" \"\npermission = \"fine\"\n",
    )
    .expect_err("invalid");
    let codes: Vec<&str> = errors.iter().map(AppPermissionError::code).collect();
    assert_eq!(
        codes,
        [
            "perm.app_permission_invalid_name",
            "perm.app_permission_invalid_name",
            "perm.missing"
        ]
    );
    // An unknown key and bad TOML are load errors, not silently ignored.
    let unknown = load_app_grants("[[entries]]\nrole = \"a\"\npermission = \"p\"\nextra = 1\n")
        .expect_err("unknown key");
    assert_eq!(unknown[0].code(), "perm.parse_failed");
    assert!(load_app_grants("[[entries").is_err());
}

#[test]
fn a_name_serialises_as_text_and_refuses_an_invalid_one_on_load() {
    let g = grant("a", "p_one");
    let text = toml::to_string(&g).unwrap();
    assert_eq!(text.trim(), "role = \"a\"\npermission = \"p_one\"");
    let back: AppPermissionGrant = toml::from_str(&text).unwrap();
    assert_eq!(back, g);
    assert!(toml::from_str::<AppPermissionGrant>("role = \"a\"\npermission = \"P\"").is_err());
    assert!(
        toml::from_str::<AppPermissionGrant>("role = \"a\"\npermission = \"p\"\nx = 1").is_err()
    );
}

// ----------------------------------------------------------- sites and snapshots

#[test]
fn site_a_and_site_b_do_not_share_app_grants() {
    let grants_a = [grant("approver", "approve_note")];
    let a = engine(&["approve_note"], &grants_a);
    // Site B: the same role name, the permission neither granted nor declared.
    let b = engine(&[], &[]);
    let p = user("ann", &["approver"]);
    assert!(a.app_permission(&p, "approve_note").allowed);
    let d = b.app_permission(&p, "approve_note");
    assert!(!d.allowed);
    assert_eq!(code(&d), "perm.app.unknown");
    // Site C declares it but grants nothing: denied as not granted.
    let c = engine(&["approve_note"], &[]);
    assert_eq!(
        code(&c.app_permission(&p, "approve_note")),
        "perm.app.not_granted"
    );
}

#[test]
fn a_new_snapshot_that_revokes_a_grant_takes_effect_on_the_next_call() {
    let p = user("ann", &["approver"]);
    let before = engine(&["approve_note"], &[grant("approver", "approve_note")]);
    assert!(before.app_permission(&p, "approve_note").allowed);
    // The old engine keeps its consistent view; the new one has no cache of the old answer.
    let after = engine(&["approve_note"], &[]);
    assert!(!after.app_permission(&p, "approve_note").allowed);
    assert!(before.app_permission(&p, "approve_note").allowed);
}

// -------------------------------------------------------------------- properties

const ROLES: &[&str] = &["r0", "r1", "r2", "r3", "r4"];
const PERMS: &[&str] = &["p_a", "p_b", "p_c"];

fn arb_grants() -> impl Strategy<Value = Vec<AppPermissionGrant>> {
    prop::collection::vec((0..ROLES.len(), 0..PERMS.len()), 0..10).prop_map(|v| {
        v.into_iter()
            .map(|(r, p)| grant(ROLES[r], PERMS[p]))
            .collect()
    })
}

fn arb_roles() -> impl Strategy<Value = Vec<usize>> {
    prop::collection::vec(0..ROLES.len(), 0..5)
}

fn principal_of(roles: &[usize]) -> Principal {
    Principal::user(
        "ann",
        roles
            .iter()
            .enumerate()
            .map(|(i, r)| AssignmentContext::unscoped(&format!("a{i}"), &[ROLES[*r]]))
            .collect(),
    )
}

proptest! {
    #[test]
    fn adding_a_role_never_removes_an_app_permission(
        grants in arb_grants(), roles in arb_roles(), extra in 0..ROLES.len(), p in 0..PERMS.len()
    ) {
        let e = engine(PERMS, &grants);
        let before = e.app_permission(&principal_of(&roles), PERMS[p]).allowed;
        let mut more = roles.clone();
        more.push(extra);
        let after = e.app_permission(&principal_of(&more), PERMS[p]).allowed;
        prop_assert!(!before || after);
    }

    #[test]
    fn granting_x_never_changes_the_answer_for_y(
        grants in arb_grants(), roles in arb_roles(), x in 0..PERMS.len(), y in 0..PERMS.len(), r in 0..ROLES.len()
    ) {
        prop_assume!(x != y);
        let before = engine(PERMS, &grants).app_permission(&principal_of(&roles), PERMS[y]).allowed;
        let mut plus = grants.clone();
        plus.push(grant(ROLES[r], PERMS[x]));
        let after = engine(PERMS, &plus).app_permission(&principal_of(&roles), PERMS[y]).allowed;
        prop_assert_eq!(before, after);
    }

    #[test]
    fn the_result_is_independent_of_assignment_and_grant_order(
        grants in arb_grants(), roles in arb_roles(), p in 0..PERMS.len()
    ) {
        let mut reversed_grants = grants.clone();
        reversed_grants.reverse();
        let mut reversed_roles: Vec<(usize, usize)> = roles.iter().copied().enumerate().collect();
        reversed_roles.reverse();
        let reversed = Principal::user(
            "ann",
            reversed_roles
                .iter()
                .map(|(i, r)| AssignmentContext::unscoped(&format!("a{i}"), &[ROLES[*r]]))
                .collect(),
        );
        let one = engine(PERMS, &grants).app_permission(&principal_of(&roles), PERMS[p]);
        let two = engine(PERMS, &reversed_grants).app_permission(&reversed, PERMS[p]);
        prop_assert_eq!(one, two);
    }

    #[test]
    fn the_decision_never_panics_and_never_allows_an_invalid_or_undeclared_name(
        text in ".*", roles in arb_roles()
    ) {
        let e = engine(PERMS, &[grant("r0", "p_a"), grant("r1", "p_b")]);
        let d = e.app_permission(&principal_of(&roles), &text);
        if d.allowed {
            let declared: BTreeSet<&str> = PERMS.iter().copied().collect();
            prop_assert!(declared.contains(text.as_str()));
        }
        let _ = e.explain_app_permission(&principal_of(&roles), &text);
    }
}
