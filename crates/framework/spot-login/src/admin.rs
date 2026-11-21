//! The administrative sign-in actions, as plain functions (the HTTP surface is a later task).
//!
//! These are the actions an administrator or `spot new-site` performs on a user's sign-in state:
//! unlock a locked account, revoke every session, reset the second factor.
//!
//! `spot new-site`'s one-time initial password is
//! [`StorageKeyStore::set_initial_password`](crate::stores::StorageKeyStore::set_initial_password):
//! it writes a sealed lookup secret and needs the capability, which only the store holds, so it
//! is not a `LoginService` method. [`reset_second_factor`] issues recovery codes, and the
//! storage-backed directory refuses to store a recovery digest (the platform app has no
//! secret-capable record for one); it therefore answers `login.store_failed` over storage.

use spot_auth::{StoredDigest, generate_recovery_codes};
use spot_http::api::ApiError;
use spot_sessions::UnlockedBy;
use spot_sessions::UserKey;
use spot_sessions::model::SessionEndReason;
use spot_sessions::throttle::AccountKey;
use spot_storage_contract::RecordId;

use crate::error::{LoginError, LoginErrorKind, invalid_credentials};
use crate::service::{LoginService, normalise_identifier};

impl LoginService {
    /// Clears an account's throttle and lockout, audited as an administrator unlock.
    ///
    /// # Errors
    ///
    /// The shared invalid-credentials answer when no such user exists.
    pub async fn unlock_account(&self, admin: RecordId, user: RecordId) -> Result<(), ApiError> {
        let record = self
            .users
            .find_by_id(user)
            .await
            .map_err(|error| error.to_api_error())?
            .ok_or_else(invalid_credentials)?;
        let account =
            AccountKey::new(normalise_identifier(&record.user_name)).map_err(|reason| {
                LoginError::new(LoginErrorKind::StoreFailed, reason).to_api_error()
            })?;
        let now = self.clock.now_utc().unix_timestamp();
        self.throttle
            .unlock(&account, UnlockedBy::Admin, "", now)
            .await
            .map_err(|error| LoginError::from(error).to_api_error())?;
        self.emit(
            "identity_account_unlocked",
            Some(user),
            serde_json::json!({
                "user": user.to_string(),
                "by": admin.to_string(),
            }),
        )
        .await
    }

    /// Revokes every session the user holds, including the one making the call. Returns how many
    /// live sessions were ended.
    ///
    /// # Errors
    ///
    /// A store failure, reported as `http.internal`.
    pub async fn revoke_all_sessions(&self, user: RecordId) -> Result<usize, ApiError> {
        let key = UserKey::new(user.to_string()).map_err(|reason| {
            LoginError::new(LoginErrorKind::StoreFailed, reason).to_api_error()
        })?;
        let report = self
            .sessions
            .revoke_all_for_user(&key, None, SessionEndReason::Other("admin".to_owned()))
            .await
            .map_err(|error| LoginError::from(error).to_api_error())?;
        self.authenticator.invalidate_all();
        Ok(report.ended)
    }

    /// Clears the user's second factor and issues a fresh set of recovery codes, returning their
    /// one-time display forms. The codes are shown once and only their digests are stored.
    ///
    /// # Errors
    ///
    /// The shared invalid-credentials answer when no such user exists.
    pub async fn reset_second_factor(&self, user: RecordId) -> Result<Vec<String>, ApiError> {
        let _ = self
            .users
            .find_by_id(user)
            .await
            .map_err(|error| error.to_api_error())?
            .ok_or_else(invalid_credentials)?;
        let codes = generate_recovery_codes(self.recovery_codes);
        let displays: Vec<String> = codes.iter().map(|code| code.display()).collect();
        let digests: Vec<StoredDigest> = codes.iter().map(|code| code.digest()).collect();
        self.users
            .set_second_factor(user, None, digests)
            .await
            .map_err(|error| error.to_api_error())?;
        self.emit(
            "identity_second_factor_removed",
            Some(user),
            serde_json::json!({
                "user": user.to_string(),
                "factor": "totp",
                "factor_id": user.to_string(),
                "by": "admin",
            }),
        )
        .await?;
        Ok(displays)
    }
}
