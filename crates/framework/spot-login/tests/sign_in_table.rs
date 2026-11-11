//! The full sign-in table over the **storage-backed** stores: 26 named cases on the in-memory
//! backend and on PostgreSQL.
//!
//! Each case builds a site (users, credentials, sessions and keys as records), runs the real
//! [`LoginService`] over `StorageSessionStore`, `StorageUserDirectory`, `StorageKeyStore` and
//! `StorageThrottleStore`, and asserts the one external answer the native API contract promises.
//!
//! # Two cases the platform app forces to fail closed
//!
//! - `recovery_code_*`: a recovery digest lives in `user_recovery_digest`, a **child** DocType,
//!   and the secret path refuses child rows, so the storage directory stores no recovery digest
//!   (it refuses to write one into a plain column). A presented recovery code is therefore
//!   simply wrong: `auth.totp_invalid`, never a sign-in on an unverifiable code.
//! - `anonymised_user`: the `user.state` select accepts it, and the directory refuses it exactly
//!   like a disabled account.
//!
//! The exact field each gap needs is in the task report.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::storage as st;
use common::{CountingHasher, MemEvents};
use spot_auth::ApiKey;
use spot_http::api::{ApiError, AuthService, Credential, LoginBody, RequestCtx, Session};
use spot_login::{Authenticated, LoginConfig, LoginService, UserDirectory, UserState};
use spot_sessions::{SessionPolicy, ThrottleConfig};
use spot_storage_contract::StorageBackend;
use time::OffsetDateTime;

/// The unix instant every case's clock starts at. At 60 the RFC 6238 seed's step is 2, and
/// `359152` is a valid six-digit code for it (the vector `spot-auth`'s own suite uses).
const NOW: i64 = 60;

/// The RFC 6238 SHA-1 seed `"12345678901234567890"`.
const RFC_SEED: &str = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ";

/// A valid code for [`NOW`] with [`RFC_SEED`].
const RFC_CODE: &str = "359152";

const PASSWORD: &str = "correct horse battery staple 7731";

/// One case of the table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Case {
    RightPassword,
    WrongPassword,
    UnknownUser,
    UnknownUserDoesEqualWork,
    DisabledUser,
    InvitedUser,
    AnonymisedUser,
    LockedAccount,
    BackoffAfterFailures,
    TotpMissing,
    TotpWrong,
    TotpRight,
    TotpReplay,
    RecoveryCodeRefusedWithoutStorage,
    RecoveryCodeIgnoredWithoutFactor,
    ApiKeyGood,
    ApiKeyRevoked,
    ApiKeyExpired,
    ApiKeyOwnerDisabled,
    ApiKeyUnknown,
    EmptyIdentifier,
    UpperCaseIdentifier,
    EmailIdentifier,
    SessionValid,
    SessionRevoked,
    SessionExpired,
}

const ALL: [Case; 26] = [
    Case::RightPassword,
    Case::WrongPassword,
    Case::UnknownUser,
    Case::UnknownUserDoesEqualWork,
    Case::DisabledUser,
    Case::InvitedUser,
    Case::AnonymisedUser,
    Case::LockedAccount,
    Case::BackoffAfterFailures,
    Case::TotpMissing,
    Case::TotpWrong,
    Case::TotpRight,
    Case::TotpReplay,
    Case::RecoveryCodeRefusedWithoutStorage,
    Case::RecoveryCodeIgnoredWithoutFactor,
    Case::ApiKeyGood,
    Case::ApiKeyRevoked,
    Case::ApiKeyExpired,
    Case::ApiKeyOwnerDisabled,
    Case::ApiKeyUnknown,
    Case::EmptyIdentifier,
    Case::UpperCaseIdentifier,
    Case::EmailIdentifier,
    Case::SessionValid,
    Case::SessionRevoked,
    Case::SessionExpired,
];

/// The throttle configuration a case needs: the lockout case must reach the lock before the
/// backoff refuses the attempt, and the backoff case must reach the backoff first.
fn throttle_for(case: Case) -> ThrottleConfig {
    // `lockout_after` must be at least `backoff_after` (the config validates that), and once
    // the backoff refuses an attempt nothing counts any more. So the lockout case reaches the
    // lock exactly at the M-th failure (M == backoff_after), and the backoff case never reaches
    // a lock at all.
    let (backoff_after, lockout_after) = match case {
        Case::LockedAccount => (3, 3),
        Case::BackoffAfterFailures => (2, 5),
        _ => (2, 5),
    };
    ThrottleConfig {
        backoff_after,
        lockout_after,
        window: Duration::from_secs(900),
        backoff_base: Duration::from_secs(1),
        backoff_multiplier: 2,
        backoff_cap: Duration::from_secs(900),
        lockout_duration: Duration::from_secs(1_800),
    }
}

fn config_for(case: Case) -> LoginConfig {
    LoginConfig {
        throttle: throttle_for(case),
        session_policy: SessionPolicy::DEFAULT,
        // No cache: every session check is a store read, so an expiry is visible at once.
        cache_ttl: Duration::ZERO,
        ..LoginConfig::for_tests()
    }
}

fn at(unix: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(unix).expect("a valid instant")
}

/// A site: the service over the storage stores, the hasher (it counts its verifies) and the
/// in-memory event sink.
struct Site<B: StorageBackend> {
    rig: st::Rig<B>,
    service: Arc<LoginService>,
    hasher: Arc<CountingHasher>,
    events: Arc<MemEvents>,
}

impl<B: StorageBackend> Site<B> {
    fn build(rig: st::Rig<B>, case: Case) -> Self {
        let events = Arc::new(MemEvents::default());
        let hasher = Arc::new(CountingHasher::new());
        let service = Arc::new(rig.service_with(config_for(case), events.clone(), hasher.clone()));
        Self {
            rig,
            service,
            hasher,
            events,
        }
    }

    /// A user with [`PASSWORD`] and the given state.
    async fn user(&self, name: &str, state: &str) -> st::RecordId {
        let id = self.rig.user(name, state).await;
        self.rig.set_password(id, PASSWORD).await;
        id
    }

    async fn login(&self, body: LoginBody) -> Result<Session, ApiError> {
        self.service
            .login_with_source(&common::ctx(None), &body, "10.0.0.1")
            .await
    }

    async fn authenticate(&self, token: &str) -> Result<Authenticated, ApiError> {
        let ctx = RequestCtx {
            credential: Some(Credential::new(token)),
            ..common::ctx(None)
        };
        self.service.authenticate(&ctx).await
    }

    fn verifies(&self) -> usize {
        self.hasher.verify_count()
    }

    fn attempts(&self) -> usize {
        common::lock(&self.events.attempts).len()
    }
}

/// A sign-in body with a password.
fn body(user: &str, password: &str) -> LoginBody {
    LoginBody {
        user: user.to_owned(),
        password: password.to_owned(),
        totp: None,
        recovery_code: None,
    }
}

fn code_of(error: &ApiError) -> String {
    error.code.as_str().to_owned()
}

fn refused<T: std::fmt::Debug>(result: Result<T, ApiError>, expected: &str, case: Case) {
    match result {
        Ok(value) => panic!("{case:?}: expected `{expected}`, got a success: {value:?}"),
        Err(error) => assert_eq!(
            code_of(&error),
            expected,
            "{case:?} answered `{}`",
            code_of(&error)
        ),
    }
}

fn accepted<T>(result: Result<T, ApiError>, case: Case) -> T {
    match result {
        Ok(value) => value,
        Err(error) => panic!(
            "{case:?}: expected success, got `{}` ({})",
            code_of(&error),
            error.message
        ),
    }
}

/// Runs one case against a site over `rig`.
async fn run_case<B: StorageBackend>(rig: st::Rig<B>, case: Case) {
    let site = Site::build(rig, case);
    match case {
        Case::RightPassword => {
            site.user("ada", "active").await;
            let session = accepted(site.login(body("ada", PASSWORD)).await, case);
            assert!(!session.token.is_empty(), "{case:?}: no token");
            assert_eq!(site.attempts(), 1, "{case:?}: no login attempt recorded");
        }
        Case::WrongPassword => {
            site.user("ada", "active").await;
            refused(
                site.login(body("ada", "not the password")).await,
                "auth.invalid_credentials",
                case,
            );
        }
        Case::UnknownUser => {
            refused(
                site.login(body("ghost", PASSWORD)).await,
                "auth.invalid_credentials",
                case,
            );
        }
        Case::UnknownUserDoesEqualWork => {
            site.user("ada", "active").await;
            let before = site.verifies();
            let _ = site.login(body("ghost", PASSWORD)).await;
            let unknown = site.verifies() - before;
            let before = site.verifies();
            let _ = site.login(body("ada", "not the password")).await;
            let wrong = site.verifies() - before;
            assert_eq!(
                unknown, wrong,
                "{case:?}: an unknown user did {unknown} hash verifies, a wrong password {wrong}"
            );
            assert_eq!(unknown, 1, "{case:?}: one verification per attempt");
        }
        Case::DisabledUser => {
            site.user("ada", "disabled").await;
            refused(
                site.login(body("ada", PASSWORD)).await,
                "auth.invalid_credentials",
                case,
            );
        }
        Case::InvitedUser => {
            site.user("ada", "invited").await;
            refused(
                site.login(body("ada", PASSWORD)).await,
                "auth.invalid_credentials",
                case,
            );
        }
        Case::AnonymisedUser => {
            site.user("ada", "anonymised").await;
            refused(
                site.login(body("ada", PASSWORD)).await,
                "auth.invalid_credentials",
                case,
            );
            let record = site
                .rig
                .stores
                .user_directory()
                .find_by_identifier("ada")
                .await
                .expect("the lookup runs")
                .expect("the user exists");
            assert_eq!(record.state, UserState::Anonymised);
        }
        Case::LockedAccount => {
            site.user("ada", "active").await;
            for _ in 0..3 {
                let _ = site.login(body("ada", "not the password")).await;
            }
            let locked = site
                .login(body("ada", PASSWORD))
                .await
                .expect_err("the account is locked");
            assert_eq!(code_of(&locked), "api.rate_limited", "{case:?}");
            assert!(
                locked.details.contains_key("retry_after"),
                "{case:?}: no retry_after"
            );
        }
        Case::BackoffAfterFailures => {
            site.user("ada", "active").await;
            for _ in 0..2 {
                let _ = site.login(body("ada", "not the password")).await;
            }
            let backed_off = site
                .login(body("ada", PASSWORD))
                .await
                .expect_err("the attempt is backed off");
            assert_eq!(code_of(&backed_off), "api.rate_limited", "{case:?}");
            assert!(backed_off.details.contains_key("retry_after"));
        }
        Case::TotpMissing => {
            let id = site.user("ada", "active").await;
            site.rig.set_totp(id, RFC_SEED).await;
            refused(
                site.login(body("ada", PASSWORD)).await,
                "auth.totp_required",
                case,
            );
        }
        Case::TotpWrong => {
            let id = site.user("ada", "active").await;
            site.rig.set_totp(id, RFC_SEED).await;
            refused(
                site.login(LoginBody {
                    totp: Some("000000".to_owned()),
                    ..body("ada", PASSWORD)
                })
                .await,
                "auth.totp_invalid",
                case,
            );
        }
        Case::TotpRight => {
            let id = site.user("ada", "active").await;
            site.rig.set_totp(id, RFC_SEED).await;
            let session = accepted(
                site.login(LoginBody {
                    totp: Some(RFC_CODE.to_owned()),
                    ..body("ada", PASSWORD)
                })
                .await,
                case,
            );
            assert!(!session.token.is_empty());
            // The replay counter is durable: the sealed value carries the accepted step.
            let record = site
                .rig
                .stores
                .user_directory()
                .find_by_id(id)
                .await
                .expect("the user reads")
                .expect("the user exists");
            assert!(
                record.totp_last_step.is_some(),
                "{case:?}: the accepted step was not recorded"
            );
        }
        Case::TotpReplay => {
            let id = site.user("ada", "active").await;
            site.rig.set_totp(id, RFC_SEED).await;
            let first = LoginBody {
                totp: Some(RFC_CODE.to_owned()),
                ..body("ada", PASSWORD)
            };
            accepted(site.login(first.clone()).await, case);
            refused(site.login(first).await, "auth.totp_invalid", case);
        }
        Case::RecoveryCodeRefusedWithoutStorage => {
            let id = site.user("ada", "active").await;
            site.rig.set_totp(id, RFC_SEED).await;
            let record = site
                .rig
                .stores
                .user_directory()
                .find_by_id(id)
                .await
                .expect("the user reads")
                .expect("the user exists");
            assert!(
                record.recovery_digests.is_empty(),
                "{case:?}: a recovery digest reached storage"
            );
            refused(
                site.login(LoginBody {
                    recovery_code: Some("aaaa-bbbb".to_owned()),
                    ..body("ada", PASSWORD)
                })
                .await,
                "auth.totp_invalid",
                case,
            );
        }
        Case::RecoveryCodeIgnoredWithoutFactor => {
            site.user("ada", "active").await;
            // A code with no second factor enrolled is not a factor: the password is enough,
            // exactly as `spot-auth`'s recovery path never runs without a stored secret.
            accepted(
                site.login(LoginBody {
                    recovery_code: Some("aaaa-bbbb".to_owned()),
                    ..body("ada", PASSWORD)
                })
                .await,
                case,
            );
        }
        Case::ApiKeyGood => {
            let id = site.user("ada", "active").await;
            let (key, stored) = ApiKey::generate();
            site.rig
                .issue_key_with(id, stored.key_id.as_str(), &stored.digest, "active", None)
                .await;
            let principal = accepted(site.authenticate(&key.text()).await, case);
            assert_eq!(principal.user_id, id);
            assert_eq!(principal.via, spot_login::Via::ApiKey);
        }
        Case::ApiKeyRevoked => {
            let id = site.user("ada", "active").await;
            let (key, stored) = ApiKey::generate();
            site.rig
                .issue_key_with(id, stored.key_id.as_str(), &stored.digest, "revoked", None)
                .await;
            refused(
                site.authenticate(&key.text()).await,
                "auth.invalid_credentials",
                case,
            );
        }
        Case::ApiKeyExpired => {
            let id = site.user("ada", "active").await;
            let (key, stored) = ApiKey::generate();
            site.rig
                .issue_key_with(
                    id,
                    stored.key_id.as_str(),
                    &stored.digest,
                    "active",
                    Some(NOW - 1),
                )
                .await;
            refused(
                site.authenticate(&key.text()).await,
                "auth.invalid_credentials",
                case,
            );
        }
        Case::ApiKeyOwnerDisabled => {
            let id = site.user("ada", "disabled").await;
            let (key, stored) = ApiKey::generate();
            site.rig
                .issue_key_with(id, stored.key_id.as_str(), &stored.digest, "active", None)
                .await;
            refused(
                site.authenticate(&key.text()).await,
                "auth.invalid_credentials",
                case,
            );
        }
        Case::ApiKeyUnknown => {
            let (key, _) = ApiKey::generate();
            refused(
                site.authenticate(&key.text()).await,
                "auth.invalid_credentials",
                case,
            );
        }
        Case::EmptyIdentifier => {
            site.user("ada", "active").await;
            refused(
                site.login(body("   ", PASSWORD)).await,
                "auth.invalid_credentials",
                case,
            );
        }
        Case::UpperCaseIdentifier => {
            site.user("Ada.Lovelace", "active").await;
            accepted(site.login(body("ADA.LOVELACE", PASSWORD)).await, case);
        }
        Case::EmailIdentifier => {
            site.user("grace", "active").await;
            accepted(site.login(body("grace@example.test", PASSWORD)).await, case);
        }
        Case::SessionValid => {
            site.user("ada", "active").await;
            let session = accepted(site.login(body("ada", PASSWORD)).await, case);
            let principal = accepted(site.authenticate(session.token.as_str()).await, case);
            assert_eq!(principal.via, spot_login::Via::Password);
            assert!(principal.session_id.is_some());
        }
        Case::SessionRevoked => {
            site.user("ada", "active").await;
            let session = accepted(site.login(body("ada", PASSWORD)).await, case);
            let ctx = RequestCtx {
                credential: Some(Credential::new(session.token.as_str())),
                ..common::ctx(None)
            };
            accepted(AuthService::logout(site.service.as_ref(), &ctx).await, case);
            refused(
                site.authenticate(session.token.as_str()).await,
                "auth.session_expired",
                case,
            );
        }
        Case::SessionExpired => {
            site.user("ada", "active").await;
            let session = accepted(site.login(body("ada", PASSWORD)).await, case);
            site.rig.clock.advance(Duration::from_secs(30 * 60 + 1));
            refused(
                site.authenticate(session.token.as_str()).await,
                "auth.session_expired",
                case,
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_sign_in_table_on_the_fake_backend() {
    for case in ALL {
        run_case(st::fake_at(at(NOW)).await, case).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_sign_in_table_on_postgres() {
    for case in ALL {
        let Some(rig) = st::postgres_at(at(NOW)).await else {
            println!("SKIPPED: set SPOT_TEST_PG_URL");
            return;
        };
        run_case(rig, case).await;
    }
}
