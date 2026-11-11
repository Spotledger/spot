//! [`StorageUserDirectory`]: users as `user` records, credentials as sealed `secret` fields.
//!
//! # Where a credential lives
//!
//! `user.password_hash` and `user.totp_secret` are `secret` fields: they are read and written
//! only through the capability ([`Engine::open_secret`](spot_document::engine::Engine::open_secret),
//! [`Engine::set_secret`](spot_document::engine::Engine::set_secret)), so no column, document,
//! event or audit entry of the platform app ever holds them. Everything else — the sign-in name,
//! the e-mail, the state, the counters — is an ordinary column.
//!
//! # The TOTP replay counter
//!
//! The platform app has no `totp_last_step` column (`apps/platform/doctypes/user.toml` is not an
//! owned path of this task), and a second factor without replay protection is not a second
//! factor. The value sealed in `user.totp_secret` is therefore `<base32>/<last accepted step>`:
//! the secret and its replay counter travel as one sealed value, are opened together and are
//! updated together by [`UserDirectory::record_totp_step`], which is a compare-and-set on the
//! record version (read before the value, checked by the write's own transaction), so of any
//! number of parallel sign-ins with one code exactly one is accepted. A value that does not parse (a
//! secret written by an older version, say) is reported as a store failure, never as "no second
//! factor": the sign-in fails closed.
//!
//! # What this store refuses, on purpose
//!
//! - **Recovery-code digests.** They live in `user_recovery_digest`, a *child* DocType, and the
//!   secret path refuses a child row (`secretfield.unsupported_doctype`): there is nowhere to put
//!   a digest that is not an ordinary column. Storing one in a plain column would put a
//!   credential in a readable place, so [`UserDirectory::set_second_factor`] and
//!   [`UserDirectory::consume_recovery_digest`] refuse with `login.store_failed`. Recovery codes
//!   are therefore unavailable until the platform app gives them a secret-capable home; the
//!   report records it.
//! - **Clearing a second factor.** The secret path has no "unset a secret" call yet, so
//!   `set_second_factor(id, None, ..)` refuses rather than silently leaving the factor in place.

use std::sync::Arc;

use spot_auth::{PasswordHash, StoredDigest, TotpSecret};
use spot_platform_app::doctype::USER;
use spot_secrets::SecretString;
use spot_storage_contract::{
    DocumentData, DocumentStore, Expr, Fields, Name, Query, QueryExecutor, QueryPath, RecordId,
    RowLock, Selection, StorageBackend, TextMatch, TextMode, Transaction,
};

use crate::error::{LoginError, LoginErrorKind};
use crate::model::{UserRecord, UserState};
use crate::ports::{BoxFuture, UserDirectory};

use super::{LoginStores, failed_at, name, record, store_failed, text};

/// The separator between the TOTP secret and its last accepted step in the sealed value.
pub const TOTP_STEP_SEPARATOR: char = '/';

/// How many times a TOTP step write retries after losing a version race.
const TOTP_CAS_ATTEMPTS: u32 = 5;

/// Users over the platform app's `user` records.
pub struct StorageUserDirectory<B: StorageBackend> {
    stores: Arc<LoginStores<B>>,
}

impl<B: StorageBackend> StorageUserDirectory<B> {
    /// A directory over one site's engine and capability.
    #[must_use]
    pub fn new(stores: Arc<LoginStores<B>>) -> Self {
        Self { stores }
    }

    fn entity() -> Result<Name, LoginError> {
        name(USER)
    }

    fn label(suffix: &str) -> String {
        format!("spot_login_{suffix}")
    }

    /// The text of one stored field.
    fn text_at(fields: &Fields, field: &str) -> Option<String> {
        fields
            .iter()
            .find(|(name, _)| name.as_str() == field)
            .and_then(|(_, value)| text(value))
    }

    /// The id of the user whose sign-in name or e-mail is `identifier`, case-insensitively.
    async fn id_for(&self, identifier: &str) -> Result<Option<RecordId>, LoginError> {
        find_user_id(&self.stores, "find_user", identifier).await
    }

    /// The full credential record of a user, or `None` when no such user exists.
    async fn record_of(&self, id: RecordId) -> Result<Option<UserRecord>, LoginError> {
        let data = self.load(id).await?;
        let Some(data) = data else {
            return Ok(None);
        };
        let user_name = Self::text_at(&data.fields, "user_name")
            .ok_or_else(|| store_failed("user: no user_name"))?;
        let state = Self::text_at(&data.fields, "state")
            .and_then(|value| UserState::parse(&value))
            .ok_or_else(|| store_failed("user: unreadable state"))?;
        let password_hash = self.password_hash(id).await?;
        let (totp_secret, totp_last_step) = self.second_factor(id).await?;
        Ok(Some(UserRecord {
            id,
            user_name,
            email: Self::text_at(&data.fields, "email"),
            full_name: Self::text_at(&data.fields, "full_name").unwrap_or_default(),
            state,
            password_hash,
            totp_secret,
            // Recovery digests have no secret-capable home yet (see the module docs).
            recovery_digests: Vec::new(),
            totp_last_step,
        }))
    }

    async fn load(&self, id: RecordId) -> Result<Option<DocumentData>, LoginError> {
        let mut tx = self.stores.begin(&Self::label("load_user"), false).await?;
        let entity = Self::entity()?;
        let loaded = tx.load(&entity, id, RowLock::None).await;
        let _ = tx.rollback().await;
        Ok(loaded
            .map_err(|error| failed_at("load user", &error))?
            .map(|document| document.data))
    }

    /// The stored Argon2id verifier, opened through the capability.
    async fn password_hash(&self, id: RecordId) -> Result<Option<PasswordHash>, LoginError> {
        let opened = self
            .stores
            .engine()
            .open_secret(self.stores.access(), USER, id, "password_hash")
            .await
            .map_err(|error| failed_at("open password_hash", &error))?;
        match opened {
            None => Ok(None),
            Some(secret) => PasswordHash::parse(secret.expose())
                .map(Some)
                .map_err(|_| store_failed("password_hash: not a verifier")),
        }
    }

    /// The sealed TOTP secret and its last accepted step.
    async fn second_factor(
        &self,
        id: RecordId,
    ) -> Result<(Option<TotpSecret>, Option<u64>), LoginError> {
        let opened = self
            .stores
            .engine()
            .open_secret(self.stores.access(), USER, id, "totp_secret")
            .await
            .map_err(|error| failed_at("open totp_secret", &error))?;
        match opened {
            None => Ok((None, None)),
            Some(secret) => {
                decode_second_factor(secret.expose()).map(|(secret, step)| (Some(secret), step))
            }
        }
    }

    /// The current version of a user record (`None` when there is no such record).
    async fn load_version(
        &self,
        id: RecordId,
    ) -> Result<Option<spot_storage_contract::Version>, LoginError> {
        let mut tx = self
            .stores
            .begin(&Self::label("secret_version"), false)
            .await?;
        let entity = Self::entity()?;
        let loaded = tx.load(&entity, id, RowLock::None).await;
        let _ = tx.rollback().await;
        Ok(loaded
            .map_err(|error| failed_at("load user", &error))?
            .map(|document| document.version))
    }

    /// Writes the sealed value of `field` for `id`, in one short transaction.
    async fn write_secret(
        &self,
        id: RecordId,
        field: &str,
        value: SecretString,
    ) -> Result<(), LoginError> {
        let version = self
            .load_version(id)
            .await?
            .ok_or_else(|| store_failed("user: no such record"))?;
        self.stores
            .engine()
            .set_secret(self.stores.access(), USER, id, field, value, version)
            .await
            .map(|_| ())
            .map_err(|error| failed_at(field, &error))
    }
}

impl<B: StorageBackend> UserDirectory for StorageUserDirectory<B> {
    fn find_by_identifier<'a>(
        &'a self,
        identifier: &'a str,
    ) -> BoxFuture<'a, Result<Option<UserRecord>, LoginError>> {
        Box::pin(async move {
            let Some(id) = self.id_for(identifier).await? else {
                return Ok(None);
            };
            self.record_of(id).await
        })
    }

    fn find_by_id<'a>(
        &'a self,
        id: RecordId,
    ) -> BoxFuture<'a, Result<Option<UserRecord>, LoginError>> {
        Box::pin(async move { self.record_of(id).await })
    }

    fn set_password_hash<'a>(
        &'a self,
        id: RecordId,
        hash: PasswordHash,
    ) -> BoxFuture<'a, Result<(), LoginError>> {
        Box::pin(async move {
            self.write_secret(
                id,
                "password_hash",
                SecretString::new(hash.phc_string().to_owned()),
            )
            .await
        })
    }

    fn record_totp_step<'a>(
        &'a self,
        id: RecordId,
        step: u64,
    ) -> BoxFuture<'a, Result<(), LoginError>> {
        Box::pin(async move {
            // Compare-and-set: the record version is read BEFORE the sealed value, and the
            // write carries it as the expected version, which the storage layer checks inside
            // the write's own transaction under the row's lock. A concurrent sign-in that
            // advanced the step (or changed the row at all) makes the write fail stale; the
            // loser then re-reads and, if the stored step has reached its own, is a replay.
            for _ in 0..TOTP_CAS_ATTEMPTS {
                let version = self
                    .load_version(id)
                    .await?
                    .ok_or_else(|| store_failed("record_totp_step: no such user"))?;
                let (secret, last) = self.second_factor(id).await?;
                let secret = secret.ok_or_else(|| {
                    store_failed("record_totp_step: the user has no second factor")
                })?;
                if last.is_some_and(|last| last >= step) {
                    return Err(LoginError::new(
                        LoginErrorKind::TotpReplayed,
                        "the code's step was already accepted",
                    ));
                }
                match self
                    .stores
                    .engine()
                    .set_secret(
                        self.stores.access(),
                        USER,
                        id,
                        "totp_secret",
                        SecretString::new(encode_second_factor(&secret, step)),
                        version,
                    )
                    .await
                {
                    Ok(_) => return Ok(()),
                    Err(error) if error.code == "document.stale_version" => {}
                    Err(error) => return Err(failed_at("totp_secret", &error)),
                }
            }
            Err(store_failed("record_totp_step: too much contention"))
        })
    }

    fn consume_recovery_digest<'a>(
        &'a self,
        _id: RecordId,
        _index: usize,
    ) -> BoxFuture<'a, Result<(), LoginError>> {
        Box::pin(async {
            // No secret-capable home for a recovery digest yet: refuse instead of writing a
            // credential into a plain column.
            Err(recovery_unavailable())
        })
    }

    fn set_second_factor<'a>(
        &'a self,
        id: RecordId,
        totp: Option<TotpSecret>,
        recovery: Vec<StoredDigest>,
    ) -> BoxFuture<'a, Result<(), LoginError>> {
        Box::pin(async move {
            if !recovery.is_empty() {
                return Err(recovery_unavailable());
            }
            let Some(totp) = totp else {
                return Err(store_failed(
                    "set_second_factor: the secret path cannot unset a secret yet",
                ));
            };
            self.write_secret(
                id,
                "totp_secret",
                SecretString::new(encode_second_factor(&totp, 0)),
            )
            .await
        })
    }
}

/// The id of the user whose sign-in name or e-mail is `identifier`, case-insensitively. The
/// one lookup both the directory and the throttle resolve an account through.
pub(crate) async fn find_user_id<B: StorageBackend>(
    stores: &LoginStores<B>,
    label: &str,
    identifier: &str,
) -> Result<Option<RecordId>, LoginError> {
    let entity = name(USER)?;
    let alias = name("u")?;
    let mut query = Query::from(&entity, &alias, QueryPath::Report);
    query.select = vec![Selection {
        expr: Expr::field(&alias, &name("id")?),
        alias: name("v")?,
    }];
    // Case-insensitive equality on both sign-in columns: `TextMatch` with `case_insensitive`
    // is the IR's one way to ask for it, and a null e-mail simply does not match.
    let matches = |field: &str| -> Result<Expr, LoginError> {
        Ok(Expr::Text(
            Box::new(Expr::field(&alias, &name(field)?)),
            TextMatch {
                mode: TextMode::Equals,
                pattern: identifier.to_owned(),
                case_insensitive: true,
            },
        ))
    };
    query.filter = Some(Expr::Or(vec![matches("user_name")?, matches("email")?]));
    query.limit = Some(1);
    let mut tx = stores.begin(&format!("spot_login_{label}"), false).await?;
    let rows = tx.query(&query).await;
    let _ = tx.rollback().await;
    Ok(rows
        .map_err(|error| failed_at("find user", &error))?
        .rows
        .into_iter()
        .flatten()
        .find_map(|value| record(&value)))
}

/// The one refusal for a recovery-code operation that has no storage.
fn recovery_unavailable() -> LoginError {
    LoginError::new(
        LoginErrorKind::StoreFailed,
        "recovery digests have no secret-capable record: `user_recovery_digest` is a child \
         DocType and the secret path refuses child rows (secretfield.unsupported_doctype)",
    )
}

/// The sealed text of a TOTP secret and its last accepted step.
fn encode_second_factor(secret: &TotpSecret, step: u64) -> String {
    format!("{}{TOTP_STEP_SEPARATOR}{step}", secret.to_base32())
}

/// A sealed TOTP value back into its secret and step. A value without the step part is
/// refused: the replay counter is not optional.
fn decode_second_factor(value: &str) -> Result<(TotpSecret, Option<u64>), LoginError> {
    let (base32, step) = value
        .split_once(TOTP_STEP_SEPARATOR)
        .ok_or_else(|| store_failed("totp_secret: unreadable (no replay step)"))?;
    let secret = TotpSecret::from_base32(base32)
        .map_err(|_| store_failed("totp_secret: unreadable secret"))?;
    let step = step
        .parse::<u64>()
        .map_err(|_| store_failed("totp_secret: unreadable replay step"))?;
    Ok((secret, Some(step)))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{decode_second_factor, encode_second_factor};
    use spot_auth::TotpSecret;

    #[test]
    fn the_replay_step_travels_with_the_secret() {
        let secret = TotpSecret::generate();
        let encoded = encode_second_factor(&secret, 7);
        let (decoded, step) = decode_second_factor(&encoded).expect("round trip");
        assert_eq!(decoded.to_base32(), secret.to_base32());
        assert_eq!(step, Some(7));
    }

    #[test]
    fn a_value_without_a_step_is_refused() {
        let secret = TotpSecret::generate();
        assert!(decode_second_factor(&secret.to_base32()).is_err());
        assert!(decode_second_factor("not base32/7").is_err());
        assert!(decode_second_factor("AAAA/not-a-step").is_err());
    }
}
