//! Re-checking the caller's own proof for a step-up: the password and, when the user is
//! enrolled, a second-factor code.
//!
//! The sign-in service ([`spot_login::LoginService`]) offers no "check these credentials and
//! start nothing" operation: its `login_*` methods start a session and its `totp_*` helpers are
//! private to `spot-login`. This module therefore re-checks with the same parts the sign-in is
//! built from and adds no cryptography of its own:
//!
//! - the password through [`PasswordHasher`] (the very [`Argon2Hasher`](spot_login::Argon2Hasher)
//!   the sign-in service uses; the parameters of the stored hash decide the work), never a second
//!   hashing implementation;
//! - the code through [`TotpConfig::verify`] (one use per time step: the step is recorded
//!   through the user directory with a compare-and-set, so a code cannot be replayed, here or at
//!   sign-in) and, failing that, a recovery code (consumed through the directory).
//!
//! The order is the sign-in's: the password first, and only a right password reaches the code
//! check, so the refusal of a wrong password is the same whether the account has a second factor
//! or not. Nothing here writes a credential into an error, a log line or an event: a refusal is
//! the closed [`Refusal`] and nothing else.

use std::sync::Arc;

use spot_auth::{AuthError, Password, TotpConfig, verify_recovery_code};
use spot_core::Clock;
use spot_http::api::registry::Code;
use spot_http::api::service::ApiError;
use spot_login::{
    LoginError, LoginErrorKind, PasswordHasher, UserDirectory, UserRecord, invalid_credentials,
};

/// Why a proof was refused. Closed: it carries no value.
#[derive(Debug)]
pub(super) enum Refusal {
    /// The password is wrong (or the user has none).
    BadPassword,
    /// The user is enrolled and sent no code.
    TotpRequired,
    /// The code is wrong, replayed, or a spent recovery code.
    TotpInvalid,
    /// The directory could not answer (or write): the answer to give.
    Failed(ApiError),
}

impl Refusal {
    /// The registry code the caller sees, for the audit line.
    pub(super) fn code(&self) -> &'static str {
        match self {
            Refusal::BadPassword => Code::AuthInvalidCredentials,
            Refusal::TotpRequired => Code::AuthTotpRequired,
            Refusal::TotpInvalid => Code::AuthTotpInvalid,
            Refusal::Failed(_) => Code::ServerUnavailable,
        }
        .as_str()
    }

    /// The answer the caller gets.
    pub(super) fn into_api_error(self) -> ApiError {
        match self {
            Refusal::BadPassword => invalid_credentials(),
            Refusal::TotpRequired => {
                LoginError::new(LoginErrorKind::TotpRequired, "").to_api_error()
            }
            Refusal::TotpInvalid => LoginError::new(LoginErrorKind::TotpInvalid, "").to_api_error(),
            Refusal::Failed(error) => error,
        }
    }
}

/// The checker of one site.
pub(super) struct Verifier {
    hasher: Arc<dyn PasswordHasher>,
    totp: TotpConfig,
    clock: Arc<dyn Clock>,
}

impl Verifier {
    pub(super) fn new(
        hasher: Arc<dyn PasswordHasher>,
        totp: TotpConfig,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            hasher,
            totp,
            clock,
        }
    }

    /// Checks the caller's password and, when `user` has a second factor, `code`. The throttle
    /// is the caller's: it admitted this attempt before the call.
    pub(super) async fn check(
        &self,
        directory: &dyn UserDirectory,
        mut user: UserRecord,
        password: &str,
        code: Option<&str>,
        request_id: &str,
    ) -> Result<(), Refusal> {
        let given = Password::new(password);
        let right = user
            .password_hash
            .as_ref()
            .is_some_and(|hash| self.hasher.verify(&given, hash));
        if !right {
            return Err(Refusal::BadPassword);
        }
        let Some(secret) = user.totp_secret.take() else {
            // No second factor: the password is the whole proof.
            return Ok(());
        };
        let Some(code) = code else {
            return Err(Refusal::TotpRequired);
        };
        match self
            .totp
            .verify(&secret, code, self.clock.as_ref(), user.totp_last_step)
        {
            Ok(step) => match directory.record_totp_step(user.id, step).await {
                Ok(()) => Ok(()),
                // Lost a race with a parallel use of the same code.
                Err(error) if error.kind == LoginErrorKind::TotpReplayed => {
                    Err(Refusal::TotpInvalid)
                }
                Err(error) => Err(Refusal::Failed(error.to_api_error_traced(request_id))),
            },
            Err(AuthError::TotpReplayed) => Err(Refusal::TotpInvalid),
            Err(_) => match verify_recovery_code(code, &user.recovery_digests) {
                Some(index) => match directory.consume_recovery_digest(user.id, index).await {
                    Ok(()) => Ok(()),
                    Err(error)
                        if matches!(
                            error.kind,
                            LoginErrorKind::RecoveryCodeUsed | LoginErrorKind::RecoveryCodeInvalid
                        ) =>
                    {
                        Err(Refusal::TotpInvalid)
                    }
                    Err(error) => Err(Refusal::Failed(error.to_api_error_traced(request_id))),
                },
                None => Err(Refusal::TotpInvalid),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::Mutex;

    use spot_auth::{
        HashParams, PasswordHash, StoredDigest, TotpAlgorithm, TotpDigits, TotpSecret,
        hash_password,
    };
    use spot_core::ManualClock;
    use spot_iam_directory::UserType;
    use spot_login::{Argon2Hasher, BoxFuture, UserState};
    use spot_storage_contract::RecordId;
    use time::OffsetDateTime;

    use super::*;

    const PASSWORD: &str = "correct horse battery staple 7731";
    /// The RFC 6238 seed; at counters 1, 2 and 3 its codes are these.
    const RFC_SEED: &str = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";
    const STEP_2: &str = "359152";
    const STEP_3: &str = "969429";

    /// A directory that only keeps the last recorded step.
    #[derive(Default)]
    struct Steps {
        recorded: Mutex<Vec<u64>>,
        fail: bool,
    }

    impl UserDirectory for Steps {
        fn find_by_identifier<'a>(
            &'a self,
            _identifier: &'a str,
        ) -> BoxFuture<'a, Result<Option<UserRecord>, LoginError>> {
            Box::pin(async { Ok(None) })
        }

        fn find_by_id<'a>(
            &'a self,
            _id: RecordId,
        ) -> BoxFuture<'a, Result<Option<UserRecord>, LoginError>> {
            Box::pin(async { Ok(None) })
        }

        fn set_password_hash<'a>(
            &'a self,
            _id: RecordId,
            _hash: PasswordHash,
        ) -> BoxFuture<'a, Result<(), LoginError>> {
            Box::pin(async { Ok(()) })
        }

        fn record_totp_step<'a>(
            &'a self,
            _id: RecordId,
            step: u64,
        ) -> BoxFuture<'a, Result<(), LoginError>> {
            Box::pin(async move {
                if self.fail {
                    return Err(LoginError::new(LoginErrorKind::StoreFailed, "down"));
                }
                self.recorded.lock().unwrap().push(step);
                Ok(())
            })
        }

        fn consume_recovery_digest<'a>(
            &'a self,
            _id: RecordId,
            _index: usize,
        ) -> BoxFuture<'a, Result<(), LoginError>> {
            Box::pin(async { Ok(()) })
        }

        fn set_second_factor<'a>(
            &'a self,
            _id: RecordId,
            _totp: Option<TotpSecret>,
            _recovery: Vec<StoredDigest>,
        ) -> BoxFuture<'a, Result<(), LoginError>> {
            Box::pin(async { Ok(()) })
        }
    }

    fn user(enrolled: bool, last_step: Option<u64>, has_password: bool) -> UserRecord {
        UserRecord {
            id: RecordId::new_v7(),
            user_name: "ada".to_owned(),
            email: None,
            full_name: "Ada".to_owned(),
            state: UserState::Active,
            user_type: UserType::Internal,
            password_hash: has_password
                .then(|| hash_password(&Password::new(PASSWORD), &HashParams::for_tests())),
            totp_secret: enrolled.then(|| TotpSecret::from_base32(RFC_SEED).unwrap()),
            recovery_digests: Vec::new(),
            totp_last_step: last_step,
        }
    }

    fn verifier() -> Verifier {
        // The clock stands at second 60: counter 2, with one step of skew either side.
        let clock = Arc::new(ManualClock::new(
            OffsetDateTime::from_unix_timestamp(60).unwrap(),
        ));
        Verifier::new(
            Arc::new(Argon2Hasher::new(HashParams::for_tests())),
            TotpConfig {
                algorithm: TotpAlgorithm::Sha1,
                digits: TotpDigits::Six,
                step_seconds: 30,
                skew_steps: 1,
            },
            clock,
        )
    }

    async fn run(
        directory: &Steps,
        user: UserRecord,
        password: &str,
        code: Option<&str>,
    ) -> Result<(), Refusal> {
        verifier()
            .check(directory, user, password, code, "req-1")
            .await
    }

    #[tokio::test]
    async fn a_right_password_is_the_whole_proof_without_a_second_factor() {
        let dir = Steps::default();
        assert!(
            run(&dir, user(false, None, true), PASSWORD, None)
                .await
                .is_ok()
        );
        assert!(dir.recorded.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_wrong_password_is_the_same_refusal_with_or_without_a_second_factor() {
        let dir = Steps::default();
        for enrolled in [false, true] {
            let refusal = run(&dir, user(enrolled, None, true), "not it", Some(STEP_2))
                .await
                .unwrap_err();
            assert!(matches!(refusal, Refusal::BadPassword));
            assert_eq!(refusal.code(), "auth.invalid_credentials");
        }
        // A user with no password has no proof either.
        let refusal = run(&dir, user(false, None, false), PASSWORD, None)
            .await
            .unwrap_err();
        assert!(matches!(refusal, Refusal::BadPassword));
        assert!(dir.recorded.lock().unwrap().is_empty(), "no code was spent");
    }

    #[tokio::test]
    async fn an_enrolled_user_needs_the_code_after_the_password() {
        let dir = Steps::default();
        let refusal = run(&dir, user(true, None, true), PASSWORD, None)
            .await
            .unwrap_err();
        assert!(matches!(refusal, Refusal::TotpRequired));
        assert_eq!(refusal.code(), "auth.totp_required");
        let refusal = run(&dir, user(true, None, true), PASSWORD, Some("000000"))
            .await
            .unwrap_err();
        assert!(matches!(refusal, Refusal::TotpInvalid));
        assert_eq!(refusal.code(), "auth.totp_invalid");
        assert!(dir.recorded.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_right_code_is_accepted_once_and_its_step_is_recorded() {
        let dir = Steps::default();
        assert!(
            run(&dir, user(true, None, true), PASSWORD, Some(STEP_2))
                .await
                .is_ok()
        );
        assert_eq!(*dir.recorded.lock().unwrap(), vec![2]);
        // The same step used before: replayed, refused, nothing recorded.
        let refusal = run(&dir, user(true, Some(2), true), PASSWORD, Some(STEP_2))
            .await
            .unwrap_err();
        assert!(matches!(refusal, Refusal::TotpInvalid));
        // A later step is fine.
        assert!(
            run(&dir, user(true, Some(2), true), PASSWORD, Some(STEP_3))
                .await
                .is_ok()
        );
        assert_eq!(*dir.recorded.lock().unwrap(), vec![2, 3]);
    }

    #[tokio::test]
    async fn a_directory_that_cannot_record_the_step_refuses_with_its_answer() {
        let dir = Steps {
            fail: true,
            ..Steps::default()
        };
        let refusal = run(&dir, user(true, None, true), PASSWORD, Some(STEP_2))
            .await
            .unwrap_err();
        assert!(matches!(refusal, Refusal::Failed(_)));
    }

    #[test]
    fn a_refusal_carries_no_value() {
        for refusal in [
            Refusal::BadPassword,
            Refusal::TotpRequired,
            Refusal::TotpInvalid,
        ] {
            let shown = format!("{refusal:?}");
            assert!(
                !shown.contains(PASSWORD) && !shown.contains(STEP_2),
                "{shown}"
            );
        }
    }
}
