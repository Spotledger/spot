//! The tenant sign-in policy: break-glass checks, local decisions and lock-out protection.

mod support;

use spot_sso::{
    LocalDecision, ProviderId, SecondFactor, SignInMode, SignInPolicy, check_policy,
    decide_local_signin, decide_sso_signin, validate_change,
};

use support::*;

/// A break-glass-eligible administrator.
fn admin(dir: &mut FakeDirectory, name: &str) -> spot_storage_contract::RecordId {
    let id = dir.add(user(new_id(), name));
    dir.make_break_glass(id);
    id
}

/// The provider id the fake directory trusts.
fn provider() -> ProviderId {
    ProviderId::new("acme")
}

#[test]
fn required_sso_with_no_break_glass_is_unsafe() {
    let dir = FakeDirectory::new();
    let policy = SignInPolicy {
        mode: SignInMode::SsoRequired,
        providers: vec![provider()],
        break_glass: Vec::new(),
        ..SignInPolicy::local()
    };
    let problems = check_policy(&policy, &dir);
    assert!(
        problems
            .iter()
            .any(|p| p.code() == "sso.break_glass_too_few")
    );
}

#[test]
fn required_sso_with_one_break_glass_is_unsafe() {
    let mut dir = FakeDirectory::new();
    let one = admin(&mut dir, "admin1");
    dir.healthy_provider(provider());
    let policy = SignInPolicy {
        mode: SignInMode::SsoRequired,
        providers: vec![provider()],
        break_glass: vec![one],
        ..SignInPolicy::local()
    };
    assert!(
        check_policy(&policy, &dir)
            .iter()
            .any(|p| p.code() == "sso.break_glass_too_few")
    );
}

#[test]
fn required_sso_with_two_eligible_break_glass_is_safe() {
    let mut dir = FakeDirectory::new();
    let one = admin(&mut dir, "admin1");
    let two = admin(&mut dir, "admin2");
    dir.healthy_provider(provider());
    let policy = SignInPolicy {
        mode: SignInMode::SsoRequired,
        providers: vec![provider()],
        break_glass: vec![one, two],
        ..SignInPolicy::local()
    };
    assert_eq!(check_policy(&policy, &dir), Vec::new());
}

#[test]
fn a_break_glass_without_a_password_is_reported() {
    let mut dir = FakeDirectory::new();
    let one = admin(&mut dir, "admin1");
    let mut no_password = user(new_id(), "admin2");
    no_password.has_local_password = false;
    let two = dir.add(no_password);
    dir.make_break_glass(two);
    dir.healthy_provider(provider());
    let policy = SignInPolicy {
        mode: SignInMode::SsoRequired,
        providers: vec![provider()],
        break_glass: vec![one, two],
        ..SignInPolicy::local()
    };
    assert!(
        check_policy(&policy, &dir)
            .iter()
            .any(|p| p.code() == "sso.break_glass_no_password")
    );
}

#[test]
fn a_break_glass_without_a_second_factor_is_reported() {
    let mut dir = FakeDirectory::new();
    let mut only_password = user(new_id(), "admin1");
    only_password.second_factors = Vec::new();
    let one = dir.add(only_password);
    dir.make_break_glass(one);
    let two = admin(&mut dir, "admin2");
    dir.healthy_provider(provider());
    let policy = SignInPolicy {
        mode: SignInMode::SsoRequired,
        providers: vec![provider()],
        break_glass: vec![one, two],
        ..SignInPolicy::local()
    };
    assert!(
        check_policy(&policy, &dir)
            .iter()
            .any(|p| p.code() == "sso.break_glass_no_second_factor")
    );
}

#[test]
fn a_break_glass_that_is_not_an_admin_is_reported() {
    let mut dir = FakeDirectory::new();
    let mut not_admin = user(new_id(), "admin1");
    not_admin.is_tenant_admin = false;
    let one = dir.add(not_admin);
    let two = admin(&mut dir, "admin2");
    dir.make_break_glass(one);
    dir.healthy_provider(provider());
    let policy = SignInPolicy {
        mode: SignInMode::SsoRequired,
        providers: vec![provider()],
        break_glass: vec![one, two],
        ..SignInPolicy::local()
    };
    assert!(
        check_policy(&policy, &dir)
            .iter()
            .any(|p| p.code() == "sso.break_glass_not_admin")
    );
}

#[test]
fn required_sso_needs_a_healthy_provider() {
    let mut dir = FakeDirectory::new();
    let one = admin(&mut dir, "admin1");
    let two = admin(&mut dir, "admin2");
    let policy = SignInPolicy {
        mode: SignInMode::SsoRequired,
        providers: vec![provider()],
        break_glass: vec![one, two],
        ..SignInPolicy::local()
    };
    // No provider marked healthy.
    assert!(
        check_policy(&policy, &dir)
            .iter()
            .any(|p| p.code() == "sso.no_provider")
    );
}

#[test]
fn local_signin_is_allowed_when_not_required() {
    for mode in [SignInMode::Local, SignInMode::SsoOptional] {
        let mut dir = FakeDirectory::new();
        let id = dir.add(user(new_id(), "person"));
        let policy = SignInPolicy {
            mode,
            ..SignInPolicy::local()
        };
        assert_eq!(
            decide_local_signin(&policy, &dir, id),
            Ok(LocalDecision::Allowed)
        );
    }
}

#[test]
fn local_signin_is_refused_under_required_for_ordinary_users() {
    let mut dir = FakeDirectory::new();
    let id = dir.add(user(new_id(), "person"));
    let policy = SignInPolicy {
        mode: SignInMode::SsoRequired,
        ..SignInPolicy::local()
    };
    let error = err(decide_local_signin(&policy, &dir, id));
    assert_eq!(error.code(), "sso.required");
}

#[test]
fn break_glass_signin_is_allowed_and_alerts() {
    let mut dir = FakeDirectory::new();
    let id = admin(&mut dir, "admin1");
    let policy = SignInPolicy {
        mode: SignInMode::SsoRequired,
        break_glass: vec![id],
        ..SignInPolicy::local()
    };
    match ok(decide_local_signin(&policy, &dir, id), "decide") {
        LocalDecision::AllowedBreakGlass {
            second_factor_required,
            alert,
        } => {
            assert!(second_factor_required);
            assert_eq!(alert.user, id);
            assert_eq!(alert.level, spot_sso::AlertLevel::Alert);
            assert_eq!(
                alert.reason,
                spot_sso::BreakGlassReason::LocalSignInWhileSsoRequired
            );
        }
        LocalDecision::Allowed => panic!("a break-glass sign-in must alert"),
    }
}

#[test]
fn switching_to_required_without_break_glass_locks_out() {
    let mut dir = FakeDirectory::new();
    let _id = dir.add(user(new_id(), "person"));
    dir.healthy_provider(provider());
    let old = SignInPolicy {
        mode: SignInMode::SsoOptional,
        providers: vec![provider()],
        ..SignInPolicy::local()
    };
    let new = SignInPolicy {
        mode: SignInMode::SsoRequired,
        providers: vec![provider()],
        break_glass: Vec::new(),
        ..SignInPolicy::local()
    };
    let errors = err(validate_change(&old, &new, &dir));
    assert!(errors.has_code("sso.policy_lockout"));
    assert!(errors.has_code("sso.break_glass_too_few"));
}

#[test]
fn sso_is_refused_under_a_local_policy() {
    let policy = SignInPolicy::local();
    let error = err(decide_sso_signin(&policy, &provider()));
    assert_eq!(error.code(), "sso.sso_not_allowed");
}

#[test]
fn an_untrusted_provider_is_refused() {
    let policy = SignInPolicy {
        mode: SignInMode::SsoOptional,
        providers: vec![ProviderId::new("acme")],
        ..SignInPolicy::local()
    };
    let error = err(decide_sso_signin(&policy, &ProviderId::new("other")));
    assert_eq!(error.code(), "sso.provider_not_configured");
}

#[test]
fn a_policy_round_trips_through_json() {
    let mut dir = FakeDirectory::new();
    let one = admin(&mut dir, "admin1");
    let two = admin(&mut dir, "admin2");
    let policy = SignInPolicy {
        mode: SignInMode::SsoRequired,
        providers: vec![provider()],
        break_glass: vec![one, two],
        ..SignInPolicy::local()
    };
    let json = serde_json::to_string(&policy).unwrap_or_default();
    let back: SignInPolicy = ok(serde_json::from_str(&json), "policy round trip");
    assert_eq!(back, policy);
}

#[test]
fn a_second_factor_is_either_totp_or_hardware() {
    let mut user = user(new_id(), "a");
    user.second_factors = vec![SecondFactor::Totp];
    assert!(user.has_strong_second_factor());
    user.second_factors = vec![SecondFactor::Hardware];
    assert!(user.has_strong_second_factor());
    user.second_factors = Vec::new();
    assert!(!user.has_strong_second_factor());
}
