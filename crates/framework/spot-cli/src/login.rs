//! Real sign-in for a site: the sign-in service's ports backed by storage and the site file.
//!
//! Credential *shape* lives in the platform app, but its secret fields have no storage column
//! until the secrets service lands (WP-SEC-10). This host therefore keeps each user's Argon2id
//! verifier in the protected `site.toml`, next to the API key digests, exactly as T2 did for
//! `spot set-password`. Everything else about a user (name, e-mail, state) is read from the
//! platform records as the system principal.
//!
//! A one-time first sign-in secret is stored once, as an Argon2id verifier marked `first_sign_in`
//! with an `expires_at`. It is **not** an API key: sent as a bearer credential it is
//! refused. Once it has expired it is refused as a wrong password, with the same work done as for
//! any wrong password. A session it starts is remembered as restricted, and until the user
//! chooses a password the request gate (`crate::gate`) allows nothing but the password change,
//! sign-out and reading the session.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use spot_auth::{ApiKey, HashParams, Password, PasswordHash, StoredApiKey, hash_password};
use spot_core::Clock;
use spot_http::api::service::{
    ApiError, AuthService, BoxFuture, LoginBody, RequestCtx, Session, SessionInfo,
};
use spot_login::model::{KeyRecord, KeyState, UserRecord, UserState};
use spot_login::ports::{Argon2Hasher, BoxFuture as PortFuture, KeyStore, UserDirectory};
use spot_login::{LoginConfig, LoginService};
use spot_platform_app::doctype;
use spot_postgres::PgBackend;
use spot_sessions::InMemorySessionStore;
use spot_storage_contract::{
    Catalog, DocumentData, DocumentStore, Expr, Isolation, Name, Query, QueryExecutor, QueryPath,
    RecordId, RowLock, Selection, StorageBackend, Transaction, TxOptions, Value,
};

use crate::audit::{
    AuditedThrottleStore, AuditingEvents, BoundedThrottleStore, PathClass, Recorder,
};
use crate::config::{self, Key};
use crate::gate::RestrictedSessions;
use crate::password_throttle::PasswordThrottle;
use crate::{Error, Result, auth, error, sitefile};

/// The longest sign-in name the throttle can key (the login contract's bound on `user`).
const ACCOUNT_NAME_MAX: usize = 320;

fn name(text: &str) -> Result<Name> {
    Name::new(text).map_err(error)
}

fn options() -> Result<TxOptions> {
    Ok(TxOptions::new(
        name("spot_cli_login")?,
        Isolation::ReadCommitted,
    ))
}

fn storage_error(e: spot_storage_contract::StorageError) -> spot_login::LoginError {
    spot_login::LoginError::new(
        spot_login::LoginErrorKind::StoreFailed,
        format!("user store failed: {e}"),
    )
}

/// One password verifier as the sign-in service sees it.
#[derive(Clone)]
pub struct Entry {
    hash: PasswordHash,
    first_sign_in: bool,
    expired: bool,
}

/// One stored verifier, with when a one-time secret stops working.
#[derive(Clone)]
struct Stored {
    hash: PasswordHash,
    first_sign_in: bool,
    expires_at: Option<i64>,
}

/// The site's password verifiers, cached in memory and written through to `site.toml`.
pub struct Credentials {
    root: PathBuf,
    site: String,
    entries: Mutex<BTreeMap<String, Stored>>,
    clock: Arc<dyn Clock>,
    /// What an expired one-time secret is checked against, so the work done is that of any
    /// other wrong password and the answer cannot tell the two apart.
    dummy: PasswordHash,
}

impl Credentials {
    /// Reads the verifiers of `site`. A malformed verifier is a configuration error.
    pub fn load(root: &Path, site: &config::Site, clock: Arc<dyn Clock>) -> Result<Self> {
        let mut entries = BTreeMap::new();
        for credential in &site.credentials {
            let hash = PasswordHash::parse(&credential.password_hash)
                .map_err(|_| Error("invalid password verifier in site.toml".into()))?;
            entries.insert(
                credential.user.clone(),
                Stored {
                    hash,
                    first_sign_in: credential.first_sign_in,
                    expires_at: credential.expires_at,
                },
            );
        }
        let (unused, _) = ApiKey::generate();
        Ok(Self {
            root: root.to_path_buf(),
            site: site.site.clone(),
            entries: Mutex::new(entries),
            clock,
            dummy: hash_password(&Password::new(&unused.text()), &HashParams::default()),
        })
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<String, Stored>> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The verifier for a user id, when the user has a password or a one-time secret. A
    /// one-time secret that has expired, or that carries no expiry (an earlier version wrote
    /// it), is returned as `expired` with a verifier nothing matches.
    pub fn get(&self, user: &RecordId) -> Option<Entry> {
        let now = self.clock.now_utc().unix_timestamp();
        self.lock().get(&user.to_string()).map(|stored| {
            let expired = stored.first_sign_in && !stored.expires_at.is_some_and(|at| now < at);
            Entry {
                hash: if expired {
                    self.dummy.clone()
                } else {
                    stored.hash.clone()
                },
                first_sign_in: stored.first_sign_in,
                expired,
            }
        })
    }

    /// Whether the user's verifier is still a one-time first sign-in secret (even an expired
    /// one: the user has not chosen a password, so it is still restricted).
    pub fn is_first_sign_in(&self, user: &RecordId) -> bool {
        self.lock()
            .get(&user.to_string())
            .is_some_and(|entry| entry.first_sign_in)
    }

    /// Replaces a user's verifier with a chosen password.
    pub fn set(&self, user: RecordId, hash: PasswordHash) -> Result<()> {
        let user = user.to_string();
        let stored = CredentialWrite {
            user: user.clone(),
            hash: hash.phc_string().to_owned(),
        };
        sitefile::update(&self.root, &self.site, |site| {
            site.credentials.retain(|c| c.user != user);
            site.credentials.push(config::Credential {
                user: stored.user.clone(),
                password_hash: stored.hash.clone(),
                first_sign_in: false,
                expires_at: None,
            });
            Ok(())
        })?;
        self.lock().insert(
            user,
            Stored {
                hash,
                first_sign_in: false,
                expires_at: None,
            },
        );
        Ok(())
    }
}

struct CredentialWrite {
    user: String,
    hash: String,
}

/// A user directory over the site's platform `user` records and the site's verifiers.
pub struct SiteDirectory {
    backend: PgBackend,
    catalog: Arc<Catalog>,
    credentials: Arc<Credentials>,
}

impl SiteDirectory {
    /// A directory over one site's storage.
    pub fn new(backend: PgBackend, catalog: Arc<Catalog>, credentials: Arc<Credentials>) -> Self {
        Self {
            backend,
            catalog,
            credentials,
        }
    }

    /// The user's record id for a sign-in name, case-insensitively.
    async fn find_id(
        &self,
        identifier: &str,
    ) -> std::result::Result<Option<RecordId>, spot_login::LoginError> {
        let identifier = identifier.trim().to_lowercase();
        let mut tx = self
            .backend
            .begin(
                self.catalog.clone(),
                options().map_err(|e| {
                    spot_login::LoginError::new(
                        spot_login::LoginErrorKind::StoreFailed,
                        e.to_string(),
                    )
                })?,
            )
            .await
            .map_err(storage_error)?;
        let result = find_by(&mut tx, &identifier).await;
        let _ = tx.rollback().await;
        result
    }

    /// Whether this sign-in name still holds a one-time first sign-in secret.
    pub async fn is_first_sign_in(
        &self,
        identifier: &str,
    ) -> std::result::Result<bool, spot_login::LoginError> {
        Ok(self
            .find_id(identifier)
            .await?
            .is_some_and(|id| self.credentials.is_first_sign_in(&id)))
    }

    async fn record(
        &self,
        id: RecordId,
    ) -> std::result::Result<Option<UserRecord>, spot_login::LoginError> {
        let mut tx = self
            .backend
            .begin(
                self.catalog.clone(),
                options().map_err(|e| {
                    spot_login::LoginError::new(
                        spot_login::LoginErrorKind::StoreFailed,
                        e.to_string(),
                    )
                })?,
            )
            .await
            .map_err(storage_error)?;
        let user = name(doctype::USER).map_err(|e| {
            spot_login::LoginError::new(spot_login::LoginErrorKind::StoreFailed, e.to_string())
        })?;
        let loaded = tx.load(&user, id, RowLock::None).await;
        let _ = tx.rollback().await;
        let Some(loaded) = loaded.map_err(storage_error)? else {
            return Ok(None);
        };
        Ok(record_of(id, &loaded.data, self.credentials.get(&id)))
    }
}

impl UserDirectory for SiteDirectory {
    fn find_by_identifier<'a>(
        &'a self,
        identifier: &'a str,
    ) -> PortFuture<'a, std::result::Result<Option<UserRecord>, spot_login::LoginError>> {
        Box::pin(async move {
            let Some(id) = self.find_id(identifier).await? else {
                return Ok(None);
            };
            self.record(id).await
        })
    }

    fn find_by_id<'a>(
        &'a self,
        id: RecordId,
    ) -> PortFuture<'a, std::result::Result<Option<UserRecord>, spot_login::LoginError>> {
        Box::pin(async move { self.record(id).await })
    }

    fn set_password_hash<'a>(
        &'a self,
        id: RecordId,
        hash: PasswordHash,
    ) -> PortFuture<'a, std::result::Result<(), spot_login::LoginError>> {
        Box::pin(async move {
            self.credentials.set(id, hash).map_err(|e| {
                spot_login::LoginError::new(spot_login::LoginErrorKind::StoreFailed, e.to_string())
            })
        })
    }

    fn record_totp_step<'a>(
        &'a self,
        _: RecordId,
        _: u64,
    ) -> PortFuture<'a, std::result::Result<(), spot_login::LoginError>> {
        // TOTP secrets have no storage column yet (WP-SEC-10); no user can enrol here.
        Box::pin(async { Ok(()) })
    }

    fn consume_recovery_digest<'a>(
        &'a self,
        _: RecordId,
        _: usize,
    ) -> PortFuture<'a, std::result::Result<(), spot_login::LoginError>> {
        Box::pin(async { Ok(()) })
    }

    fn set_second_factor<'a>(
        &'a self,
        _: RecordId,
        _: Option<spot_auth::TotpSecret>,
        _: Vec<spot_auth::recovery::StoredDigest>,
    ) -> PortFuture<'a, std::result::Result<(), spot_login::LoginError>> {
        Box::pin(async { Ok(()) })
    }
}

fn text(fields: &BTreeMap<Name, Value>, field: &str) -> Option<String> {
    fields.iter().find_map(|(name, value)| {
        (name.as_str() == field)
            .then(|| match value {
                Value::Text(text) => Some(text.clone()),
                _ => None,
            })
            .flatten()
    })
}

fn record_of(id: RecordId, data: &DocumentData, credential: Option<Entry>) -> Option<UserRecord> {
    let user_name = text(&data.fields, "user_name")?;
    let state = text(&data.fields, "state").and_then(|s| UserState::parse(&s))?;
    let (password_hash, first_sign_in) = credential
        .map(|entry| (Some(entry.hash), entry.first_sign_in && !entry.expired))
        .unwrap_or((None, false));
    Some(UserRecord {
        id,
        user_name,
        email: text(&data.fields, "email").filter(|e| !e.is_empty()),
        full_name: text(&data.fields, "full_name").unwrap_or_default(),
        // An invitation with a one-time secret is signable; the first successful sign-in
        // asks for a password before the client can do anything else.
        state: if first_sign_in && matches!(state, UserState::Invited) {
            UserState::Active
        } else {
            // A disabled or anonymised user stays so, whatever verifier is left in the file.
            state
        },
        password_hash,
        totp_secret: None,
        recovery_digests: Vec::new(),
        totp_last_step: None,
    })
}

async fn find_by<T: QueryExecutor>(
    tx: &mut T,
    identifier: &str,
) -> std::result::Result<Option<RecordId>, spot_login::LoginError> {
    let entity = name(doctype::USER).map_err(|e| {
        spot_login::LoginError::new(spot_login::LoginErrorKind::StoreFailed, e.to_string())
    })?;
    let alias = name("u").map_err(|e| {
        spot_login::LoginError::new(spot_login::LoginErrorKind::StoreFailed, e.to_string())
    })?;
    let mut query = Query::from(&entity, &alias, QueryPath::Report);
    query.select = vec![Selection {
        expr: Expr::field(
            &alias,
            &name("id").map_err(|e| {
                spot_login::LoginError::new(spot_login::LoginErrorKind::StoreFailed, e.to_string())
            })?,
        ),
        alias: name("v").map_err(|e| {
            spot_login::LoginError::new(spot_login::LoginErrorKind::StoreFailed, e.to_string())
        })?,
    }];
    let value = Expr::value(Value::Text(identifier.to_owned()));
    query.filter = Some(Expr::Or(vec![
        Expr::field(
            &alias,
            &name("user_name").map_err(|e| {
                spot_login::LoginError::new(spot_login::LoginErrorKind::StoreFailed, e.to_string())
            })?,
        )
        .eq(value.clone()),
        Expr::field(
            &alias,
            &name("email").map_err(|e| {
                spot_login::LoginError::new(spot_login::LoginErrorKind::StoreFailed, e.to_string())
            })?,
        )
        .eq(value),
    ]));
    query.limit = Some(1);
    let rows = tx.query(&query).await.map_err(storage_error)?;
    Ok(rows.rows.into_iter().flatten().find_map(|value| {
        if let Value::Id(id) = value {
            Some(id)
        } else {
            None
        }
    }))
}

/// The site's API key verifiers, as the sign-in service's key store.
pub struct FileKeyStore {
    keys: Vec<Key>,
}

impl FileKeyStore {
    /// A key store over the site file's verifiers.
    pub fn new(keys: Vec<Key>) -> Self {
        Self { keys }
    }
}

impl KeyStore for FileKeyStore {
    fn find_by_key_id<'a>(
        &'a self,
        key_id: &'a str,
    ) -> PortFuture<'a, std::result::Result<Option<KeyRecord>, spot_login::LoginError>> {
        Box::pin(async move {
            let found = self.keys.iter().find(|key| key.key_id == key_id);
            let Some(key) = found else {
                return Ok(None);
            };
            let stored: StoredApiKey = auth::stored(key).map_err(|e| {
                spot_login::LoginError::new(spot_login::LoginErrorKind::StoreFailed, e.to_string())
            })?;
            let owner = spot_http::api::codec::parse_record_id(&key.user).ok_or_else(|| {
                spot_login::LoginError::new(
                    spot_login::LoginErrorKind::StoreFailed,
                    "invalid API key owner",
                )
            })?;
            Ok(Some(KeyRecord {
                key_id: stored.key_id,
                digest: stored.digest,
                owner,
                state: KeyState::Active,
                // An expiry that cannot be represented counts as already expired.
                expires_at: key.expires_at.map(|at| {
                    time::OffsetDateTime::from_unix_timestamp(at)
                        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH)
                }),
            }))
        })
    }
}

/// The in-memory state a site keeps across a reload, so a reload does not sign everyone out and
/// does not turn a restricted session into a free one.
///
/// **Sessions and throttle counters are per process**: they are not written to the
/// site database, so a restart signs everyone out and forgets every lockout. This is a stated
/// limitation, not an accident: persisting them needs the secrets service (WP-SEC-10) for the
/// session token digests. Both are bounded: a user holds at most the session policy's
/// `max_per_user` sessions, and the throttle tracks at most
/// [`THROTTLE_KEY_CAP`](crate::audit::THROTTLE_KEY_CAP) keys, evicting the oldest key that is
/// not a live lockout and refusing sign-ins (fail closed) if every key is one.
#[derive(Clone)]
pub struct LoginStores {
    /// Sessions.
    pub sessions: Arc<InMemorySessionStore>,
    /// Sign-in throttle counters (bounded, per process).
    pub throttle: Arc<BoundedThrottleStore>,
    /// The sessions a one-time secret started.
    pub restricted: Arc<RestrictedSessions>,
    /// The site's identity audit trail.
    pub recorder: Arc<Recorder>,
}

impl LoginStores {
    /// Empty stores over a site's audit recorder.
    pub fn new(recorder: Arc<Recorder>, clock: Arc<dyn Clock>) -> Self {
        Self {
            sessions: Arc::default(),
            throttle: Arc::new(BoundedThrottleStore::new(clock)),
            restricted: Arc::default(),
            recorder,
        }
    }

    /// The limitation, as the line the host logs at start.
    pub const LIMITATION: &'static str = "sessions and sign-in throttle counters are held in this process only: a restart signs every user out and forgets every lockout";
}

/// The site's `AuthService` and the extra first-sign-in and password-change operations.
pub struct SiteAuth {
    service: Arc<LoginService>,
    directory: Arc<SiteDirectory>,
    credentials: Arc<Credentials>,
    restricted: Arc<RestrictedSessions>,
    clock: Arc<dyn Clock>,
    recorder: Arc<Recorder>,
    /// The site file's keys, read only to name the reason a refused key is audited under.
    keys: Arc<FileKeyStore>,
    /// The per-user throttle of the password-change route (WP-PRD-01.T14).
    password_throttle: PasswordThrottle<AuditedThrottleStore<BoundedThrottleStore>>,
}

impl SiteAuth {
    /// Builds the sign-in service over the site's stores, or over the stores a reload keeps.
    pub fn new(
        config: LoginConfig,
        directory: Arc<SiteDirectory>,
        credentials: Arc<Credentials>,
        keys: Vec<Key>,
        clock: Arc<dyn Clock>,
        stores: &LoginStores,
    ) -> Result<Self> {
        let hasher = Arc::new(Argon2Hasher::new(config.hash_params.clone()));
        let password_throttle = PasswordThrottle::new(
            AuditedThrottleStore::new(stores.throttle.clone(), stores.recorder.clone()),
            config.throttle,
            clock.clone(),
        )
        .map_err(|e| Error(e.to_string()))?;
        let keys = Arc::new(FileKeyStore::new(keys));
        let service = LoginService::new(
            config,
            stores.sessions.clone(),
            Arc::new(AuditedThrottleStore::new(
                stores.throttle.clone(),
                stores.recorder.clone(),
            )),
            directory.clone(),
            keys.clone(),
            Arc::new(AuditingEvents::new(
                stores.recorder.clone(),
                stores.sessions.clone(),
            )),
            clock.clone(),
            hasher,
        )
        .map_err(|e| Error(e.to_string()))?;
        Ok(Self {
            service: Arc::new(service),
            directory,
            credentials,
            restricted: stores.restricted.clone(),
            clock,
            recorder: stores.recorder.clone(),
            keys,
            password_throttle,
        })
    }

    /// Signs in and reports whether the session is restricted to choosing a password, which is
    /// the case when the verifier used was a one-time secret.
    pub async fn login_with_source(
        &self,
        ctx: &RequestCtx,
        body: &LoginBody,
        source: &str,
    ) -> std::result::Result<(Session, bool), ApiError> {
        if spot_login::normalise_identifier(&body.user).len() > ACCOUNT_NAME_MAX {
            // The sign-in service cannot key a name this long and would answer with a server
            // error before auditing anything. It cannot match a user, so it is an ordinary wrong
            // credential, audited with the name cut down.
            let mut record = self.recorder.record("sign_in_attempt");
            record.attempted = Some(crate::audit::redact_name(&body.user));
            record.reason = Some("invalid_identifier".into());
            self.recorder.failure(&record);
            return Err(spot_login::LoginError::new(
                spot_login::LoginErrorKind::InvalidCredentials,
                "",
            )
            .to_api_error());
        }
        let before = self
            .directory
            .is_first_sign_in(&body.user)
            .await
            .map_err(|e| e.to_api_error())?;
        let session = self.service.login_with_source(ctx, body, source).await?;
        // Looked at again after the verifier was checked: whichever way the site file changed
        // in between, a sign-in that may have used a one-time secret is restricted.
        let after = self
            .directory
            .is_first_sign_in(&body.user)
            .await
            .map_err(|e| e.to_api_error())?;
        let first = before || after;
        if first {
            let issued = RequestCtx {
                site: ctx.site.clone(),
                credential: Some(spot_http::api::service::Credential::new(&session.token)),
                idempotency_key: None,
                language: None,
                request_id: ctx.request_id.clone(),
                peer: None,
            };
            let principal = self.service.authenticate(&issued).await?;
            if let Some(id) = &principal.session_id {
                self.restricted.insert(
                    id,
                    principal.expires_at,
                    self.clock.now_utc().unix_timestamp(),
                );
            }
        }
        Ok((session, first))
    }

    /// The caller's credential as the sign-in service sees it.
    pub async fn authenticate(
        &self,
        ctx: &RequestCtx,
    ) -> std::result::Result<spot_login::Authenticated, ApiError> {
        self.authenticate_on(ctx, PathClass::Api).await
    }

    /// Like [`SiteAuth::authenticate`], naming the class of route the credential was presented
    /// to. An API key that is refused is audited (WP-PRD-01.T15); the caller gets exactly the
    /// service's answer whether or not the record is written.
    pub async fn authenticate_on(
        &self,
        ctx: &RequestCtx,
        path: PathClass,
    ) -> std::result::Result<spot_login::Authenticated, ApiError> {
        let result = self.service.authenticate(ctx).await;
        if let Err(error) = &result
            && error.code.as_str() == "auth.invalid_credentials"
            && let Some(credential) = &ctx.credential
            && credential.token.starts_with("spk_")
        {
            self.audit_rejected_key(&credential.token, path).await;
        }
        result
    }

    async fn audit_rejected_key(&self, presented: &str, path: PathClass) {
        // The store is the site file's, in memory: this lookup costs a scan and one digest, and
        // runs whether or not the record is within budget, so the budget is no timing oracle.
        let found = match ApiKey::parse(presented) {
            Ok(key) => self.keys.find_by_key_id(key.key_id()).await.ok().flatten(),
            Err(_) => None,
        };
        let reason = crate::audit::classify(presented, found.as_ref(), self.clock.now_utc());
        crate::audit::rejected(&self.recorder, presented, reason, path);
    }

    /// Whether the authenticated caller may do nothing but choose a password: its session was
    /// started by a one-time secret, or its user still holds one (so an API key of that user is
    /// restricted too).
    pub fn is_restricted(&self, principal: &spot_login::Authenticated) -> bool {
        principal
            .session_id
            .as_deref()
            .is_some_and(|id| self.restricted.contains(id))
            || self.credentials.is_first_sign_in(&principal.user_id)
    }

    fn audit_password_change(&self, user: &str, reason: &str) {
        let mut record = self.recorder.record("password_change_failed");
        record.user = Some(user.to_owned());
        record.reason = Some(reason.to_owned());
        self.recorder.failure(&record);
    }

    /// Changes the caller's password and completes a first sign-in: the user becomes active and
    /// the one-time secret stops working. A restricted session is *replaced*: the old one is
    /// revoked and a new one (a new id, unrestricted) is returned. Other callers get `None`.
    pub async fn change_password(
        &self,
        ctx: &RequestCtx,
        current: &str,
        new: &str,
        source: &str,
    ) -> std::result::Result<Option<Session>, ApiError> {
        let principal = self.service.authenticate(ctx).await?;
        let was_restricted = self.is_restricted(&principal);
        let user = principal.user_id.to_string();
        let request_id = ctx.request_id.as_str();
        // The throttle comes first: a locked user is refused before the password is looked at,
        // so the answer is the same for a right and a wrong one and no hashing is done.
        if let Err(refusal) = self.password_throttle.admit(&user, request_id).await {
            self.audit_password_change(&user, "password_change_locked");
            return Err(refusal);
        }
        if let Err(error) = self.service.change_password(ctx, current, new).await {
            // A refused change is audited (and bounded like any failure an attacker holding a
            // stolen session can repeat); the caller gets the service's answer unchanged.
            if error.code.as_str() == "auth.invalid_credentials" {
                self.audit_password_change(&user, "password_change_wrong_current");
                // Fail closed: if the failure cannot be counted the route refuses.
                self.password_throttle.failed(&user, request_id).await?;
            } else {
                if error.code.as_str() == "auth.password_policy" {
                    // The current password was right; only the new one was refused.
                    self.password_throttle.succeeded(&user).await;
                }
                let reason: String = error
                    .code
                    .as_str()
                    .chars()
                    .filter(|c| c.is_ascii_lowercase() || matches!(c, '_' | '.'))
                    .take(48)
                    .collect();
                self.audit_password_change(&user, &reason);
            }
            return Err(error);
        }
        self.password_throttle.succeeded(&user).await;
        activate_user(&self.directory, principal.user_id)
            .await
            .map_err(|e| {
                ApiError::new(spot_http::api::registry::Code::HttpInternal, e.to_string())
            })?;
        let Some(old) = principal.session_id.as_deref().filter(|_| was_restricted) else {
            return Ok(None);
        };
        // The old session ends before the new one starts: there is no moment with both.
        self.service.logout(ctx).await?;
        self.restricted.remove(old);
        let body = LoginBody {
            user: principal.user_name.clone(),
            password: new.to_owned(),
            totp: None,
            recovery_code: None,
        };
        // A failure here leaves the user signed out with the password changed: they sign in.
        Ok(self
            .service
            .login_with_source(ctx, &body, source)
            .await
            .ok())
    }
}

/// How every user session signs in. A session is only started by a password sign-in (SSO starts
/// none here, and an API key is a bearer credential with no session of its own), so the method
/// the contract's `SessionRecord` names is always this one until SSO lands (ADR-070 section 7).
pub const SESSION_METHOD: &str = "password";

/// The audit kind of a user ending one of their own sessions.
pub const SESSION_END_KIND: &str = "session_ended_by_user";

/// The audit reason of that ending.
pub const SESSION_END_REASON: &str = "user_ended";

/// One live session of the caller, as the session list shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnSession {
    /// The session's opaque id.
    pub id: String,
    /// Unix seconds the session started.
    pub created: i64,
    /// Unix seconds it was last seen.
    pub last_seen: i64,
    /// Unix seconds of its absolute expiry.
    pub expires_at: i64,
    /// The salted source digest, never the address.
    pub source: String,
    /// The device label, when one was recorded.
    pub device: Option<String>,
    /// Whether it is the session the request is served under.
    pub current: bool,
}

impl SiteAuth {
    /// The caller, when the caller is a person who may manage sessions: a session bearer that is
    /// not restricted to choosing a password. An API key is not a person and has no session; it
    /// is refused (`permission.denied`), as is a restricted session.
    async fn session_manager(
        &self,
        ctx: &RequestCtx,
    ) -> std::result::Result<spot_login::Authenticated, ApiError> {
        let principal = self.authenticate_on(ctx, PathClass::Api).await?;
        if principal.session_id.is_none()
            || !matches!(principal.via, spot_login::Via::Password)
            || self.is_restricted(&principal)
        {
            return Err(ApiError::from_code(
                spot_http::api::registry::Code::PermissionDenied,
            ));
        }
        Ok(principal)
    }

    /// The caller's own live sessions: the current one first, then the others newest first
    /// (`auth_session_list`).
    ///
    /// # Errors
    ///
    /// The shared authentication failures; `permission.denied` for an API key or a restricted
    /// session.
    pub async fn own_sessions(
        &self,
        ctx: &RequestCtx,
    ) -> std::result::Result<Vec<OwnSession>, ApiError> {
        self.session_manager(ctx).await?;
        let mut sessions: Vec<OwnSession> = self
            .service
            .list_sessions(ctx)
            .await?
            .into_iter()
            .filter(|session| session.live)
            .map(|session| OwnSession {
                id: session.id,
                created: session.created,
                last_seen: session.last_seen,
                expires_at: session.expires_at,
                source: session.source,
                device: session.device,
                current: session.current,
            })
            .collect();
        sessions.sort_by(|a, b| {
            b.current
                .cmp(&a.current)
                .then(b.created.cmp(&a.created))
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(sessions)
    }

    /// Ends one of the caller's own live sessions (`auth_session_end`). An id that is not in the
    /// caller's live list (unknown, another user's, already ended) is one answer,
    /// `auth.session_not_found`. The ending is audited before it happens, and an ending that
    /// cannot be audited is refused (fail closed); the audit record names the user, the session id
    /// and the reason `user_ended`, never a token.
    ///
    /// # Errors
    ///
    /// The shared authentication failures; `permission.denied` for an API key or a restricted
    /// session; `auth.session_not_found`.
    pub async fn end_own_session(
        &self,
        ctx: &RequestCtx,
        id: &str,
    ) -> std::result::Result<(), ApiError> {
        let principal = self.session_manager(ctx).await?;
        let live = self.own_sessions(ctx).await?;
        if !live.iter().any(|session| session.id == id) {
            return Err(spot_login::LoginError::new(
                spot_login::LoginErrorKind::SessionNotFound,
                "",
            )
            .to_api_error_traced(ctx.request_id.as_str()));
        }
        let mut record = self.recorder.record(SESSION_END_KIND);
        record.user = Some(principal.user_id.to_string());
        record.session = Some(id.to_owned());
        record.reason = Some(SESSION_END_REASON.to_owned());
        self.recorder.required(&record).map_err(|_| {
            ApiError::from_code(spot_http::api::registry::Code::ServerUnavailable)
                .detail("retry_after", serde_json::Value::from(5))
        })?;
        self.service.end_session(ctx, id).await?;
        self.restricted.remove(id);
        Ok(())
    }
}

impl AuthService for SiteAuth {
    fn login<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        body: &'a LoginBody,
    ) -> BoxFuture<'a, std::result::Result<Session, ApiError>> {
        Box::pin(async move { self.login_with_source(ctx, body, "").await.map(|(s, _)| s) })
    }

    fn logout<'a>(
        &'a self,
        ctx: &'a RequestCtx,
    ) -> BoxFuture<'a, std::result::Result<(), ApiError>> {
        Box::pin(async move {
            let session = self
                .service
                .authenticate(ctx)
                .await
                .ok()
                .and_then(|principal| principal.session_id);
            self.service.logout(ctx).await?;
            if let Some(id) = session {
                self.restricted.remove(&id);
            }
            Ok(())
        })
    }

    fn session<'a>(
        &'a self,
        ctx: &'a RequestCtx,
    ) -> BoxFuture<'a, std::result::Result<SessionInfo, ApiError>> {
        Box::pin(async move { self.service.session(ctx).await })
    }
}

/// Marks a user active after their first password is chosen.
async fn activate_user(directory: &SiteDirectory, user: RecordId) -> Result<()> {
    let mut tx = directory
        .backend
        .begin(directory.catalog.clone(), options()?)
        .await
        .map_err(error)?;
    let entity = name(doctype::USER)?;
    let loaded = tx.load(&entity, user, RowLock::None).await.map_err(error)?;
    let Some(loaded) = loaded else {
        let _ = tx.rollback().await;
        return Err(Error("the user record no longer exists".into()));
    };
    let mut data = loaded.data;
    data.fields
        .insert(name("state")?, Value::Text("active".into()));
    tx.update(&data, loaded.version).await.map_err(error)?;
    tx.commit().await.map_err(error)
}

/// The sign-in configuration this host runs with. Sessions are re-checked on every request
/// (`cache_ttl` zero) so a disabled user's session stops working at once.
pub fn default_login_config() -> LoginConfig {
    LoginConfig {
        cache_ttl: Duration::ZERO,
        ..LoginConfig::default()
    }
}

/// The site's clock.
pub fn system_clock() -> Arc<dyn Clock> {
    Arc::new(spot_core::SystemClock::new())
}
