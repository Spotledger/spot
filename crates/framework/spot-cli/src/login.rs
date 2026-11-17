//! Real sign-in for a site: the sign-in service over the storage-backed stores of `spot-login`
//! (WP-PLT-02.T8).
//!
//! Sessions, the per-account throttle counters, password verifiers, second factors, recovery
//! codes and API key digests are records of the site database; every credential travels the
//! secrets service (`spot_login::LoginStores`, sealed fields). A restart or a reload keeps every
//! session and every lockout, and two nodes on one database share both.
//!
//! What still lives in `site.toml`: **one-time first sign-in secrets** (`first_sign_in = true`)
//! until the user chooses a password. They are short-lived (72 hours by default), are not API
//! keys, and are refused once expired (with the same work as for any wrong password).
//! A session one of them starts is restricted, and until the user chooses a password the request
//! gate (`crate::gate`) allows nothing but the password change, sign-out and reading the session
//!. Chosen-password verifiers and API key digests that an operator command stages in
//! the file (`spot set-password`, `spot create-user`) are imported into the sealed fields by the
//! `login_stores` service at every start and reload, in one write per user, and then removed from
//! the file (see `crate::services::login_stores`).
//!
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
use spot_login::{LoginConfig, LoginService, StorageKeyStore, StorageUserDirectory};
use spot_platform_app::doctype;
use spot_postgres::PgBackend;
use spot_storage_contract::{
    Catalog, DocumentStore, Isolation, Name, RecordId, RowLock, StorageBackend, Transaction,
    TxOptions, Value,
};

use crate::audit::{
    AuditedAdmission, AuditedThrottleStore, AuditingEvents, BoundedThrottleStore, PathClass,
    Recorder,
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

/// One password verifier as the sign-in service sees it.
#[derive(Clone)]
pub struct Entry {
    hash: PasswordHash,
    first_sign_in: bool,
    expired: bool,
}

/// One staged one-time secret, with when it stops working.
#[derive(Clone)]
struct Stored {
    hash: PasswordHash,
    expires_at: Option<i64>,
}

/// The one-time first sign-in secrets still in `site.toml`, cached in memory and written
/// through. Everything else the file used to hold is in the site database (module docs).
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
    /// Reads the one-time secrets of `site`. A malformed verifier is a configuration error. A
    /// verifier that is not a one-time secret is **not** read here: the `login_stores` service
    /// imports it into the database (and it is never a sign-in path from the file).
    pub fn load(root: &Path, site: &config::Site, clock: Arc<dyn Clock>) -> Result<Self> {
        let mut entries = BTreeMap::new();
        for credential in site.credentials.iter().filter(|c| c.first_sign_in) {
            let hash = PasswordHash::parse(&credential.password_hash)
                .map_err(|_| Error("invalid password verifier in site.toml".into()))?;
            entries.insert(
                credential.user.clone(),
                Stored {
                    hash,
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

    /// The one-time verifier for a user id, when the user still has one. One that has expired,
    /// or that carries no expiry (an earlier version wrote it), is returned as `expired` with a
    /// verifier nothing matches.
    pub fn get(&self, user: &RecordId) -> Option<Entry> {
        let now = self.clock.now_utc().unix_timestamp();
        self.lock().get(&user.to_string()).map(|stored| {
            let expired = !stored.expires_at.is_some_and(|at| now < at);
            Entry {
                hash: if expired {
                    self.dummy.clone()
                } else {
                    stored.hash.clone()
                },
                first_sign_in: true,
                expired,
            }
        })
    }

    /// Whether the user still holds a one-time first sign-in secret (even an expired one: the
    /// user has not chosen a password, so it is still restricted).
    pub fn is_first_sign_in(&self, user: &RecordId) -> bool {
        self.lock().contains_key(&user.to_string())
    }

    /// Forgets (and removes from the file) the user's one-time secret: a chosen password ends it.
    pub fn clear(&self, user: RecordId) -> Result<()> {
        let user = user.to_string();
        if !self.lock().contains_key(&user) {
            return Ok(());
        }
        sitefile::update(&self.root, &self.site, |site| {
            site.credentials
                .retain(|c| !(c.user == user && c.first_sign_in));
            Ok(())
        })?;
        self.lock().remove(&user);
        Ok(())
    }
}

/// A user directory over the site's `user` records (credentials sealed, `spot_login`'s stores)
/// with the site file's one-time first sign-in secrets laid over it.
pub struct SiteDirectory {
    inner: Arc<StorageUserDirectory<PgBackend>>,
    backend: PgBackend,
    catalog: Arc<Catalog>,
    credentials: Arc<Credentials>,
}

impl SiteDirectory {
    /// A directory over one site's storage.
    pub fn new(
        stores: &Arc<spot_login::LoginStores<PgBackend>>,
        backend: PgBackend,
        catalog: Arc<Catalog>,
        credentials: Arc<Credentials>,
    ) -> Self {
        Self {
            inner: Arc::new(stores.user_directory()),
            backend,
            catalog,
            credentials,
        }
    }

    /// Whether this sign-in name still holds a one-time first sign-in secret.
    pub async fn is_first_sign_in(
        &self,
        identifier: &str,
    ) -> std::result::Result<bool, spot_login::LoginError> {
        Ok(self
            .inner
            .find_by_identifier(identifier)
            .await?
            .is_some_and(|user| self.credentials.is_first_sign_in(&user.id)))
    }

    /// The stored record with a staged one-time secret laid over it: the secret is the
    /// verifier, and an invitation with a live one is signable (the first sign-in asks for a
    /// password before the client can do anything else). A disabled or anonymised user stays so.
    fn overlay(&self, mut user: UserRecord) -> UserRecord {
        if let Some(entry) = self.credentials.get(&user.id) {
            let live = entry.first_sign_in && !entry.expired;
            user.password_hash = Some(entry.hash);
            if live && matches!(user.state, UserState::Invited) {
                user.state = UserState::Active;
            }
        }
        user
    }
}

impl UserDirectory for SiteDirectory {
    fn find_by_identifier<'a>(
        &'a self,
        identifier: &'a str,
    ) -> PortFuture<'a, std::result::Result<Option<UserRecord>, spot_login::LoginError>> {
        Box::pin(async move {
            Ok(self
                .inner
                .find_by_identifier(identifier)
                .await?
                .map(|user| self.overlay(user)))
        })
    }

    fn find_by_id<'a>(
        &'a self,
        id: RecordId,
    ) -> PortFuture<'a, std::result::Result<Option<UserRecord>, spot_login::LoginError>> {
        Box::pin(async move {
            Ok(self
                .inner
                .find_by_id(id)
                .await?
                .map(|user| self.overlay(user)))
        })
    }

    fn find_standing_by_id<'a>(
        &'a self,
        id: RecordId,
    ) -> PortFuture<'a, std::result::Result<Option<UserRecord>, spot_login::LoginError>> {
        Box::pin(async move {
            Ok(self
                .inner
                .find_standing_by_id(id)
                .await?
                .map(|user| self.overlay(user)))
        })
    }

    fn set_password_hash<'a>(
        &'a self,
        id: RecordId,
        hash: PasswordHash,
    ) -> PortFuture<'a, std::result::Result<(), spot_login::LoginError>> {
        Box::pin(async move {
            // The sealed write first, then the one-time secret ends: a crash in between leaves
            // both, and the staged secret still expires.
            self.inner.set_password_hash(id, hash).await?;
            self.credentials.clear(id).map_err(|e| {
                spot_login::LoginError::new(spot_login::LoginErrorKind::StoreFailed, e.to_string())
            })
        })
    }

    fn record_totp_step<'a>(
        &'a self,
        id: RecordId,
        step: u64,
    ) -> PortFuture<'a, std::result::Result<(), spot_login::LoginError>> {
        self.inner.record_totp_step(id, step)
    }

    fn consume_recovery_digest<'a>(
        &'a self,
        id: RecordId,
        index: usize,
    ) -> PortFuture<'a, std::result::Result<(), spot_login::LoginError>> {
        self.inner.consume_recovery_digest(id, index)
    }

    fn set_second_factor<'a>(
        &'a self,
        id: RecordId,
        totp: Option<spot_auth::TotpSecret>,
        recovery: Vec<spot_auth::recovery::StoredDigest>,
    ) -> PortFuture<'a, std::result::Result<(), spot_login::LoginError>> {
        self.inner.set_second_factor(id, totp, recovery)
    }

    fn begin_totp_enrolment<'a>(
        &'a self,
        id: RecordId,
        secret: spot_auth::TotpSecret,
    ) -> PortFuture<'a, std::result::Result<(), spot_login::LoginError>> {
        self.inner.begin_totp_enrolment(id, secret)
    }

    fn pending_totp<'a>(
        &'a self,
        id: RecordId,
    ) -> PortFuture<'a, std::result::Result<Option<spot_auth::TotpSecret>, spot_login::LoginError>>
    {
        self.inner.pending_totp(id)
    }
}

/// In-memory API key verifiers: the development token of `spot serve --dev` (never persisted).
pub struct FileKeyStore {
    keys: Vec<Key>,
}

impl FileKeyStore {
    /// A key store over in-memory verifiers.
    pub fn new(keys: Vec<Key>) -> Self {
        Self { keys }
    }

    fn find(&self, key_id: &str) -> std::result::Result<Option<KeyRecord>, spot_login::LoginError> {
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
            scim_only: false,
        }))
    }
}

/// The site's API keys: the database first, then the in-memory development token.
pub struct SiteKeyStore {
    stored: StorageKeyStore<PgBackend>,
    dev: FileKeyStore,
}

impl SiteKeyStore {
    /// Keys over the site's storage, plus `dev` in-memory verifiers.
    pub fn new(stores: &Arc<spot_login::LoginStores<PgBackend>>, dev: Vec<Key>) -> Self {
        Self {
            stored: stores.key_store(),
            dev: FileKeyStore::new(dev),
        }
    }
}

impl KeyStore for SiteKeyStore {
    fn find_by_key_id<'a>(
        &'a self,
        key_id: &'a str,
    ) -> PortFuture<'a, std::result::Result<Option<KeyRecord>, spot_login::LoginError>> {
        Box::pin(async move {
            if let Some(found) = KeyStore::find_by_key_id(&self.stored, key_id).await? {
                return Ok(Some(found));
            }
            self.dev.find(key_id)
        })
    }

    fn consume_initial_secret<'a>(
        &'a self,
        user: RecordId,
        presented: &'a spot_auth::OpaqueToken,
        now: i64,
    ) -> PortFuture<'a, std::result::Result<(), spot_login::LoginError>> {
        KeyStore::consume_initial_secret(&self.stored, user, presented, now)
    }
}

/// What the host keeps per site across a reload: the audit trail, the password-change throttle
/// and the sessions a one-time secret started.
///
/// Sessions and the sign-in throttle counters are **not** here any more: they are records of the
/// site database (`spot_login::LoginStores`), so a restart or a reload keeps them and two nodes
/// share them. The password-change throttle is still per process and bounded (it is keyed by user
/// id, which the storage throttle, keyed by sign-in name, cannot hold); its counters reset on a
/// restart, and the sign-in lockout does not.
#[derive(Clone)]
pub struct LoginStores {
    /// Password-change throttle counters (bounded, per process).
    pub throttle: Arc<BoundedThrottleStore>,
    /// The sessions a one-time secret started.
    pub restricted: Arc<RestrictedSessions>,
    /// The site's identity audit trail.
    pub recorder: Arc<Recorder>,
}

impl LoginStores {
    /// Empty per-process state over a site's audit recorder.
    pub fn new(recorder: Arc<Recorder>, clock: Arc<dyn Clock>) -> Self {
        Self {
            throttle: Arc::new(BoundedThrottleStore::new(clock)),
            restricted: Arc::default(),
            recorder,
        }
    }

    /// The one limitation left, as the line the host logs at start.
    pub const LIMITATION: &'static str = "sessions and sign-in lockouts are stored in the site database and survive a restart; only the password-change throttle is held in this process";
}

/// The storage the sign-in service runs over: the engine's stores and the pieces `SiteAuth`
/// needs beside them.
pub struct SiteStorage {
    /// The engine, the catalog and the secrets capability of `spot-login`.
    pub stores: Arc<spot_login::LoginStores<PgBackend>>,
    /// The database, for the one write `spot-login` has no store method for (activating a user).
    pub backend: PgBackend,
    /// The catalog of the platform app.
    pub catalog: Arc<Catalog>,
}

impl SiteStorage {
    /// The stores of `site` over its running engine. The capability is taken once, as
    /// `SecretCaller::LoginService`.
    ///
    /// # Errors
    ///
    /// A site name that is not a site id.
    pub fn new(
        engine: Arc<spot_document::engine::Engine<PgBackend>>,
        catalog: Arc<Catalog>,
        site: &str,
        clock: Arc<dyn Clock>,
    ) -> Result<Self> {
        use spot_app_api::context::{Actor, Invocation};
        let invocation = Invocation {
            site: spot_core::SiteId::new(site).map_err(error)?,
            actor: Actor::System {
                component: name("login_service")?,
            },
            command_id: None,
            language: None,
            now: clock.now_utc(),
        };
        let backend = engine.backend().clone();
        let stores = Arc::new(spot_login::LoginStores::new(
            engine,
            catalog.clone(),
            &invocation,
            clock,
        ));
        Ok(Self {
            stores,
            backend,
            catalog,
        })
    }
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
    keys: Arc<SiteKeyStore>,
    /// The per-user throttle of the password-change route (WP-PRD-01.T14).
    password_throttle: PasswordThrottle<AuditedThrottleStore<BoundedThrottleStore>>,
    /// The site's sign-in throttle, for the password-change door (WP-IAM-11.T24). The host sets
    /// it with [`SiteAuth::with_signin_throttle`]; without it only the per-user throttle above
    /// counts a change attempt.
    signin: Option<Arc<crate::services::signin_throttle::SigninThrottle>>,
}

impl SiteAuth {
    /// The sign-in service, for the second-factor self-service operations (WP-PLT-02.T9).
    #[must_use]
    pub fn login_service(&self) -> &Arc<LoginService> {
        &self.service
    }

    /// Builds the sign-in service over the site's database and the per-process state a reload
    /// keeps. `dev_keys` are the in-memory development verifiers (never persisted); `admission`
    /// is the throttle every sign-in is admitted through (`spot_login::CoordinatorAdmission` in
    /// production).
    pub fn new(
        config: LoginConfig,
        storage: &SiteStorage,
        credentials: Arc<Credentials>,
        dev_keys: Vec<Key>,
        clock: Arc<dyn Clock>,
        stores: &LoginStores,
        admission: Arc<dyn spot_login::SignInAdmission>,
    ) -> Result<Self> {
        let hasher = Arc::new(Argon2Hasher::new(config.hash_params.clone()));
        let directory = Arc::new(SiteDirectory::new(
            &storage.stores,
            storage.backend.clone(),
            storage.catalog.clone(),
            credentials.clone(),
        ));
        let password_throttle = PasswordThrottle::new(
            AuditedThrottleStore::new(stores.throttle.clone(), stores.recorder.clone()),
            config.throttle,
            clock.clone(),
        )
        .map_err(|e| Error(e.to_string()))?;
        let keys = Arc::new(SiteKeyStore::new(&storage.stores, dev_keys));
        let sessions: Arc<dyn spot_sessions::SessionStore> =
            Arc::new(storage.stores.session_store());
        let throttle_store = Arc::new(
            storage
                .stores
                .throttle_store(config.throttle.window)
                .with_lockout(
                    config.throttle.lockout_after,
                    config.throttle.lockout_duration,
                ),
        );
        let service = LoginService::new(
            config,
            sessions.clone(),
            Arc::new(AuditedThrottleStore::new(
                throttle_store,
                stores.recorder.clone(),
            )),
            directory.clone(),
            keys.clone(),
            Arc::new(AuditingEvents::new(stores.recorder.clone(), sessions)),
            clock.clone(),
            hasher,
            // A refusal or an outage of the throttle is audited (WP-IAM-11.T23).
            Arc::new(AuditedAdmission::new(admission, stores.recorder.clone())),
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
            signin: None,
        })
    }

    /// Admits every password change's re-authentication through the throttle's password-change
    /// door, before the per-user throttle and before the current password is looked at.
    #[must_use]
    pub fn with_signin_throttle(
        mut self,
        throttle: Arc<crate::services::signin_throttle::SigninThrottle>,
    ) -> Self {
        self.signin = Some(throttle);
        self
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
        let mut result = self.service.authenticate(ctx).await;
        // A key scoped for SCIM only is the provisioning endpoint's credential, nothing else's.
        if path != PathClass::Scim
            && let Ok(principal) = &result
            && principal.scim_only
        {
            result = Err(ApiError::new(
                spot_http::api::registry::Code::AuthInvalidCredentials,
                "The credentials are wrong or no longer valid.",
            ));
        }
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
        if let Some(handle) = &self.signin {
            let peer = ctx.peer.map(|ip| ip.to_string());
            handle
                .admit(
                    crate::services::signin_throttle::Door::PasswordChange,
                    ctx.site.as_str(),
                    &user,
                    peer.as_deref(),
                    Some(request_id),
                )
                .await?;
        }
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
                let code = if e.to_string().starts_with(PORTAL_ROLE_REFUSED) {
                    spot_http::api::registry::Code::PermissionDenied
                } else {
                    spot_http::api::registry::Code::HttpInternal
                };
                ApiError::new(code, e.to_string())
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

    fn audience_of<'a>(
        &'a self,
        ctx: &'a RequestCtx,
    ) -> BoxFuture<'a, std::result::Result<spot_core::Audience, ApiError>> {
        Box::pin(async move { self.service.audience_of(ctx).await })
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
    // Barrier 3 of ADR-083 at first sign-in: a portal user holding a role a portal user may not
    // hold is not activated. The user stays `invited`, the transaction writes nothing.
    let portal = data
        .fields
        .get(&name("user_type")?)
        .and_then(crate::services::scim_store::text)
        .is_some_and(|kind| kind == "portal");
    if portal && let Err(refusal) = check_portal_assignments(&mut tx, user).await {
        let _ = tx.rollback().await;
        return Err(refusal);
    }
    data.fields
        .insert(name("state")?, Value::Text("active".into()));
    tx.update(&data, loaded.version).await.map_err(error)?;
    tx.commit().await.map_err(error)
}

/// The prefix of the error `activate_user` returns when the outside-role check refuses, so the
/// caller can answer a refusal and not a failure.
const PORTAL_ROLE_REFUSED: &str = "first_sign_in.portal_role_refused";

/// Checks every active role assignment of the portal user `user` against the portal allow-list
/// (`spot_iam_roles::OutsideGuard`). The role records carry no kind and no duties in storage, so
/// the guard is built blind (`from_roles`): every stored role counts as a desk role, a role that
/// cannot be found counts as unknown, and both are refused (closed). A portal user with no
/// assignment passes.
///
/// # Errors
///
/// A refusal (`first_sign_in.portal_role_refused`, never naming the role) or a read that failed.
async fn check_portal_assignments<T: Transaction>(tx: &mut T, user: RecordId) -> Result<()> {
    use crate::services::scim_store::{idv, rows, text};
    let unreadable = |_| Error("the role assignments cannot be read".into());
    let roles = rows(tx, doctype::ROLE, &["id", "role_name"])
        .await
        .map_err(unreadable)?;
    let assignments = rows(tx, doctype::ROLE_ASSIGNMENT, &["user", "role", "state"])
        .await
        .map_err(unreadable)?;
    let names: BTreeMap<RecordId, String> = roles
        .iter()
        .filter_map(|r| Some((idv(r.first()?)?, text(r.get(1)?)?)))
        .collect();
    let stored: Vec<spot_iam_roles::Role> = names
        .values()
        .map(|n| spot_iam_roles::Role::custom(n))
        .collect();
    let id = user.to_string();
    let guard = spot_iam_roles::OutsideGuard::from_roles(&stored, [id.clone()]);
    for row in &assignments {
        let mine = row.first().and_then(idv) == Some(user);
        let active = row.get(2).and_then(text).as_deref() == Some("active");
        if !(mine && active) {
            continue;
        }
        let allowed = row
            .get(1)
            .and_then(idv)
            .and_then(|role| names.get(&role))
            .is_some_and(|role| {
                guard
                    .check(&id, &spot_iam_roles::RoleName::from(role.as_str()))
                    .is_ok()
            });
        if !allowed {
            return Err(Error(format!(
                "{PORTAL_ROLE_REFUSED}: a portal user holds a role a portal user may not hold, so the account stays invited"
            )));
        }
    }
    Ok(())
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
