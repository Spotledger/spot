//! The throttle on the password-change route (WP-PRD-01.T14).
//!
//! `POST /auth/password` asks for the *current* password, so a stolen session (a token, an
//! unattended browser) could guess it at wire speed while the sign-in lockout, which keys on the
//! sign-in name, never sees the attempts. This module counts failed current-password checks per
//! **user id** in the same [`ThrottleStore`] the sign-in service uses, under keys no sign-in can
//! produce, with the sign-in policy.
//!
//! * [`PasswordThrottle::admit`] runs **before** the password is verified. While the user is
//!   locked (or backed off) it refuses with the sign-in's own answer whatever the password is,
//!   so a locked route is not an oracle and costs no hashing.
//! * [`PasswordThrottle::failed`] records one wrong current password. It fails closed: when the
//!   counter cannot be written the route refuses.
//! * [`PasswordThrottle::succeeded`] resets this user's password-change counter, and only that.
//!
//! The counter is independent of the sign-in counter both ways. Its key is `pwchange:` plus the
//! user's record id (never the sign-in name or the session token), so failed password changes
//! cannot lock the user out of signing in from another device, failed sign-ins cannot lock the
//! password route, and renaming the user does not reset the count. Its source dimension is also
//! per user, so one user's failures never back another user off.
//!
//! The counters are as durable as the sign-in counters (per process, bounded by
//! [`THROTTLE_KEY_CAP`](crate::audit::THROTTLE_KEY_CAP), a recorded security finding); because they go through the
//! store, the persistent store that closes a recorded security finding covers them.
use std::sync::Arc;

use spot_core::Clock;
use spot_http::api::service::ApiError;
use spot_login::{LoginError, LoginErrorKind};
use spot_sessions::model::SaltKey;
use spot_sessions::throttle::{
    AccountKey, LoginThrottle, ThrottleConfig, ThrottleDecision, ThrottleError, ThrottleStore,
};

/// The prefix of every password-change throttle key. A sign-in name cannot be mistaken for it
/// because the key after the prefix is a record id, and the audit decorator recognises it.
pub const KEY_PREFIX: &str = "pwchange:";

/// The per-user throttle of the password-change route.
pub struct PasswordThrottle<S: ThrottleStore> {
    throttle: LoginThrottle<S>,
    clock: Arc<dyn Clock>,
    salt: Arc<SaltKey>,
}

impl<S: ThrottleStore> PasswordThrottle<S> {
    /// A throttle over `store` with the sign-in `config`.
    ///
    /// # Errors
    ///
    /// [`ThrottleError`] when `config` is invalid.
    pub fn new(
        store: S,
        config: ThrottleConfig,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, ThrottleError> {
        let salt = Arc::new(SaltKey::generate());
        Ok(Self {
            throttle: LoginThrottle::new(store, config, salt.clone())?,
            clock,
            salt,
        })
    }

    /// The store the counters live in (tests inspect it).
    pub fn store(&self) -> &S {
        &self.throttle.store
    }

    /// The store key of `user`'s counter.
    #[must_use]
    pub fn key_of(user: &str) -> String {
        format!("{KEY_PREFIX}{user}")
    }

    fn account(user: &str) -> Result<AccountKey, ThrottleError> {
        AccountKey::new(Self::key_of(user)).map_err(ThrottleError::InvalidAccount)
    }

    fn source(user: &str) -> String {
        format!("pwchange-source/{user}")
    }

    fn now(&self) -> i64 {
        self.clock.now_utc().unix_timestamp()
    }

    fn store_failed(error: ThrottleError, request_id: &str) -> ApiError {
        LoginError::from(error).to_api_error_traced(request_id)
    }

    /// Whether `user` may try a current password now. Call it before verifying anything.
    ///
    /// # Errors
    ///
    /// The sign-in's lockout (`auth.locked`) or back-off (`auth.rate_limited`) answer with
    /// `retry_after`, or a server error when the counter cannot be read (fail closed).
    pub async fn admit(&self, user: &str, request_id: &str) -> Result<(), ApiError> {
        let account = Self::account(user).map_err(|error| Self::store_failed(error, request_id))?;
        let now = self.now();
        match self
            .throttle
            .check(&account, &Self::source(user), now)
            .await
            .map_err(|error| Self::store_failed(error, request_id))?
        {
            ThrottleDecision::Allow => Ok(()),
            ThrottleDecision::Delay { delay, .. } => Err(LoginError::new(
                LoginErrorKind::Throttled,
                format!("retry_after={}", delay.as_secs()),
            )
            .to_api_error_traced(request_id)),
            ThrottleDecision::Locked { until } => {
                let wait = u64::try_from((until - now).max(0)).unwrap_or(0);
                Err(
                    LoginError::new(LoginErrorKind::AccountLocked, format!("retry_after={wait}"))
                        .to_api_error_traced(request_id),
                )
            }
        }
    }

    /// Counts one wrong current password for `user`.
    ///
    /// # Errors
    ///
    /// A server error when the counter cannot be written (the route refuses).
    pub async fn failed(&self, user: &str, request_id: &str) -> Result<(), ApiError> {
        let account = Self::account(user).map_err(|error| Self::store_failed(error, request_id))?;
        self.throttle
            .record_failure(&account, &Self::source(user), self.now())
            .await
            .map_err(|error| Self::store_failed(error, request_id))?;
        // A counter the bounded store evicted to make room for its own source entry would let
        // the guesses go uncounted: read it back, and refuse when it is not there.
        let kept = self
            .throttle
            .store
            .get(true, account.as_str().as_bytes())
            .await
            .map_err(|error| Self::store_failed(error, request_id))?;
        if kept.failures == 0 {
            return Err(Self::store_failed(
                ThrottleError::StoreFailed("the password-change counter was not kept".into()),
                request_id,
            ));
        }
        Ok(())
    }

    /// Resets `user`'s password-change counter (and only that) after a correct current
    /// password. A store that cannot clear it leaves the counter as it was, which only makes
    /// the route stricter.
    pub async fn succeeded(&self, user: &str) {
        let Ok(account) = Self::account(user) else {
            return;
        };
        let source = Self::source(user);
        let _ = self
            .throttle
            .record_success(&account, &source, self.now())
            .await;
        let digest = self.salt.digest_source(&source);
        let _ = self.throttle.store.clear(false, digest.as_bytes()).await;
    }
}
