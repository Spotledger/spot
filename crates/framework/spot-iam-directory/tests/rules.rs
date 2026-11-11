//! The portal and service rules.
#![allow(missing_docs)]

use spot_iam_directory::{
    RoleKind, SignInMethod, UserType, codes, validate_assignment_kind, validate_sign_in_method,
};

const TYPES: [UserType; 4] = [
    UserType::Internal,
    UserType::Portal,
    UserType::Service,
    UserType::Agent,
];

#[test]
fn a_portal_user_holds_no_desk_role() {
    let e = validate_assignment_kind(UserType::Portal, RoleKind::Desk)
        .err()
        .unwrap_or_else(|| panic!("must fail"));
    assert_eq!(e.codes(), [codes::PORTAL_ROLE]);
    assert!(validate_assignment_kind(UserType::Portal, RoleKind::Portal).is_ok());
    for ty in [UserType::Internal, UserType::Service, UserType::Agent] {
        for role in [RoleKind::Desk, RoleKind::Portal] {
            assert!(
                validate_assignment_kind(ty, role).is_ok(),
                "{ty:?} {role:?}"
            );
        }
    }
}

#[test]
fn service_and_agent_users_cannot_sign_in_with_a_password() {
    for ty in [UserType::Service, UserType::Agent] {
        let e = validate_sign_in_method(ty, SignInMethod::Password)
            .err()
            .unwrap_or_else(|| panic!("{ty:?} must fail"));
        assert_eq!(e.codes(), [codes::NO_PASSWORD]);
    }
    for ty in TYPES {
        for m in [
            SignInMethod::Sso,
            SignInMethod::MagicLink,
            SignInMethod::ApiKey,
            SignInMethod::ClientCredentials,
        ] {
            assert!(validate_sign_in_method(ty, m).is_ok(), "{ty:?} {m:?}");
        }
    }
    for ty in [UserType::Internal, UserType::Portal] {
        assert!(validate_sign_in_method(ty, SignInMethod::Password).is_ok());
    }
}

// ---- The portal rule as an allow-list (ADR-083 Barrier 3, WP-IAM-11.T4) ----

mod allow_list {
    use spot_iam_directory::{UserType, validate_assignment};
    use spot_iam_roles::{Registry, RoleName, effective};

    const FIXTURE: &str = include_str!("../../spot-iam-roles/tests/fixtures/outside.toml");

    fn verdict(user: UserType, role: &str) -> Result<(), String> {
        let registry = Registry::from_toml(FIXTURE).unwrap_or_else(|e| panic!("{e}"));
        let role = registry
            .role(&RoleName::from(role))
            .unwrap_or_else(|| panic!("no role {role}"));
        validate_assignment(user, &effective(role, &registry))
            .map_err(|e| format!("{}|{e}", e.codes().join(",")))
    }

    #[test]
    fn a_portal_user_holds_only_a_fully_exposed_portal_role() {
        // (role, portal outcome: None = allowed, Some(code) = refused)
        let table: [(&str, Option<&str>); 8] = [
            ("Desk Clerk", Some("iam_outside.role_kind")),
            ("Desk Exposed Only", Some("iam_outside.role_kind")),
            ("Portal Viewer", None),
            ("Portal Leaky", Some("iam_outside.duty_not_exposed")),
            (
                "Portal Inherits Leaky",
                Some("iam_outside.duty_not_exposed"),
            ),
            ("Portal Over Desk", Some("iam_outside.role_kind")),
            ("Portal Direct", Some("iam_outside.duty_not_exposed")),
            ("Portal Old Duty", Some("iam_outside.duty_not_exposed")),
        ];
        for (role, expected) in table {
            let got = verdict(UserType::Portal, role);
            match expected {
                None => assert!(got.is_ok(), "{role}: {got:?}"),
                Some(code) => {
                    let message = got
                        .err()
                        .unwrap_or_else(|| panic!("{role} must be refused"));
                    assert!(message.starts_with(code), "{role}: {message}");
                    assert!(message.contains(role), "{role}: {message}");
                }
            }
            // The same role is no business of this rule for any other kind of user.
            for other in [UserType::Internal, UserType::Service, UserType::Agent] {
                assert!(verdict(other, role).is_ok(), "{other:?} {role}");
            }
        }
    }
}
