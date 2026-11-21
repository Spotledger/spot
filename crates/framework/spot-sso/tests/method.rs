//! The no-weaker-method rule, delegated to `spot-iam-access`.

use spot_sso::{AuthMethod, TargetSignIn, session_method_change};

#[test]
fn an_sso_session_may_not_switch_to_a_user_without_sso() {
    let target = TargetSignIn {
        method: AuthMethod::Password,
        allows_sso: false,
    };
    let error = session_method_change(AuthMethod::Sso, &target);
    assert_eq!(error, Err(spot_sso::MethodError::SsoNotAllowed));
}

#[test]
fn a_session_may_not_move_to_a_weaker_method() {
    let target = TargetSignIn {
        method: AuthMethod::Password,
        allows_sso: true,
    };
    assert!(matches!(
        session_method_change(AuthMethod::PasswordTotp, &target),
        Err(spot_sso::MethodError::Weaker { .. })
    ));
}

#[test]
fn a_session_may_stay_or_strengthen() {
    let stay = TargetSignIn {
        method: AuthMethod::Sso,
        allows_sso: true,
    };
    assert_eq!(session_method_change(AuthMethod::Sso, &stay), Ok(()));
    let stronger = TargetSignIn {
        method: AuthMethod::Hardware,
        allows_sso: true,
    };
    assert_eq!(
        session_method_change(AuthMethod::Password, &stronger),
        Ok(())
    );
}
