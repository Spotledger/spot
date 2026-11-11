//! [`Authenticator`]: turns a request credential into an authenticated principal.
//!
//! Every secured service calls this before doing anything. It accepts either an opaque session
//! token or an `spk_<key_id>_<secret>` API key. Session lookups are cached for a short TTL so the
//! hot path is not a store read per request; the cache is invalidated on revoke **by this node**,
//! and a revoke on another node takes effect within the TTL.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use spot_auth::{ApiKey, OpaqueToken};
use spot_core::Clock;
use spot_http::api::{ApiError, Code, RequestCtx};
use spot_sessions::{SaltKey, SessionManager, SessionPolicy, SessionStore, UserKey};
use spot_storage_contract::RecordId;

use crate::error::{LoginError, invalid_credentials};
use crate::model::UserState;
use crate::ports::{KeyStore, UserDirectory};

/// How the caller proved who they are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Via {
    /// A session token issued after a password (and any second factor) sign-in.
    Password,
    /// An API key.
    ApiKey,
}

/// The authenticated principal a request acts as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Authenticated {
    /// The user's stable record id.
    pub user_id: RecordId,
    /// The user's sign-in name.
    pub user_name: String,
    /// How the caller proved identity.
    pub via: Via,
    /// The session's id, for a session credential; `None` for an API key.
    pub session_id: Option<String>,
    /// Unix seconds the credential stops working.
    pub expires_at: i64,
    /// A rotated session token the caller must deliver, when rotation fell due. Never cached.
    pub renewed_token: Option<String>,
}

impl Authenticated {
    /// The same principal without a to-deliver rotated token, for the cache.
    fn cached(&self) -> Self {
        Self {
            renewed_token: None,
            ..self.clone()
        }
    }
}

/// One cached session lookup: the principal and when the entry stops being trusted.
struct CacheEntry {
    principal: Authenticated,
    valid_until: Duration,
}

/// Validates session tokens and API keys, with a short per-node session cache.
pub struct Authenticator {
    sessions: SessionManager<Arc<dyn SessionStore>>,
    users: Arc<dyn UserDirectory>,
    keys: Arc<dyn KeyStore>,
    clock: Arc<dyn Clock>,
    ttl: Duration,
    cache: Mutex<HashMap<[u8; 32], CacheEntry>>,
}

impl Authenticator {
    /// Builds an authenticator over the shared session store and the user and key ports.
    #[must_use]
    pub fn new(
        store: Arc<dyn SessionStore>,
        policy: SessionPolicy,
        salt: Arc<SaltKey>,
        users: Arc<dyn UserDirectory>,
        keys: Arc<dyn KeyStore>,
        clock: Arc<dyn Clock>,
        ttl: Duration,
    ) -> Self {
        Self {
            sessions: SessionManager::new(store, policy, salt),
            users,
            keys,
            clock,
            ttl,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// The staleness bound: a revoke on another node is honoured within this many seconds.
    #[must_use]
    pub fn cache_ttl(&self) -> Duration {
        self.ttl
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<[u8; 32], CacheEntry>> {
        self.cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Drops one token's cached lookup; called on every revoke by the same node.
    pub fn invalidate_token(&self, token: &str) {
        if let Ok(presented) = OpaqueToken::parse(token) {
            let key = *presented.digest().as_bytes();
            self.lock().remove(&key);
        }
    }

    /// Drops the whole cache; called after a bulk revoke (a password change, a deactivation).
    pub fn invalidate_all(&self) {
        self.lock().clear();
    }

    fn cached(&self, key: &[u8; 32]) -> Option<Authenticated> {
        let now = self.clock.monotonic();
        let cache = self.lock();
        let entry = cache.get(key)?;
        if entry.valid_until > now {
            Some(entry.principal.clone())
        } else {
            None
        }
    }

    fn remember(&self, key: [u8; 32], principal: &Authenticated) {
        let valid_until = self.clock.monotonic() + self.ttl;
        self.lock().insert(
            key,
            CacheEntry {
                principal: principal.cached(),
                valid_until,
            },
        );
    }

    /// Authenticates the request's credential, or explains why it cannot.
    pub async fn authenticate(&self, ctx: &RequestCtx) -> Result<Authenticated, ApiError> {
        let credential = ctx
            .credential
            .as_ref()
            .ok_or_else(|| ApiError::from(Code::AuthRequired))?;
        let token = credential.token.as_str();
        if token.starts_with("spk_") {
            self.authenticate_key(token, ctx.request_id.as_str()).await
        } else {
            self.authenticate_session(token, ctx.request_id.as_str())
                .await
        }
    }

    async fn authenticate_session(
        &self,
        token: &str,
        request_id: &str,
    ) -> Result<Authenticated, ApiError> {
        let now = self.clock.now_utc().unix_timestamp();
        let presented = OpaqueToken::parse(token).map_err(|_| session_expired())?;
        let key = *presented.digest().as_bytes();
        if let Some(principal) = self.cached(&key) {
            return Ok(principal);
        }

        let validated = self
            .sessions
            .validate(token, now)
            .await
            .map_err(|error| match error {
                error @ spot_sessions::SessionError::StoreFailed(_) => {
                    LoginError::from(error).to_api_error_traced(request_id)
                }
                _ => session_expired(),
            })?;

        let user_id =
            record_id_from_text(validated.session.user.as_str()).ok_or_else(session_expired)?;
        let user = self
            .users
            .find_by_id(user_id)
            .await
            .map_err(|error| error.to_api_error_traced(request_id))?
            .ok_or_else(session_expired)?;
        if !user.state.is_signable() {
            return Err(session_expired());
        }

        let principal = Authenticated {
            user_id,
            user_name: user.user_name,
            via: Via::Password,
            session_id: Some(validated.session.id.clone()),
            expires_at: validated.session.absolute_expires,
            renewed_token: validated
                .renewed_token
                .map(|token| token.as_str().to_owned()),
        };
        self.remember(*validated.session.token_digest.as_bytes(), &principal);
        Ok(principal)
    }

    async fn authenticate_key(
        &self,
        token: &str,
        request_id: &str,
    ) -> Result<Authenticated, ApiError> {
        let now = self.clock.now_utc();
        let presented = ApiKey::parse(token).map_err(|_| key_rejected())?;
        let record = self
            .keys
            .find_by_key_id(presented.key_id())
            .await
            .map_err(|error| error.to_api_error_traced(request_id))?
            .ok_or_else(key_rejected)?;

        let same_id = record.key_id == presented.key_id();
        let same_secret = record.digest.matches(&presented.digest());
        if !(same_id && same_secret) || record.state != crate::model::KeyState::Active {
            return Err(key_rejected());
        }
        if record.expires_at.is_some_and(|expires| now >= expires) {
            return Err(key_rejected());
        }

        let user = self
            .users
            .find_by_id(record.owner)
            .await
            .map_err(|error| error.to_api_error_traced(request_id))?
            .ok_or_else(key_rejected)?;
        if user.state != UserState::Active {
            return Err(key_rejected());
        }

        Ok(Authenticated {
            user_id: record.owner,
            user_name: user.user_name,
            via: Via::ApiKey,
            session_id: None,
            expires_at: record
                .expires_at
                .map_or(i64::MAX, |instant| instant.unix_timestamp()),
            renewed_token: None,
        })
    }
}

/// The one external answer for a dead session token.
fn session_expired() -> ApiError {
    ApiError::new(
        Code::AuthSessionExpired,
        "The session has ended; sign in again.",
    )
}

/// The one external answer for a missing, revoked, expired or mis-owned API key.
fn key_rejected() -> ApiError {
    invalid_credentials()
}

/// Parses a hyphenated UUIDv7 back into a [`RecordId`] (the `UserKey` stores it as text).
pub(crate) fn record_id_from_text(text: &str) -> Option<RecordId> {
    let hex: Vec<u8> = text.bytes().filter(|byte| *byte != b'-').collect();
    if hex.len() != 32 {
        return None;
    }
    let digit = |byte: u8| -> Option<u8> { (byte as char).to_digit(16).map(|value| value as u8) };
    let mut bytes = [0u8; 16];
    for (slot, pair) in bytes.iter_mut().zip(hex.chunks(2)) {
        let (high, low) = match pair {
            [high, low] => (digit(*high)?, digit(*low)?),
            _ => return None,
        };
        *slot = high * 16 + low;
    }
    RecordId::from_bytes(bytes).ok()
}

/// The user key for a record id, as `spot-sessions` wants it.
pub(crate) fn user_key(id: RecordId) -> Result<UserKey, LoginError> {
    UserKey::new(id.to_string())
        .map_err(|reason| LoginError::new(crate::error::LoginErrorKind::StoreFailed, reason))
}
