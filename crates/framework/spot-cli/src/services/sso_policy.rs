//! The tenant sign-in policy, stored (WP-IAM-05.T16): [`PgPolicyStore`] is the [`PolicySource`]
//! the password door reads on every attempt (WP-IAM-05.T12) and the record the administration API
//! changes (`sso_admin.rs`).
//!
//! One row per tenant in `spot_sso_policy` (`tenant`, `version`, `policy` as JSON). A tenant with
//! **no row** has never configured single sign-on: its policy is `local`, version 0. A row that
//! cannot be read or parsed, or a database that does not answer, is **not** `local`: it is
//! [`PolicyUnavailable`], and the password door refuses everyone (fail closed, as T12 requires).
//! The policy is read fresh for every attempt (no cache), so a change is effective on the next
//! request. Changes are compare-and-set on the version: two administrators changing the policy at
//! once cannot both win, and the loser is told ([`PolicyStoreError::Stale`]). The SQL lives in
//! `spot-sso-pg` since WP-IAM-05.T18; this file adapts it to the host.

use spot_http::api::service::BoxFuture;
use spot_login::{PolicyUnavailable, SigninMode, SigninPolicy};
use spot_sso::{SignInMode, SignInPolicy};
use spot_sso_pg::Pool;
pub use spot_sso_pg::{POLICY_DDL, PolicyStoreError};

use crate::services::signin_policy::PolicySource;
use crate::{Error, Result};

/// The mode as the password door knows it.
#[must_use]
pub fn login_mode(mode: SignInMode) -> SigninMode {
    match mode {
        SignInMode::Local => SigninMode::Local,
        SignInMode::SsoOptional => SigninMode::SsoOptional,
        SignInMode::SsoRequired => SigninMode::SsoRequired,
    }
}

/// The password door's view of a stored policy.
#[must_use]
pub fn door_policy(policy: &SignInPolicy) -> SigninPolicy {
    SigninPolicy {
        mode: login_mode(policy.mode),
        break_glass: policy.break_glass.clone(),
    }
}

/// The policy store of one tenant. The SQL is `spot_sso_pg::PgPolicyStore` (WP-IAM-05.T18); this
/// wraps it for the host's error type and the password door's port.
#[derive(Clone)]
pub struct PgPolicyStore {
    rows: spot_sso_pg::PgPolicyStore,
}

impl std::fmt::Debug for PgPolicyStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgPolicyStore")
            .field("tenant", &self.rows.tenant())
            .finish_non_exhaustive()
    }
}

impl PgPolicyStore {
    /// A store on `pool` for `tenant`; `None` when the tenant is empty or longer than 128 bytes.
    #[must_use]
    pub fn new(pool: Pool, tenant: &str) -> Option<Self> {
        spot_sso_pg::PgPolicyStore::new(pool, tenant).map(|rows| Self { rows })
    }

    /// The tenant this store serves.
    #[must_use]
    pub fn tenant(&self) -> &str {
        self.rows.tenant()
    }

    /// Creates the table if missing. Idempotent.
    ///
    /// # Errors
    ///
    /// `sso_providers_host.migration_failed`.
    pub async fn prepare(&self) -> Result<()> {
        self.rows.prepare().await.map_err(|_| {
            Error(
                "sso_providers_host.migration_failed: the sign-in tables could not be created"
                    .into(),
            )
        })
    }

    /// The stored policy and its version; `(local, 0)` for a tenant with no row.
    ///
    /// # Errors
    ///
    /// [`PolicyStoreError::Unavailable`] when the database does not answer or the row does not
    /// parse (never `local` by mistake).
    pub async fn read(&self) -> std::result::Result<(SignInPolicy, i64), PolicyStoreError> {
        self.rows.read().await
    }

    /// Replaces the policy if the stored version is `expected` (0: no row yet). Returns the new
    /// version.
    ///
    /// # Errors
    ///
    /// [`PolicyStoreError::Stale`], [`PolicyStoreError::Unavailable`].
    pub async fn write(
        &self,
        policy: &SignInPolicy,
        expected: i64,
    ) -> std::result::Result<i64, PolicyStoreError> {
        self.rows.write(policy, expected).await
    }
}

impl PolicySource for PgPolicyStore {
    fn load<'a>(
        &'a self,
        tenant: &'a str,
    ) -> BoxFuture<'a, std::result::Result<SigninPolicy, PolicyUnavailable>> {
        Box::pin(async move {
            // A store answers for its own tenant only.
            if tenant != self.tenant() {
                return Err(PolicyUnavailable);
            }
            self.read()
                .await
                .map(|(policy, _)| door_policy(&policy))
                .map_err(|_| PolicyUnavailable)
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use spot_storage_contract::RecordId;

    use super::*;

    #[test]
    fn the_door_sees_the_mode_and_the_break_glass_administrators() {
        let admin = RecordId::new_v7();
        let policy = SignInPolicy {
            mode: SignInMode::SsoRequired,
            break_glass: vec![admin],
            ..SignInPolicy::local()
        };
        let door = door_policy(&policy);
        assert_eq!(door.mode, SigninMode::SsoRequired);
        assert_eq!(door.break_glass, vec![admin]);
        assert_eq!(door_policy(&SignInPolicy::local()), SigninPolicy::local());
    }

    #[test]
    fn a_stored_policy_round_trips() {
        let policy = SignInPolicy {
            mode: SignInMode::SsoOptional,
            ..SignInPolicy::local()
        };
        let text = serde_json::to_string(&policy).unwrap();
        assert_eq!(serde_json::from_str::<SignInPolicy>(&text).unwrap(), policy);
    }
}
