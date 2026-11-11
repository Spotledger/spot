//! Impersonation, step-up and the method ladder.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;

use spot_events_contract::Actor;
use spot_iam_access::{
    AuthMethod, DEFAULT_STEP_UP_OPERATIONS, Impersonation, ImpersonationError, ImpersonationPolicy,
    ImpersonationRefusal as R, ImpersonationTarget, MethodError, MethodGuard, OperationName,
    PrincipalClass, StepUpPolicy, StripReason, SupportPrincipal, TargetSignIn, codes,
};
use spot_iam_roles::{Registry, Role, RoleName};
use spot_permissions::{AssignmentContext, Principal, Role as GrantRole, TENANT_ADMINISTRATOR};
use spot_storage_contract::RecordId;
use spot_support_session::{
    ActionScope, DocTypeScope, Scope, SessionGrant, SessionPolicy, SessionSpec, Subject,
    SubjectKind, Tier,
};

const T0: i64 = 1_000;

fn spec(actions: ActionScope, ceiling: Tier) -> SessionSpec {
    SessionSpec {
        id: "grant-1".to_owned(),
        tenant: "acme".to_owned(),
        subject: Subject::new(SubjectKind::Person, "support-1"),
        tier_ceiling: ceiling,
        scope: Scope {
            doctypes: DocTypeScope::Named(vec!["sales_invoice".to_owned()]),
            actions,
        },
        reason: "customer asked us to look at their invoice screen".to_owned(),
        requested_by: "support-1".to_owned(),
        starts_at: T0,
        expires_at: T0 + 7200,
    }
}

fn grant_with(actions: ActionScope, ceiling: Tier) -> SessionGrant {
    let pending =
        SessionGrant::request(&spec(actions, ceiling), &SessionPolicy::default(), T0).unwrap();
    let approver = spot_support_session::Approver::new("owner-1", true);
    let (grant, _event) = pending
        .approve(&approver, T0, &SessionPolicy::default())
        .unwrap();
    grant
}

fn grant() -> SessionGrant {
    grant_with(ActionScope::Only(vec!["impersonate".to_owned()]), Tier::T1)
}

fn support() -> SupportPrincipal {
    SupportPrincipal {
        id: "support-1".to_owned(),
        class: PrincipalClass::Support,
    }
}

fn assignment(id: &str, roles: &[&str]) -> AssignmentContext {
    AssignmentContext::unscoped(id, roles)
}

fn target(assignments: Vec<AssignmentContext>) -> ImpersonationTarget {
    ImpersonationTarget {
        id: "user-9".to_owned(),
        tenant: "acme".to_owned(),
        class: PrincipalClass::Ordinary,
        enabled: true,
        principal: Principal::user("user-9", assignments),
        emergency_assignments: BTreeSet::new(),
    }
}

fn begin(t: &ImpersonationTarget) -> Result<spot_iam_access::Impersonated, ImpersonationError> {
    Impersonation::begin(
        &grant(),
        &support(),
        t,
        &ImpersonationPolicy::default(),
        T0 + 10,
    )
}

#[test]
fn an_ordinary_user_is_impersonated_with_both_identities() {
    let t = target(vec![assignment("a1", &["Clerk"])]);
    let imp = begin(&t).unwrap();
    assert_eq!(imp.principal().user, "user-9");
    assert_eq!(imp.principal().assignments, t.principal.assignments);
    assert!(imp.stripped().is_empty());
    let id = imp.identity();
    assert_eq!(
        (id.acting.as_str(), id.as_user.as_str()),
        ("support-1", "user-9")
    );
    let trail = imp.trail();
    assert_eq!(trail.acting, "support-1");
    assert_eq!(trail.as_user, "user-9");
    assert_eq!(trail.grant_id, "grant-1");
    assert_eq!(trail.tenant, "acme");
}

#[test]
fn the_actor_carries_both_identities_in_the_events_form() {
    let a = RecordId::from_bytes([1, 2, 3, 4, 5, 6, 0x70, 8, 0x80, 1, 2, 3, 4, 5, 6, 7]).unwrap();
    let b = RecordId::from_bytes([9, 2, 3, 4, 5, 6, 0x70, 8, 0x80, 1, 2, 3, 4, 5, 6, 7]).unwrap();
    let identity = spot_iam_access::Identity {
        acting: a.to_string(),
        as_user: b.to_string(),
    };
    assert_eq!(
        identity.actor(),
        Some(Actor::User {
            id: b,
            via: Some(a)
        })
    );
    let bad = spot_iam_access::Identity {
        acting: "support-1".to_owned(),
        as_user: "user-9".to_owned(),
    };
    assert_eq!(bad.actor(), None);
}

#[test]
fn administrator_and_break_glass_roles_are_stripped_and_listed() {
    let t = target(vec![
        assignment("a1", &["Clerk", TENANT_ADMINISTRATOR]),
        assignment("a2", &["Emergency Operator"]),
        assignment("a3", &[TENANT_ADMINISTRATOR]),
    ]);
    let policy = ImpersonationPolicy::default().with_break_glass(["Emergency Operator".to_owned()]);
    let imp = Impersonation::begin(&grant(), &support(), &t, &policy, T0 + 1).unwrap();
    let roles: Vec<(&str, &str, &StripReason)> = imp
        .stripped()
        .iter()
        .map(|s| (s.assignment.as_str(), s.role.as_str(), &s.reason))
        .collect();
    assert_eq!(
        roles,
        vec![
            (
                "a1",
                TENANT_ADMINISTRATOR,
                &StripReason::TenantAdministrator
            ),
            ("a2", "Emergency Operator", &StripReason::BreakGlass),
            (
                "a3",
                TENANT_ADMINISTRATOR,
                &StripReason::TenantAdministrator
            ),
        ]
    );
    // What is left is a subset of what the target held, and holds no stripped role.
    assert_eq!(
        imp.principal().assignments,
        vec![assignment("a1", &["Clerk"])]
    );
    assert_eq!(imp.trail().stripped_roles.len(), 3);
}

#[test]
fn the_administrator_role_is_stripped_even_if_the_policy_forgets_it() {
    let t = target(vec![assignment("a1", &[TENANT_ADMINISTRATOR])]);
    let mut policy = ImpersonationPolicy::default();
    policy.forbidden_roles.clear();
    let imp = Impersonation::begin(&grant(), &support(), &t, &policy, T0 + 1).unwrap();
    assert!(imp.principal().assignments.is_empty());
}

#[test]
fn an_emergency_assignment_is_stripped_whole() {
    let mut t = target(vec![
        assignment("a1", &["Clerk"]),
        assignment("em", &["Clerk", "Auditor"]),
    ]);
    t.emergency_assignments.insert("em".to_owned());
    let imp = begin(&t).unwrap();
    assert_eq!(
        imp.principal().assignments,
        vec![assignment("a1", &["Clerk"])]
    );
    assert!(
        imp.stripped()
            .iter()
            .all(|s| s.assignment == "em" && s.reason == StripReason::EmergencyAssignment)
    );
    assert_eq!(imp.stripped().len(), 2);
}

#[test]
fn roles_inheriting_the_administrator_are_stripped_through_any_depth() {
    let mut admin = Role::custom(TENANT_ADMINISTRATOR);
    admin.parents = vec![];
    let mut mid = Role::custom("Mid");
    mid.parents = vec![RoleName::from(TENANT_ADMINISTRATOR)];
    let mut top = Role::custom("Top");
    top.parents = vec![RoleName::from("Mid")];
    let plain = Role::custom("Plain");
    let registry = Registry::new(vec![], vec![admin, mid, top, plain]).unwrap();
    let policy = ImpersonationPolicy::default().with_registry(&registry);
    let t = target(vec![assignment("a1", &["Top", "Mid", "Plain"])]);
    let imp = Impersonation::begin(&grant(), &support(), &t, &policy, T0 + 1).unwrap();
    assert_eq!(
        imp.principal().assignments,
        vec![assignment("a1", &["Plain"])]
    );
    let stripped: Vec<&str> = imp.stripped().iter().map(|s| s.role.as_str()).collect();
    assert_eq!(stripped, vec!["Mid", "Top"]);
    assert!(imp.stripped().iter().all(|s| matches!(&s.reason, StripReason::InheritsForbidden { ancestor } if ancestor == TENANT_ADMINISTRATOR)));
}

#[test]
fn nothing_is_added_and_the_system_principal_is_refused() {
    let t = target(vec![]);
    let imp = begin(&t).unwrap();
    assert!(imp.principal().assignments.is_empty());
    assert!(!imp.principal().is_system);
    let mut sys = target(vec![]);
    sys.principal = Principal::system();
    assert_eq!(begin(&sys).unwrap_err().refusal(), Some(&R::TargetIsSystem));
}

#[test]
fn only_the_support_tier_may_begin() {
    let t = target(vec![assignment("a1", &["Clerk"])]);
    for class in [
        PrincipalClass::TenantAdministrator,
        PrincipalClass::Ordinary,
    ] {
        let who = SupportPrincipal {
            id: "support-1".to_owned(),
            class,
        };
        let e = Impersonation::begin(&grant(), &who, &t, &ImpersonationPolicy::default(), T0 + 1)
            .unwrap_err();
        assert_eq!(e.code(), codes::IMPERSONATION_NOT_ALLOWED);
        assert_eq!(e.refusal(), Some(&R::NotSupportTier));
    }
}

#[test]
fn self_support_disabled_and_foreign_tenant_targets_are_refused() {
    let mut t = target(vec![assignment("a1", &["Clerk"])]);
    t.id = "support-1".to_owned();
    assert_eq!(
        begin(&t).unwrap_err().refusal(),
        Some(&R::SelfImpersonation)
    );

    let mut t = target(vec![]);
    t.class = PrincipalClass::Support;
    assert_eq!(begin(&t).unwrap_err().refusal(), Some(&R::TargetIsSupport));

    let mut t = target(vec![]);
    t.enabled = false;
    let e = begin(&t).unwrap_err();
    assert_eq!(e.code(), codes::IMPERSONATION_NOT_ALLOWED);
    assert_eq!(e.refusal(), Some(&R::TargetDisabled));

    let mut t = target(vec![]);
    t.tenant = "other".to_owned();
    assert_eq!(begin(&t).unwrap_err().refusal(), Some(&R::WrongTenant));
}

#[test]
fn the_error_text_does_not_depend_on_the_target() {
    let mut a = target(vec![]);
    a.enabled = false;
    let mut b = target(vec![]);
    b.class = PrincipalClass::Support;
    assert_eq!(
        begin(&a).unwrap_err().to_string(),
        begin(&b).unwrap_err().to_string()
    );
}

#[test]
fn the_grant_must_be_valid_approved_and_cover_impersonation() {
    let t = target(vec![assignment("a1", &["Clerk"])]);
    let policy = ImpersonationPolicy::default();
    let go = |g: &SessionGrant, who: &SupportPrincipal, now: i64| {
        Impersonation::begin(g, who, &t, &policy, now)
    };

    // Before the window and after it.
    let e = go(&grant(), &support(), T0 - 1).unwrap_err();
    assert_eq!(
        (e.code(), e.refusal()),
        (codes::IMPERSONATION_GRANT_INVALID, Some(&R::GrantNotActive))
    );
    assert_eq!(
        go(&grant(), &support(), T0 + 7200).unwrap_err().refusal(),
        Some(&R::GrantNotActive)
    );

    // Not approved.
    let pending = SessionGrant::request(
        &spec(ActionScope::Any, Tier::T1),
        &SessionPolicy::default(),
        T0,
    )
    .unwrap();
    let unapproved = pending.grant().clone();
    assert_eq!(
        go(&unapproved, &support(), T0 + 1).unwrap_err().refusal(),
        Some(&R::GrantNotApproved)
    );

    // Revoked.
    let mut revoked = grant();
    revoked.revoked = Some(spot_support_session::Revocation {
        by: "owner-1".to_owned(),
        at: T0 + 5,
        reason: "changed our mind".to_owned(),
    });
    assert_eq!(
        go(&revoked, &support(), T0 + 10).unwrap_err().refusal(),
        Some(&R::GrantRevoked)
    );

    // Another subject's grant.
    let other = SupportPrincipal {
        id: "support-2".to_owned(),
        class: PrincipalClass::Support,
    };
    assert_eq!(
        go(&grant(), &other, T0 + 1).unwrap_err().refusal(),
        Some(&R::GrantSubjectMismatch)
    );

    // A grant that does not name impersonation.
    let narrow = grant_with(ActionScope::Only(vec!["explain".to_owned()]), Tier::T1);
    let e = go(&narrow, &support(), T0 + 1).unwrap_err();
    assert_eq!(e.code(), codes::IMPERSONATION_GRANT_INVALID);
    assert!(matches!(
        e.refusal(),
        Some(R::GrantDoesNotCover("session.action_not_in_scope"))
    ));

    // A ceiling below the tier impersonation needs.
    let low = grant_with(ActionScope::Any, Tier::T0);
    assert!(matches!(
        go(&low, &support(), T0 + 1).unwrap_err().refusal(),
        Some(R::GrantDoesNotCover("session.above_ceiling"))
    ));

    // A grant for another tenant.
    let mut foreign = grant();
    foreign.tenant = "other".to_owned();
    assert_eq!(
        go(&foreign, &support(), T0 + 1).unwrap_err().code(),
        codes::IMPERSONATION_NOT_ALLOWED
    );

    // Self-approved.
    let mut selfie = grant();
    selfie.approved_by = "support-1".to_owned();
    assert_eq!(
        go(&selfie, &support(), T0 + 1).unwrap_err().refusal(),
        Some(&R::GrantSelfApproved)
    );

    // A grant of Any actions works.
    assert!(go(&grant_with(ActionScope::Any, Tier::T1), &support(), T0 + 1).is_ok());
}

#[test]
fn the_impersonation_ends_with_the_grant_or_the_policy_maximum() {
    let t = target(vec![assignment("a1", &["Clerk"])]);
    let g = grant();
    let policy = ImpersonationPolicy {
        max_duration_secs: 600,
        ..ImpersonationPolicy::default()
    };
    let imp = Impersonation::begin(&g, &support(), &t, &policy, T0 + 100).unwrap();
    assert_eq!(imp.valid_until(), T0 + 700);
    assert!(imp.check(T0 + 699).is_ok());
    assert_eq!(imp.check(T0 + 700), Err(ImpersonationError::Expired));
    assert_eq!(
        ImpersonationError::Expired.code(),
        codes::IMPERSONATION_EXPIRED
    );

    // Near the end of the grant, the grant wins.
    let late = Impersonation::begin(&g, &support(), &t, &policy, T0 + 7000).unwrap();
    assert_eq!(late.valid_until(), g.expires_at);
    let huge = ImpersonationPolicy {
        max_duration_secs: i64::MAX,
        ..ImpersonationPolicy::default()
    };
    assert_eq!(
        Impersonation::begin(&g, &support(), &t, &huge, T0 + 1)
            .unwrap()
            .valid_until(),
        g.expires_at
    );
}

#[test]
fn the_trail_serialises_for_the_audit_record() {
    let imp = begin(&target(vec![assignment("a1", &[TENANT_ADMINISTRATOR])])).unwrap();
    let json = serde_json::to_value(imp.trail()).unwrap();
    assert_eq!(json["acting"], "support-1");
    assert_eq!(json["as_user"], "user-9");
    assert_eq!(
        json["stripped_roles"][0],
        format!("a1:{TENANT_ADMINISTRATOR}")
    );
    let _ = GrantRole(String::new());
}

// ---------------------------------------------------------------- step-up and methods

#[test]
fn the_default_list_is_present_and_cannot_be_shrunk() {
    let p = StepUpPolicy::default();
    for op in DEFAULT_STEP_UP_OPERATIONS {
        assert!(p.operations().contains(&OperationName::new(op)));
    }
    assert_eq!(DEFAULT_STEP_UP_OPERATIONS.len(), 6);
    // Loading an empty list, or a list without the defaults, still has them.
    let loaded: StepUpPolicy =
        serde_json::from_str(r#"{"operations":[],"max_age_seconds":60}"#).unwrap();
    assert_eq!(loaded.operations().len(), 6);
    let custom: StepUpPolicy = serde_json::from_str(r#"{"operations":["post_payroll"]}"#).unwrap();
    assert_eq!(custom.operations().len(), 7);
    assert!(
        custom
            .operations()
            .contains(&OperationName::new("post_payroll"))
    );
    let extended = StepUpPolicy::default().with_operation(OperationName::new("x"));
    assert_eq!(extended.operations().len(), 7);
}

#[test]
fn step_up_depends_on_the_operation_and_the_age() {
    let p = StepUpPolicy::new([OperationName::new("post_payroll")], 300);
    let export = OperationName::new("data_export");
    assert!(!p.needs_step_up(&export, 1000, 1300));
    assert!(p.needs_step_up(&export, 1000, 1301));
    assert!(
        p.needs_step_up(&export, 2000, 1000),
        "a sign-in from the future is stale"
    );
    assert!(p.needs_step_up(&OperationName::new("post_payroll"), 0, 10_000));
    assert!(!p.needs_step_up(&OperationName::new("read_list"), 0, 10_000));
    assert_eq!(
        p.require(&export, 0, 10_000).unwrap_err().code(),
        codes::STEP_UP_REQUIRED
    );
    assert!(p.require(&export, 9_900, 10_000).is_ok());
    // The age is capped, and never negative.
    assert_eq!(StepUpPolicy::new([], 1_000_000).max_age_seconds(), 3600);
    assert_eq!(StepUpPolicy::new([], -5).max_age_seconds(), 0);
}

#[test]
fn the_method_ladder() {
    use AuthMethod::{Hardware, Password, PasswordTotp, Sso};
    let allow = |method| TargetSignIn {
        method,
        allows_sso: true,
    };
    // Same or stronger is fine.
    for (from, to) in [
        (Password, Password),
        (Password, PasswordTotp),
        (Password, Sso),
        (PasswordTotp, Hardware),
        (Sso, Hardware),
        (Hardware, Sso),
        (Sso, Sso),
    ] {
        assert_eq!(
            MethodGuard::may_switch(from, &allow(to)),
            Ok(()),
            "{from:?} -> {to:?}"
        );
    }
    // Weaker is refused.
    for (from, to) in [
        (PasswordTotp, Password),
        (Sso, PasswordTotp),
        (Hardware, Password),
        (Sso, Password),
    ] {
        let e = MethodGuard::may_switch(from, &allow(to)).unwrap_err();
        assert_eq!(e, MethodError::Weaker { from, to });
        assert_eq!(e.code(), codes::METHOD_WEAKER);
    }
    assert!(Password.is_weaker_than(PasswordTotp) && PasswordTotp.is_weaker_than(Sso));
    assert!(!Sso.is_weaker_than(Hardware) && !Hardware.is_weaker_than(Sso));
}

#[test]
fn an_sso_session_cannot_switch_to_a_user_without_sso() {
    let no_sso = TargetSignIn {
        method: AuthMethod::Hardware,
        allows_sso: false,
    };
    let e = MethodGuard::may_switch(AuthMethod::Sso, &no_sso).unwrap_err();
    assert_eq!(e, MethodError::SsoNotAllowed);
    assert_eq!(e.code(), codes::METHOD_SSO_NOT_ALLOWED);
    // A hardware session may switch to that user.
    assert!(MethodGuard::may_switch(AuthMethod::Hardware, &no_sso).is_ok());
}
