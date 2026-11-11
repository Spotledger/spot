//! [`StorageKeyStore`]: API keys as `api_key` records, digests as lookup secrets.
//!
//! A key is looked up by its **public** id (`api_key.key_id`, an ordinary unique text column) and
//! its secret part is verified by comparing the presented digest against the sealed
//! `api_key.digest` in constant time (`TokenDigest::matches`). The digest is a lookup secret, so
//! it is blind-indexed as well: a caller that must find a *record* by a secret it holds (the
//! one-time initial secret, see [`crate::admin`]) uses
//! [`Engine::find_by_secret`](spot_document::engine::Engine::find_by_secret) and never a column.
//!
//! Nothing here returns the digest to anyone but this crate: [`KeyRecord`] is built for the
//! sign-in service, and its `Debug` shows `Secret(***)`.

use std::sync::Arc;

use spot_auth::TokenDigest;
use spot_platform_app::doctype::API_KEY;

/// The `api_key.key_id` prefix of a one-time initial secret.
pub const INITIAL_KEY_PREFIX: &str = "initial-";
use spot_storage_contract::{
    DocumentData, DocumentStore, Expr, Name, Query, QueryExecutor, QueryPath, RecordId, RowLock,
    Selection, StorageBackend, Transaction, Value,
};

use crate::error::{LoginError, LoginErrorKind};
use crate::model::{KeyRecord, KeyState};
use crate::ports::{BoxFuture, KeyStore};

use super::{LoginStores, failed_at, name, record, seconds, text, unhex};

/// API keys over the platform app's `api_key` records.
pub struct StorageKeyStore<B: StorageBackend> {
    stores: Arc<LoginStores<B>>,
}

impl<B: StorageBackend> StorageKeyStore<B> {
    /// A store over one site's engine and capability.
    #[must_use]
    pub fn new(stores: Arc<LoginStores<B>>) -> Self {
        Self { stores }
    }

    fn entity() -> Result<Name, LoginError> {
        name(API_KEY)
    }

    fn column(text: &str) -> Result<Name, LoginError> {
        name(text)
    }

    fn label(suffix: &str) -> String {
        format!("spot_login_{suffix}")
    }

    /// The digest of a key record, opened through the capability.
    async fn digest_of(&self, id: RecordId) -> Result<Option<TokenDigest>, LoginError> {
        let opened = self
            .stores
            .engine()
            .open_secret(self.stores.access(), API_KEY, id, "digest")
            .await
            .map_err(|error| failed_at("open api_key.digest", &error))?;
        let Some(secret) = opened else {
            return Ok(None);
        };
        let bytes = unhex(secret.expose())
            .ok_or_else(|| super::store_failed("api_key.digest: not a digest"))?;
        TokenDigest::from_bytes(&bytes)
            .map(Some)
            .map_err(|_| super::store_failed("api_key.digest: not a digest"))
    }

    /// The record id of the `api_key` whose public id is `key_id`.
    async fn id_for_key_id(&self, key_id: &str) -> Result<Option<RecordId>, LoginError> {
        let entity = Self::entity()?;
        let alias = Self::column("k")?;
        let mut query = Query::from(&entity, &alias, QueryPath::Report);
        query.select = vec![Selection {
            expr: Expr::field(&alias, &Self::column("id")?),
            alias: Self::column("v")?,
        }];
        query.filter = Some(
            Expr::field(&alias, &Self::column("key_id")?)
                .eq(Expr::value(Value::Text(key_id.to_owned()))),
        );
        query.limit = Some(1);
        let mut tx = self.stores.begin(&Self::label("find_key"), false).await?;
        let rows = tx.query(&query).await;
        let _ = tx.rollback().await;
        Ok(rows
            .map_err(|error| failed_at("find api_key", &error))?
            .rows
            .into_iter()
            .flatten()
            .find_map(|value| record(&value)))
    }

    /// The full record of one `api_key` row.
    async fn record_of(&self, id: RecordId) -> Result<Option<KeyRecord>, LoginError> {
        let Some(digest) = self.digest_of(id).await? else {
            return Ok(None);
        };
        let mut tx = self.stores.begin(&Self::label("load_key"), false).await?;
        let entity = Self::entity()?;
        let loaded = tx.load(&entity, id, RowLock::None).await;
        let _ = tx.rollback().await;
        let Some(loaded) = loaded.map_err(|error| failed_at("load api_key", &error))? else {
            return Ok(None);
        };
        Ok(Some(key_of(id, &loaded.data, digest)?))
    }

    /// The record found by a secret this crate holds — the one-time initial secret's only
    /// lookup path. One indexed equality on the blind index; `None` when no record holds it.
    pub async fn find_by_digest(
        &self,
        digest: &TokenDigest,
    ) -> Result<Option<KeyRecord>, LoginError> {
        let found = self
            .stores
            .engine()
            .find_by_secret(
                self.stores.access(),
                API_KEY,
                "digest",
                super::hex(digest.as_bytes()).as_bytes(),
            )
            .await
            .map_err(|error| failed_at("find api_key by digest", &error))?;
        let Some(id) = found else {
            return Ok(None);
        };
        self.record_of(id).await
    }

    /// Whether a key record can still be used at `now`: active, and not past its expiry.
    #[must_use]
    pub fn usable_at(key: &KeyRecord, now: i64) -> bool {
        key.state == KeyState::Active
            && !key
                .expires_at
                .is_some_and(|expires| now >= expires.unix_timestamp())
    }

    /// Marks a key revoked, in one short transaction. Used to spend a one-time initial secret.
    pub async fn revoke(&self, key_id: &str) -> Result<bool, LoginError> {
        let Some(id) = self.id_for_key_id(key_id).await? else {
            return Ok(false);
        };
        let mut tx = self.stores.begin(&Self::label("revoke_key"), true).await?;
        let entity = Self::entity()?;
        let loaded = tx
            .load(
                &entity,
                id,
                RowLock::Update(spot_storage_contract::Wait::Block),
            )
            .await;
        let Some(mut loaded) = loaded.map_err(|error| failed_at("load api_key", &error))? else {
            let _ = tx.rollback().await;
            return Ok(false);
        };
        loaded
            .data
            .fields
            .insert(Self::column("state")?, Value::Text("revoked".to_owned()));
        let updated = tx.update(&loaded.data, loaded.version).await;
        match updated {
            Ok(_) => {
                tx.commit()
                    .await
                    .map_err(|error| failed_at("commit revoke api_key", &error))?;
                Ok(true)
            }
            Err(error) => {
                let _ = tx.rollback().await;
                Err(failed_at("revoke api_key", &error))
            }
        }
    }
}

impl<B: StorageBackend> StorageKeyStore<B> {
    /// Issues the one-time initial password of `user`: a **single-use, expiring lookup secret**
    /// that is never stored in the clear.
    ///
    /// The record is an `api_key` row (owner, state and expiry are exactly what a one-time
    /// bearer credential needs): its `key_id` is `initial-<record id>`, its `digest` is the
    /// sealed blind-indexable value of the presented secret, and `expires_at` is `now + ttl`.
    /// Nothing readable holds the secret and nothing holds its digest: `find_by_secret` is the
    /// only way back from a presented secret to the row.
    ///
    /// # Hand-over (what `spot new-site` does)
    ///
    /// 1. The CLI generates an [`OpaqueToken`], gives it to this call, and prints the token text
    ///    **once** to the operator's terminal (or the systemd journal of the one-shot command);
    ///    it is never written to `site.toml`, a log line, a job payload or an event.
    /// 2. The operator signs in with that text as the password; `spot-login` calls
    ///    [`StorageKeyStore::consume_initial_secret`], which spends the row in the same
    ///    transaction that checks it.
    /// 3. From then on the text is dead: the row is revoked (a second use is
    ///    `login.initial_secret_used`) and, once `ttl` has passed, an unused one is
    ///    `login.initial_secret_invalid`.
    ///
    /// # Errors
    ///
    /// `login.store_failed` when the record cannot be written, or when `ttl` is not positive.
    pub async fn set_initial_password(
        &self,
        user: RecordId,
        secret: &spot_auth::OpaqueToken,
        ttl: std::time::Duration,
        now: i64,
    ) -> Result<String, LoginError> {
        let ttl = i64::try_from(ttl.as_secs())
            .ok()
            .filter(|seconds| *seconds > 0)
            .ok_or_else(|| super::store_failed("initial secret: the expiry must be positive"))?;
        let id = RecordId::new_v7();
        let key_id = format!("{INITIAL_KEY_PREFIX}{id}");
        let mut doc = spot_app_api::Document::new(Self::entity()?, id);
        doc.set(Self::column("key_id")?, Value::Text(key_id.clone()));
        doc.set(Self::column("owner")?, Value::Id(user));
        doc.set(Self::column("state")?, Value::Text("active".to_owned()));
        doc.set(
            Self::column("label")?,
            Value::Text("one-time initial secret".to_owned()),
        );
        doc.set(
            Self::column("expires_at")?,
            Value::DateTime(super::instant(now.saturating_add(ttl))?),
        );
        self.stores
            .engine()
            .create_with_secrets(
                self.stores.access(),
                doc,
                vec![(
                    "digest".to_owned(),
                    spot_secrets::SecretString::new(super::hex(secret.digest().as_bytes())),
                )],
            )
            .await
            .map_err(|error| super::failed_at("issue initial secret", &error))?;
        Ok(key_id)
    }

    /// Spends `user`'s one-time initial secret, if `presented` is it: one transaction that
    /// finds the row by blind index, checks owner, kind, state and expiry under a write lock,
    /// and revokes it. A second call with the same text finds the row revoked.
    ///
    /// # Errors
    ///
    /// `login.initial_secret_invalid` for a text that is unknown, expired or another user's;
    /// `login.initial_secret_used` for one already spent. The two answers are the only
    /// difference the caller ever sees, and neither names the row.
    pub async fn consume_initial_secret(
        &self,
        user: RecordId,
        presented: &spot_auth::OpaqueToken,
        now: i64,
    ) -> Result<(), LoginError> {
        let invalid = || {
            LoginError::new(
                LoginErrorKind::InitialSecretInvalid,
                "the one-time secret is not valid",
            )
        };
        let Some(id) = self
            .stores
            .engine()
            .find_by_secret(
                self.stores.access(),
                API_KEY,
                "digest",
                super::hex(presented.digest().as_bytes()).as_bytes(),
            )
            .await
            .map_err(|error| super::failed_at("find initial secret", &error))?
        else {
            return Err(invalid());
        };
        let mut tx = self
            .stores
            .begin(&Self::label("consume_initial"), true)
            .await?;
        let entity = Self::entity()?;
        let loaded = tx
            .load(
                &entity,
                id,
                RowLock::Update(spot_storage_contract::Wait::Block),
            )
            .await;
        let Some(mut loaded) =
            loaded.map_err(|error| super::failed_at("load initial secret", &error))?
        else {
            let _ = tx.rollback().await;
            return Err(invalid());
        };
        let text_of = |fields: &spot_storage_contract::Fields, field: &str| {
            fields
                .iter()
                .find(|(name, _)| name.as_str() == field)
                .and_then(|(_, value)| text(value))
        };
        let fields = &loaded.data.fields;
        let key_id = text_of(fields, "key_id").unwrap_or_default();
        let owner = fields
            .iter()
            .find(|(name, _)| name.as_str() == "owner")
            .and_then(|(_, value)| record(value));
        let state = text_of(fields, "state").unwrap_or_default();
        // A missing or non-date expiry is a malformed row: it fails CLOSED (never "no expiry").
        let expires = fields
            .iter()
            .find(|(name, _)| name.as_str() == "expires_at")
            .and_then(|(_, value)| seconds(value));
        if !key_id.starts_with(INITIAL_KEY_PREFIX) || owner != Some(user) {
            let _ = tx.rollback().await;
            return Err(invalid());
        }
        if state != "active" {
            let _ = tx.rollback().await;
            return Err(LoginError::new(
                LoginErrorKind::InitialSecretUsed,
                "the one-time secret was already used",
            ));
        }
        if expires.is_none_or(|expires| now >= expires) {
            let _ = tx.rollback().await;
            return Err(invalid());
        }
        loaded
            .data
            .fields
            .insert(Self::column("state")?, Value::Text("revoked".to_owned()));
        let updated = tx.update(&loaded.data, loaded.version).await;
        match updated {
            Ok(_) => tx
                .commit()
                .await
                .map_err(|error| super::failed_at("commit initial secret", &error)),
            Err(error) => {
                let _ = tx.rollback().await;
                Err(super::failed_at("spend initial secret", &error))
            }
        }
    }
}

impl<B: StorageBackend> KeyStore for StorageKeyStore<B> {
    fn consume_initial_secret<'a>(
        &'a self,
        user: RecordId,
        presented: &'a spot_auth::OpaqueToken,
        now: i64,
    ) -> BoxFuture<'a, Result<(), LoginError>> {
        Box::pin(StorageKeyStore::consume_initial_secret(
            self, user, presented, now,
        ))
    }

    fn find_by_key_id<'a>(
        &'a self,
        key_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<KeyRecord>, LoginError>> {
        Box::pin(async move {
            let Some(id) = self.id_for_key_id(key_id).await? else {
                return Ok(None);
            };
            self.record_of(id).await
        })
    }
}

/// The domain record of one stored `api_key` row.
fn key_of(id: RecordId, data: &DocumentData, digest: TokenDigest) -> Result<KeyRecord, LoginError> {
    let text = |field: &str| {
        data.fields
            .iter()
            .find(|(name, _)| name.as_str() == field)
            .and_then(|(_, value)| text(value))
    };
    let key_id = text("key_id").ok_or_else(|| super::store_failed("api_key: no key_id"))?;
    let owner = data
        .fields
        .iter()
        .find(|(name, _)| name.as_str() == "owner")
        .and_then(|(_, value)| record(value))
        .ok_or_else(|| super::store_failed("api_key: no owner"))?;
    let state = text("state")
        .and_then(|value| KeyState::parse(&value))
        .ok_or_else(|| super::store_failed("api_key: unreadable state"))?;
    let expires_at = data
        .fields
        .iter()
        .find(|(name, _)| name.as_str() == "expires_at")
        .and_then(|(_, value)| seconds(value))
        .map(|seconds| {
            time::OffsetDateTime::from_unix_timestamp(seconds)
                .map_err(|_| super::store_failed("api_key: unreadable expires_at"))
        })
        .transpose()?;
    let _ = id;
    Ok(KeyRecord {
        key_id,
        digest,
        owner,
        state,
        expires_at,
    })
}
