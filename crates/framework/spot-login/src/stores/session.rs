//! [`StorageSessionStore`]: sessions as `session` records, token digests as lookup secrets.
//!
//! # What is stored where
//!
//! A `session` record carries its window, its user link, its client label and its state as
//! ordinary columns; the token digest is a `secret` with `lookup = true`, so the engine seals it
//! into `token_digest__sealed` and blind-indexes it into `token_digest__bidx`, in the same
//! transaction as the record. A session is therefore found by
//! [`Engine::find_by_secret`](spot_document::engine::Engine::find_by_secret) — one indexed
//! equality on the blind index — and never by a plain-text column: the table holds no form of
//! the token and no form of its digest that a reader could resolve.
//!
//! # End reasons
//!
//! The platform app's `session.state` is a three-value select (`active`, `ended`, `expired`) and
//! there is no column for an end reason. This store uses the two ended values for the two
//! distinctions the schema can carry: a session ended on purpose (logout, password change, an
//! administrator) is `ended`; one the *system* ended because a newer sign-in crossed the
//! per-user cap is `expired`. Expiry by the clock is never written (it is derived from the
//! window, half-open), so `expired` unambiguously means "evicted". A dedicated `ended_reason`
//! column would let every reason round-trip; the report lists it as an open question.
//!
//! The `session` DocType also has no column for the source digest, so [`SessionStore::list_for_user`]
//! and [`SessionStore::active_sources`] answer with the zero digest. That is a display loss, not
//! a leak: the salted digest was never going to be shown back.

use std::sync::Arc;

use spot_app_api::Document;
use spot_auth::TokenDigest;
use spot_platform_app::doctype::SESSION;
use spot_secrets::SecretString;
use spot_sessions::error::InternalReason;
use spot_sessions::model::SessionEndReason;
use spot_sessions::model::TOUCH_GRANULARITY;
use spot_sessions::store::{RevokeReport, SessionFuture, SessionRecord, Touched, purgeable};
use spot_sessions::{ClientInfo, SessionError, SessionState, SessionStore, SourceHash, UserKey};
use spot_storage_contract::{
    CmpOp, DocumentStore, Expr, Fields, Name, Nulls, Order, OrderDir, Query, QueryExecutor,
    QueryPath, RecordId, RowLock, Selection, StorageBackend, Transaction, Value, Wait,
};

use super::{LoginStores, hex, instant, name, parse_record, record, seconds, text, unhex};

/// Sessions over the platform app's `session` records.
pub struct StorageSessionStore<B: StorageBackend> {
    stores: Arc<LoginStores<B>>,
}

impl<B: StorageBackend> StorageSessionStore<B> {
    /// A store over one site's engine and capability.
    #[must_use]
    pub fn new(stores: Arc<LoginStores<B>>) -> Self {
        Self { stores }
    }

    fn entity() -> Result<Name, SessionError> {
        name(SESSION).map_err(|error| SessionError::StoreFailed(error.store_text()))
    }

    fn column(text: &str) -> Result<Name, SessionError> {
        name(text).map_err(|error| SessionError::StoreFailed(error.store_text()))
    }

    /// The one answer for a digest or id that cannot resolve to a session.
    fn unknown() -> SessionError {
        SessionError::Invalid {
            reason: Box::new(InternalReason::UnknownDigest),
        }
    }

    fn storage(what: &str, error: &spot_storage_contract::StorageError) -> SessionError {
        SessionError::StoreFailed(super::failed_text(what, error))
    }

    fn label(suffix: &str) -> String {
        format!("spot_login_{suffix}")
    }

    fn id_of(session_id: &str) -> Result<RecordId, SessionError> {
        parse_record(session_id).ok_or_else(Self::unknown)
    }

    /// The unix seconds of one stored date field.
    fn at(fields: &Fields, field: &str) -> Result<i64, SessionError> {
        fields
            .iter()
            .find(|(name, _)| name.as_str() == field)
            .and_then(|(_, value)| seconds(value))
            .ok_or_else(|| SessionError::StoreFailed(format!("session: `{field}` is not a date")))
    }

    /// The text of one stored field.
    fn text_at(fields: &Fields, field: &str) -> Option<String> {
        fields
            .iter()
            .find(|(name, _)| name.as_str() == field)
            .and_then(|(_, value)| text(value))
    }

    /// The domain record of a stored row, with the digest the caller already holds.
    fn read(
        id: RecordId,
        fields: &Fields,
        digest: TokenDigest,
    ) -> Result<SessionRecord, SessionError> {
        let user_text = fields
            .iter()
            .find(|(name, _)| name.as_str() == "user")
            .and_then(|(_, value)| record(value))
            .map(|id| id.to_string())
            .ok_or_else(|| SessionError::StoreFailed("session: no user".to_owned()))?;
        let user = UserKey::new(user_text)
            .map_err(|reason| SessionError::StoreFailed(format!("session: {reason}")))?;
        Ok(SessionRecord {
            id: id.to_string(),
            user,
            token_digest: digest,
            created: Self::at(fields, "created_at")?,
            last_seen: Self::at(fields, "last_seen")?,
            idle_expires: Self::at(fields, "idle_expires_at")?,
            absolute_expires: Self::at(fields, "absolute_expires_at")?,
            client: decode_client(Self::text_at(fields, "client_info")),
            source: SourceHash::from_bytes(&[0u8; 32]).map_err(SessionError::StoreFailed)?,
            state: decode_state(Self::text_at(fields, "state"))?,
        })
    }

    /// The digest stored in one record, opened through the capability.
    async fn digest_of(&self, id: RecordId) -> Result<Option<TokenDigest>, SessionError> {
        let opened = self
            .stores
            .engine()
            .open_secret(self.stores.access(), SESSION, id, "token_digest")
            .await
            .map_err(|error| {
                SessionError::StoreFailed(super::failed_text("open token_digest", &error))
            })?;
        let Some(secret) = opened else {
            return Ok(None);
        };
        let bytes = unhex(secret.expose())
            .ok_or_else(|| SessionError::StoreFailed("token_digest: not a digest".to_owned()))?;
        TokenDigest::from_bytes(&bytes)
            .map(Some)
            .map_err(|error| SessionError::StoreFailed(format!("token_digest: {error}")))
    }

    /// Finds the record a token digest belongs to, through the blind index.
    async fn id_for_digest(&self, digest: &TokenDigest) -> Result<Option<RecordId>, SessionError> {
        self.stores
            .engine()
            .find_by_secret(
                self.stores.access(),
                SESSION,
                "token_digest",
                hex(digest.as_bytes()).as_bytes(),
            )
            .await
            .map_err(|error| SessionError::StoreFailed(super::failed_text("find session", &error)))
    }

    /// Every session id of `user`, oldest first (creation order).
    async fn ids_for(&self, user: &UserKey) -> Result<Vec<RecordId>, SessionError> {
        let user_id = parse_record(user.as_str()).ok_or_else(Self::unknown)?;
        let entity = Self::entity()?;
        let alias = Self::column("s")?;
        let mut query = Query::from(&entity, &alias, QueryPath::Report);
        query.select = vec![Selection {
            expr: Expr::field(&alias, &Self::column("id")?),
            alias: Self::column("v")?,
        }];
        query.filter =
            Some(Expr::field(&alias, &Self::column("user")?).eq(Expr::value(Value::Id(user_id))));
        query.order_by = vec![Order {
            expr: Expr::field(&alias, &Self::column("created_at")?),
            dir: OrderDir::Asc,
            nulls: Nulls::First,
        }];
        let mut tx = self
            .stores
            .begin(&Self::label("list_ids"), false)
            .await
            .map_err(|error| SessionError::StoreFailed(error.store_text()))?;
        let rows = tx
            .query(&query)
            .await
            .map_err(|error| Self::storage("list sessions", &error));
        let _ = tx.rollback().await;
        Ok(rows?
            .rows
            .into_iter()
            .flatten()
            .filter_map(|value| record(&value))
            .collect())
    }

    /// One record by id, digest included.
    async fn by_id(&self, id: RecordId) -> Result<Option<SessionRecord>, SessionError> {
        let Some(digest) = self.digest_of(id).await? else {
            return Ok(None);
        };
        let mut tx = self
            .stores
            .begin(&Self::label("read_session"), false)
            .await
            .map_err(|error| SessionError::StoreFailed(error.store_text()))?;
        let entity = Self::entity()?;
        let loaded = tx.load(&entity, id, RowLock::None).await;
        let _ = tx.rollback().await;
        let Some(loaded) = loaded.map_err(|error| Self::storage("read session", &error))? else {
            return Ok(None);
        };
        Self::read(id, &loaded.data.fields, digest).map(Some)
    }

    /// The session body of a record without its digest (for the paths that never answer with a
    /// record: `touch`, `purge`).
    /// The new idle expiry a touch at `now` would write, or `None` when there is nothing to
    /// write: the session is not active, is already idle-expired, or the clamped candidate is
    /// inside the granularity window of the stored expiry.
    fn touch_candidate(
        fields: &Fields,
        now: i64,
        idle: i64,
        absolute_expires: i64,
    ) -> Result<Option<i64>, SessionError> {
        let (state, idle_expires, _) = Self::state_fields(fields)?;
        if !matches!(state, SessionState::Active) || now >= idle_expires {
            return Ok(None);
        }
        // The clamp: never past the absolute bound, and no write inside the granularity window
        // (a busy client writes at most once per granularity).
        let candidate = (now + idle).min(absolute_expires);
        if candidate - idle_expires < TOUCH_GRANULARITY.as_secs() as i64 {
            return Ok(None);
        }
        Ok(Some(candidate))
    }

    fn state_fields(fields: &Fields) -> Result<(SessionState, i64, i64), SessionError> {
        Ok((
            decode_state(Self::text_at(fields, "state"))?,
            Self::at(fields, "idle_expires_at")?,
            Self::at(fields, "absolute_expires_at")?,
        ))
    }
}

impl<B: StorageBackend> SessionStore for StorageSessionStore<B> {
    fn create<'a>(&'a self, session: SessionRecord) -> SessionFuture<'a, SessionRecord> {
        Box::pin(async move {
            // The manager hands an empty id: the store assigns it (and echoes it back, so
            // the caller gets a record whose id is real).
            let id = if session.id.is_empty() {
                RecordId::new_v7()
            } else {
                Self::id_of(&session.id)?
            };
            let user = parse_record(session.user.as_str()).ok_or_else(Self::unknown)?;
            let mut doc = Document::new(Self::entity()?, id);
            doc.set(Self::column("user")?, Value::Id(user));
            doc.set(Self::column("state")?, Value::Text("active".to_owned()));
            doc.set(Self::column("method")?, Value::Text("password".to_owned()));
            let date = |seconds: i64| -> Result<Value, SessionError> {
                instant(seconds)
                    .map(Value::DateTime)
                    .map_err(|error| SessionError::StoreFailed(error.store_text()))
            };
            doc.set(Self::column("created_at")?, date(session.created)?);
            doc.set(Self::column("last_seen")?, date(session.last_seen)?);
            doc.set(
                Self::column("idle_expires_at")?,
                date(session.idle_expires)?,
            );
            doc.set(
                Self::column("absolute_expires_at")?,
                date(session.absolute_expires)?,
            );
            if let Some(label) = encode_client(&session.client) {
                doc.set(Self::column("client_info")?, Value::Text(label));
            }
            // The digest, and only the digest, reaches storage: sealed with the record in one
            // transaction and blind-indexed, because the field is a lookup field.
            self.stores
                .engine()
                .create_with_secrets(
                    self.stores.access(),
                    doc,
                    vec![(
                        "token_digest".to_owned(),
                        SecretString::new(hex(session.token_digest.as_bytes())),
                    )],
                )
                .await
                .map_err(|error| {
                    SessionError::StoreFailed(super::failed_text("create session", &error))
                })?;
            Ok(SessionRecord {
                id: id.to_string(),
                ..session
            })
        })
    }

    fn find_by_digest<'a>(
        &'a self,
        digest: &'a TokenDigest,
    ) -> SessionFuture<'a, Option<SessionRecord>> {
        Box::pin(async move {
            let Some(id) = self.id_for_digest(digest).await? else {
                return Ok(None);
            };
            let mut tx = self
                .stores
                .begin(&Self::label("find_session"), false)
                .await
                .map_err(|error| SessionError::StoreFailed(error.store_text()))?;
            let entity = Self::entity()?;
            let loaded = tx.load(&entity, id, RowLock::None).await;
            let _ = tx.rollback().await;
            let Some(loaded) = loaded.map_err(|error| Self::storage("read session", &error))?
            else {
                return Ok(None);
            };
            let digest = TokenDigest::from_bytes(digest.as_bytes())
                .map_err(|reason| SessionError::StoreFailed(reason.to_string()))?;
            Self::read(id, &loaded.data.fields, digest).map(Some)
        })
    }

    fn touch<'a>(
        &'a self,
        id: &'a str,
        now: i64,
        idle: i64,
        absolute_expires: i64,
    ) -> SessionFuture<'a, Touched> {
        Box::pin(async move {
            let record_id = Self::id_of(id)?;
            let entity = Self::entity()?;
            // First a plain read: the hot path (a busy client inside the granularity window, or a
            // dead session) must not open a write transaction or take a row lock.
            let mut read = self
                .stores
                .begin(&Self::label("touch_session_read"), false)
                .await
                .map_err(|error| SessionError::StoreFailed(error.store_text()))?;
            let peeked = read.load(&entity, record_id, RowLock::None).await;
            let _ = read.rollback().await;
            let Some(peeked) = peeked.map_err(|error| Self::storage("touch read", &error))? else {
                return Ok(Touched::Skipped);
            };
            if Self::touch_candidate(&peeked.data.fields, now, idle, absolute_expires)?.is_none() {
                return Ok(Touched::Skipped);
            }
            // A write is due: reload under the lock and decide again on the locked row.
            let mut tx = self
                .stores
                .begin(&Self::label("touch_session"), true)
                .await
                .map_err(|error| SessionError::StoreFailed(error.store_text()))?;
            let loaded = tx
                .load(&entity, record_id, RowLock::Update(Wait::Block))
                .await;
            let Some(mut loaded) = loaded.map_err(|error| Self::storage("touch load", &error))?
            else {
                let _ = tx.rollback().await;
                return Ok(Touched::Skipped);
            };
            let Some(candidate) =
                Self::touch_candidate(&loaded.data.fields, now, idle, absolute_expires)?
            else {
                let _ = tx.rollback().await;
                return Ok(Touched::Skipped);
            };
            let window = instant(candidate)
                .map_err(|error| SessionError::StoreFailed(error.store_text()))?;
            let seen =
                instant(now).map_err(|error| SessionError::StoreFailed(error.store_text()))?;
            loaded
                .data
                .fields
                .insert(Self::column("idle_expires_at")?, Value::DateTime(window));
            loaded
                .data
                .fields
                .insert(Self::column("last_seen")?, Value::DateTime(seen));
            let updated = tx.update(&loaded.data, loaded.version).await;
            match updated {
                Ok(_) => {
                    tx.commit()
                        .await
                        .map_err(|error| Self::storage("commit touch", &error))?;
                    Ok(Touched::Extended)
                }
                Err(error) => {
                    let _ = tx.rollback().await;
                    Err(Self::storage("touch session", &error))
                }
            }
        })
    }

    fn rotate<'a>(&'a self, id: &'a str, new_digest: TokenDigest) -> SessionFuture<'a, ()> {
        Box::pin(async move {
            let record_id = Self::id_of(id)?;
            let mut tx = self
                .stores
                .begin(&Self::label("rotate_session"), false)
                .await
                .map_err(|error| SessionError::StoreFailed(error.store_text()))?;
            let entity = Self::entity()?;
            let loaded = tx.load(&entity, record_id, RowLock::None).await;
            let _ = tx.rollback().await;
            let version = loaded
                .map_err(|error| Self::storage("rotate load", &error))?
                .ok_or_else(Self::unknown)?
                .version;
            // One sealed write: the old digest stops resolving the instant it commits.
            self.stores
                .engine()
                .set_secret(
                    self.stores.access(),
                    SESSION,
                    record_id,
                    "token_digest",
                    SecretString::new(hex(new_digest.as_bytes())),
                    version,
                )
                .await
                .map(|_| ())
                .map_err(|error| {
                    SessionError::StoreFailed(super::failed_text("rotate session", &error))
                })
        })
    }

    fn revoke<'a>(&'a self, id: &'a str, reason: String) -> SessionFuture<'a, RevokeReport> {
        Box::pin(async move {
            let Ok(record_id) = Self::id_of(id) else {
                return Ok(RevokeReport { revoked: 0 });
            };
            let mut tx = self
                .stores
                .begin(&Self::label("revoke_session"), true)
                .await
                .map_err(|error| SessionError::StoreFailed(error.store_text()))?;
            let entity = Self::entity()?;
            let loaded = tx
                .load(&entity, record_id, RowLock::Update(Wait::Block))
                .await;
            let Some(mut loaded) = loaded.map_err(|error| Self::storage("revoke load", &error))?
            else {
                let _ = tx.rollback().await;
                return Ok(RevokeReport { revoked: 0 });
            };
            if !matches!(
                decode_state(Self::text_at(&loaded.data.fields, "state"))?,
                SessionState::Active
            ) {
                // Ended already: the first end stands (idempotent revoke).
                let _ = tx.rollback().await;
                return Ok(RevokeReport { revoked: 0 });
            }
            loaded
                .data
                .fields
                .insert(Self::column("state")?, Value::Text(encode_state(&reason)));
            let updated = tx.update(&loaded.data, loaded.version).await;
            match updated {
                Ok(_) => {
                    tx.commit()
                        .await
                        .map_err(|error| Self::storage("commit revoke", &error))?;
                    Ok(RevokeReport { revoked: 1 })
                }
                Err(error) => {
                    let _ = tx.rollback().await;
                    Err(Self::storage("revoke session", &error))
                }
            }
        })
    }

    fn revoke_all_for_user<'a>(
        &'a self,
        user: &'a UserKey,
        except: Option<&'a str>,
        reason: String,
    ) -> SessionFuture<'a, RevokeReport> {
        Box::pin(async move {
            let mut revoked = 0;
            for id in self.ids_for(user).await? {
                let text_id = id.to_string();
                if except == Some(text_id.as_str()) {
                    continue;
                }
                revoked += self.revoke(&text_id, reason.clone()).await?.revoked;
            }
            Ok(RevokeReport { revoked })
        })
    }

    fn list_for_user<'a>(&'a self, user: &'a UserKey) -> SessionFuture<'a, Vec<SessionRecord>> {
        Box::pin(async move {
            let mut out = Vec::new();
            for id in self.ids_for(user).await? {
                if let Some(session) = self.by_id(id).await? {
                    out.push(session);
                }
            }
            Ok(out)
        })
    }

    fn purge_expired<'a>(&'a self, now: i64, limit: usize) -> SessionFuture<'a, usize> {
        Box::pin(async move {
            let at = instant(now).map_err(|error| SessionError::StoreFailed(error.store_text()))?;
            let entity = Self::entity()?;
            let alias = Self::column("s")?;
            let mut query = Query::from(&entity, &alias, QueryPath::Report);
            query.select = vec![Selection {
                expr: Expr::field(&alias, &Self::column("id")?),
                alias: Self::column("v")?,
            }];
            query.filter = Some(Expr::Or(vec![
                Expr::field(&alias, &Self::column("absolute_expires_at")?)
                    .cmp(CmpOp::Le, Expr::value(Value::DateTime(at))),
                Expr::field(&alias, &Self::column("idle_expires_at")?)
                    .cmp(CmpOp::Le, Expr::value(Value::DateTime(at))),
            ]));
            query.limit = Some(u32::try_from(limit).unwrap_or(u32::MAX));
            let ids = {
                let mut tx = self
                    .stores
                    .begin(&Self::label("purge_query"), false)
                    .await
                    .map_err(|error| SessionError::StoreFailed(error.store_text()))?;
                let rows = tx.query(&query).await;
                let _ = tx.rollback().await;
                rows.map_err(|error| Self::storage("purge query", &error))?
                    .rows
                    .into_iter()
                    .flatten()
                    .filter_map(|value| record(&value))
                    .collect::<Vec<_>>()
            };
            let mut purged = 0;
            for id in ids {
                let mut tx = self
                    .stores
                    .begin(&Self::label("purge_one"), true)
                    .await
                    .map_err(|error| SessionError::StoreFailed(error.store_text()))?;
                let entity = Self::entity()?;
                let loaded = tx.load(&entity, id, RowLock::Update(Wait::Block)).await;
                let Some(loaded) = loaded.map_err(|error| Self::storage("purge load", &error))?
                else {
                    let _ = tx.rollback().await;
                    continue;
                };
                let session = SessionRecord {
                    id: id.to_string(),
                    user: UserKey::new("00000000-0000-0000-0000-000000000000")
                        .map_err(|reason| SessionError::StoreFailed(reason.to_string()))?,
                    token_digest: TokenDigest::from_bytes(&[0u8; 32])
                        .map_err(|reason| SessionError::StoreFailed(reason.to_string()))?,
                    created: 0,
                    last_seen: 0,
                    idle_expires: Self::at(&loaded.data.fields, "idle_expires_at")?,
                    absolute_expires: Self::at(&loaded.data.fields, "absolute_expires_at")?,
                    client: ClientInfo::default(),
                    source: SourceHash::from_bytes(&[0u8; 32])
                        .map_err(|reason| SessionError::StoreFailed(reason.to_string()))?,
                    state: SessionState::Active,
                };
                if !purgeable(&session, now) {
                    let _ = tx.rollback().await;
                    continue;
                }
                let deleted = tx.delete(&entity, id, loaded.version).await;
                match deleted {
                    Ok(()) => {
                        tx.commit()
                            .await
                            .map_err(|error| Self::storage("commit purge", &error))?;
                        purged += 1;
                    }
                    Err(error) => {
                        let _ = tx.rollback().await;
                        return Err(Self::storage("purge delete", &error));
                    }
                }
            }
            Ok(purged)
        })
    }

    fn oldest_active<'a>(
        &'a self,
        user: &'a UserKey,
        now: i64,
    ) -> SessionFuture<'a, Option<SessionRecord>> {
        Box::pin(async move {
            for id in self.ids_for(user).await? {
                if let Some(session) = self.by_id(id).await?
                    && session.alive_at(now)
                {
                    return Ok(Some(session));
                }
            }
            Ok(None)
        })
    }

    fn exists<'a>(&'a self, digest: &'a TokenDigest) -> SessionFuture<'a, bool> {
        Box::pin(async move { Ok(self.id_for_digest(digest).await?.is_some()) })
    }

    fn active_sources<'a>(&'a self, user: &'a UserKey) -> SessionFuture<'a, Vec<SourceHash>> {
        Box::pin(async move {
            let mut out = Vec::new();
            // Every session that has not been revoked; expiry is a clock question this
            // signature does not carry (`SessionStore::active_sources` passes no `now`).
            for id in self.ids_for(user).await? {
                if let Some(session) = self.by_id(id).await?
                    && !session.is_revoked()
                {
                    out.push(
                        SourceHash::from_bytes(session.source.as_bytes())
                            .map_err(|reason| SessionError::StoreFailed(reason.to_string()))?,
                    );
                }
            }
            Ok(out)
        })
    }
}

/// The stored `state` text of an end reason: the two ended values the select has.
fn encode_state(reason: &str) -> String {
    if reason == SessionEndReason::Evicted.to_string() {
        "expired".to_owned()
    } else {
        "ended".to_owned()
    }
}

/// The domain state of a stored `state` text.
fn decode_state(stored: Option<String>) -> Result<SessionState, SessionError> {
    match stored.as_deref() {
        Some("active") => Ok(SessionState::Active),
        Some("ended") => Ok(SessionState::Revoked {
            reason: SessionEndReason::Ended,
        }),
        // Never written for a clock expiry (that stays derived), so it is the eviction the
        // schema can record.
        Some("expired") => Ok(SessionState::Revoked {
            reason: SessionEndReason::Evicted,
        }),
        _ => Err(SessionError::StoreFailed(
            "session: unreadable state".to_owned(),
        )),
    }
}

/// The device and client labels as one bounded text column.
fn encode_client(client: &ClientInfo) -> Option<String> {
    match (client.device.as_deref(), client.client.as_deref()) {
        (None, None) => None,
        (Some(device), None) => Some(device.to_owned()),
        (None, Some(app)) => Some(format!("/{app}")),
        (Some(device), Some(app)) => Some(format!("{device}/{app}")),
    }
}

/// The labels back from the one text column.
fn decode_client(stored: Option<String>) -> ClientInfo {
    let Some(stored) = stored else {
        return ClientInfo::default();
    };
    match stored.split_once('/') {
        Some(("", app)) => ClientInfo {
            device: None,
            client: Some(app.to_owned()),
        },
        Some((device, app)) => ClientInfo {
            device: Some(device.to_owned()),
            client: Some(app.to_owned()),
        },
        None => ClientInfo {
            device: Some(stored),
            client: None,
        },
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{decode_client, decode_state, encode_client, encode_state};
    use spot_sessions::model::SessionEndReason;
    use spot_sessions::{ClientInfo, SessionState};

    #[test]
    fn client_labels_round_trip() {
        for client in [
            ClientInfo::default(),
            ClientInfo::new(Some("laptop"), None).expect("valid"),
            ClientInfo::new(None, Some("app")).expect("valid"),
            ClientInfo::new(Some("laptop"), Some("app")).expect("valid"),
        ] {
            assert_eq!(decode_client(encode_client(&client)), client);
        }
    }

    #[test]
    fn eviction_is_the_only_system_end_the_schema_can_hold() {
        assert_eq!(encode_state("evicted"), "expired");
        assert_eq!(encode_state("password_changed"), "ended");
        assert_eq!(
            decode_state(Some("expired".to_owned())).expect("known state"),
            SessionState::Revoked {
                reason: SessionEndReason::Evicted
            }
        );
        assert!(decode_state(Some("gone".to_owned())).is_err());
        assert!(decode_state(None).is_err());
    }
}
