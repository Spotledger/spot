//! Barrier 3 of ADR-083 on the role model: the allow-list, the guard, role edits, the
//! snapshot backstop, the closed default for old duties, and a property over random graphs.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::at;
use proptest::prelude::*;
use spot_iam_roles::{
    Assignment, Duty, DutyName, DutyRef, OutsideGuard, Registry, Role, RoleKind, RoleName, codes,
    effective, portal_holders,
};

const FIXTURE: &str = include_str!("fixtures/outside.toml");

fn registry() -> Registry {
    Registry::from_toml(FIXTURE).unwrap_or_else(|e| panic!("{e}"))
}

fn resolved(r: &Registry, role: &str) -> spot_iam_roles::ResolvedRole {
    effective(r.role(&RoleName::from(role)).unwrap(), r)
}

#[test]
fn the_allow_list_decides_each_role() {
    let r = registry();
    // (role, expected code, text the message must name)
    let table: [(&str, Option<(&str, &str)>); 9] = [
        ("Desk Clerk", Some((codes::OUTSIDE_ROLE_KIND, "Desk Clerk"))),
        (
            "Desk Exposed Only",
            Some((codes::OUTSIDE_ROLE_KIND, "Desk Exposed Only")),
        ),
        ("Portal Viewer", None),
        ("Portal Empty", None),
        (
            "Portal Leaky",
            Some((codes::OUTSIDE_DUTY_NOT_EXPOSED, "post_invoices")),
        ),
        (
            "Portal Inherits Leaky",
            Some((codes::OUTSIDE_DUTY_NOT_EXPOSED, "post_invoices")),
        ),
        (
            "Portal Old Duty",
            Some((codes::OUTSIDE_DUTY_NOT_EXPOSED, "old_stored_duty")),
        ),
        (
            "Portal Over Desk",
            Some((codes::OUTSIDE_ROLE_KIND, "Desk Exposed Only")),
        ),
        (
            "Portal Direct",
            Some((codes::OUTSIDE_DUTY_NOT_EXPOSED, "Portal Direct")),
        ),
    ];
    for (role, expected) in table {
        let verdict = resolved(&r, role).check_for_portal();
        match (verdict, expected) {
            (Ok(()), None) => {}
            (Err(errors), Some((code, names))) => {
                assert!(errors.has(code), "{role}: {errors}");
                assert!(errors.to_string().contains(names), "{role}: {errors}");
                assert!(errors.to_string().contains(role), "{role}: {errors}");
            }
            (other, expected) => panic!("{role}: got {other:?}, expected {expected:?}"),
        }
    }
}

#[test]
fn a_stored_duty_without_the_mark_is_closed() {
    let duty: Duty = toml::from_str("name = \"old\"\nversion = 1\n").unwrap();
    assert!(!duty.outside_exposed);
    let role: Role = toml::from_str("name = \"r\"\norigin = { kind = \"custom\" }\n").unwrap();
    assert_eq!(role.kind(), RoleKind::Desk);
    assert_eq!(Role::custom("x").kind(), RoleKind::Desk);
}

#[test]
fn the_guard_protects_portal_users_only_and_fails_closed() {
    let r = registry();
    let guard = OutsideGuard::new(&r, ["portal-1"]);
    assert!(guard.is_portal("portal-1"));
    // An internal user may hold anything.
    for role in r.roles() {
        assert!(
            guard.check("staff-1", &role.name).is_ok(),
            "{}",
            role.name.as_str()
        );
    }
    // A portal user: exactly the allow-listed roles.
    for (role, allowed) in [
        ("Portal Viewer", true),
        ("Portal Empty", true),
        ("Desk Clerk", false),
        ("Portal Leaky", false),
        ("Portal Inherits Leaky", false),
        ("Portal Direct", false),
        ("No Such Role", false),
    ] {
        let verdict = guard.check("portal-1", &RoleName::from(role));
        assert_eq!(verdict.is_ok(), allowed, "{role}: {verdict:?}");
    }
    // The empty guard protects no one.
    assert!(
        OutsideGuard::none()
            .check("portal-1", &RoleName::from("Desk Clerk"))
            .is_ok()
    );
}

#[test]
fn the_blind_guard_refuses_whatever_it_cannot_see() {
    let r = registry();
    let roles: Vec<Role> = r.roles().cloned().collect();
    let guard = OutsideGuard::from_roles(&roles, ["p"]);
    assert!(guard.check("p", &RoleName::from("Portal Empty")).is_ok());
    // Has a duty: its mark cannot be seen, so it is refused even though the duty is exposed.
    assert!(guard.check("p", &RoleName::from("Portal Viewer")).is_err());
    assert!(guard.check("p", &RoleName::from("Desk Clerk")).is_err());
    assert!(guard.check("p", &RoleName::from("Portal Direct")).is_err());
    assert!(
        guard
            .check("p", &RoleName::from("Portal Over Desk"))
            .is_err()
    );
}

fn grant_role(r: &Registry, name: &str) -> Role {
    r.role(&RoleName::from(name)).unwrap().clone()
}

#[test]
fn an_edit_that_opens_a_role_held_by_a_portal_user_is_refused() {
    let r = registry();
    let guard = OutsideGuard::new(&r, ["portal-1"]);
    let held = [
        Assignment::new("a1", "portal-1", "Portal Viewer"),
        Assignment::new("a2", "staff-1", "Desk Clerk"),
    ];
    let holders = portal_holders(&guard, &held, at(0));
    assert_eq!(holders.len(), 1);

    // Adds a staff duty to the portal role.
    let mut leaky = grant_role(&r, "Portal Viewer");
    leaky.duties.push(DutyRef::named("post_invoices"));
    let err = r.with_role_guarded(leaky, &holders).unwrap_err();
    assert!(err.has(codes::OUTSIDE_ROLE_EDIT_REFUSED), "{err}");
    assert!(err.to_string().contains("1 portal user"), "{err}");
    assert!(!err.to_string().contains("portal-1"), "{err}");

    // Turns it into a desk role.
    let mut desk = grant_role(&r, "Portal Viewer");
    desk.kind = RoleKind::Desk;
    assert!(r.with_role_guarded(desk, &holders).is_err());

    // The same edit on a role no portal user holds is allowed, and so is a harmless edit.
    let mut other = grant_role(&r, "Portal Empty");
    other.duties.push(DutyRef::named("post_invoices"));
    assert!(r.with_role_guarded(other, &holders).is_ok());
    let mut harmless = grant_role(&r, "Portal Viewer");
    harmless.duties.push(DutyRef::named("view_own_invoices"));
    harmless.duties.dedup_by(|a, b| a.name == b.name);
    assert!(r.with_role_guarded(harmless, &holders).is_ok());
}

#[test]
fn an_edit_to_an_ancestor_or_a_shared_duty_is_caught_through_the_holder() {
    let r = registry();
    let guard = OutsideGuard::new(&r, ["portal-1"]);
    // The portal user holds a role that is clean today.
    let held = [Assignment::new("a1", "portal-1", "Portal Empty")];
    let holders = portal_holders(&guard, &held, at(0));
    // A new parent that carries a staff duty, added to the held role.
    let mut child = grant_role(&r, "Portal Empty");
    child.parents.push(RoleName::from("Portal Leaky"));
    assert!(r.with_role_guarded(child, &holders).is_err());

    // A pack that re-ships the exposed duty as unexposed.
    let held = [Assignment::new("a1", "portal-1", "Portal Viewer")];
    let holders = portal_holders(&guard, &held, at(0));
    let closed = Duty {
        outside_exposed: false,
        ..r.duty(&DutyName::from("view_own_invoices"))
            .unwrap()
            .clone()
    };
    assert!(
        r.install_pack_guarded(vec![closed], vec![], &holders)
            .is_err()
    );
}

#[test]
fn ended_assignments_do_not_count_as_holders() {
    let r = registry();
    let guard = OutsideGuard::new(&r, ["portal-1"]);
    let mut ended = Assignment::new("a1", "portal-1", "Portal Viewer");
    ended.valid_to = Some(at(-10));
    assert!(portal_holders(&guard, &[ended], at(0)).is_empty());
}

#[test]
fn the_snapshot_drops_a_forged_assignment_and_says_so() {
    let r = registry();
    let guard = OutsideGuard::new(&r, ["portal-1"]);
    // Built by hand, past every other check.
    let forged = Assignment::new("a1", "portal-1", "Desk Clerk");
    let fine = Assignment::new("a2", "portal-1", "Portal Viewer");
    let staff = Assignment::new("a3", "staff-1", "Desk Clerk");
    let input = spot_iam_roles::build_snapshot_input_guarded(
        &r,
        &[forged.clone(), fine, staff],
        at(0),
        &guard,
    )
    .unwrap();
    let held: Vec<&str> = input.assignments["portal-1"]
        .iter()
        .map(|a| a.id.as_str())
        .collect();
    assert_eq!(held, ["a2"]);
    assert!(input.assignments.contains_key("staff-1"));
    assert_eq!(input.refused.len(), 1);
    assert_eq!(input.refused[0].assignment, "a1");
    assert_eq!(input.refused[0].code, codes::OUTSIDE_ROLE_KIND);

    // The unguarded build is unchanged and keeps it (it cannot know).
    let plain = spot_iam_roles::build_snapshot_input(&r, &[forged], at(0)).unwrap();
    assert!(plain.refused.is_empty());
    assert!(plain.assignments.contains_key("portal-1"));
}

proptest! {
    /// For random role graphs of depth up to 8, the guard lets a portal user hold a role only
    /// when every duty its closure reaches is exposed, every role in it is portal, and none
    /// grants directly.
    #[test]
    fn a_portal_role_reaches_exposed_duties_only(
        // Per role: kind portal?, duty picks (bit per duty), direct grant?
        specs in proptest::collection::vec((any::<bool>(), 0u8..8, any::<bool>()), 1..8),
    ) {
        let duty_exposed = [true, false, true];
        let mut text = String::new();
        for (i, exposed) in duty_exposed.iter().enumerate() {
            text.push_str(&format!(
                "[[duty]]\nname = \"d{i}\"\nversion = 1\noutside_exposed = {exposed}\n\
                 [[duty.grants]]\ndoctype = \"customer\"\nrole = \"d{i}\"\nrights = [\"read\"]\n"
            ));
        }
        for (i, (portal, picks, direct)) in specs.iter().enumerate() {
            text.push_str(&format!("[[role]]\nname = \"r{i}\"\norigin = {{ kind = \"custom\" }}\n"));
            if *portal { text.push_str("kind = \"portal\"\n"); }
            let duties: Vec<String> = (0..3)
                .filter(|b| picks & (1 << b) != 0)
                .map(|b| format!("{{ name = \"d{b}\" }}"))
                .collect();
            text.push_str(&format!("duties = [{}]\n", duties.join(", ")));
            if i > 0 { text.push_str(&format!("parents = [\"r{}\"]\n", i - 1)); }
            if *direct {
                text.push_str(&format!(
                    "[[role.direct_grants]]\ndoctype = \"customer\"\nrole = \"r{i}\"\nrights = [\"read\"]\n"
                ));
            }
        }
        let r = Registry::from_toml(&text).unwrap();
        let guard = OutsideGuard::new(&r, ["p"]);
        for (i, _) in specs.iter().enumerate() {
            // The chain r_i -> r_{i-1} -> ... -> r_0.
            let closure = &specs[..=i];
            let expect_ok = closure.iter().all(|(portal, picks, direct)| {
                *portal && !*direct && (0..3).all(|b| picks & (1 << b) == 0 || duty_exposed[b])
            });
            let verdict = guard.check("p", &RoleName::from(format!("r{i}").as_str()));
            prop_assert_eq!(verdict.is_ok(), expect_ok);
            if verdict.is_ok() {
                // A portal user's effective grants come from exposed duties only.
                let role = r.role(&RoleName::from(format!("r{i}").as_str())).unwrap();
                for g in &effective(role, &r).grants {
                    for source in &g.sources {
                        let duty = source.duty.as_ref().expect("no direct grants pass");
                        prop_assert!(r.duty(duty).unwrap().outside_exposed);
                    }
                }
            }
        }
    }
}
