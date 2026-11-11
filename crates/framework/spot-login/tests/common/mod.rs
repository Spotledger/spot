//! Shared fixtures for the sign-in tests: in-memory ports and the test rig.
#![allow(
    dead_code,
    unused_imports,
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used
)]
//! End-to-end tests of the sign-in service over in-memory ports and the `spot-sessions`
//! in-memory stores: the sign-in decision table, throttle and lockout, the second factor,
//! password change and session management, the authenticator and its cache, and redaction.

pub mod storage;

pub use std::collections::HashMap;
pub use std::sync::atomic::{AtomicUsize, Ordering};
pub use std::sync::{Arc, Mutex, MutexGuard};
pub use std::time::Duration;

pub use spot_auth::{
    ApiKey, HashParams, Password, PasswordHash, StoredDigest, TotpSecret, generate_recovery_codes,
};
pub use spot_core::{ManualClock, SiteId};
pub use spot_http::api::{AuthService, Credential, LoginBody, RequestCtx};
pub use spot_http::request::RequestId;
pub use spot_login::{
    Argon2Hasher, BoxFuture, KeyRecord, KeyState, KeyStore, LoginConfig, LoginError,
    LoginErrorKind, LoginService, PasswordHasher, UserDirectory, UserRecord, UserState,
};
pub use spot_sessions::throttle::InMemoryThrottleStore;
pub use spot_sessions::{InMemorySessionStore, SessionPolicy, ThrottleConfig, ThrottleStore};
pub use spot_storage_contract::RecordId;
pub use time::OffsetDateTime;

/// Unwraps a `Result` in a test, panicking with the error (the workspace denies `unwrap`).
pub fn ok<T, E: std::fmt::Debug>(result: Result<T, E>) -> T {
    match result {
        Ok(value) => value,
        Err(error) => panic!("unexpected error: {error:?}"),
    }
}

/// Unwraps the error side of a `Result` in a test.
pub fn err<T, E>(result: Result<T, E>) -> E {
    match result {
        Ok(_) => panic!("expected an error"),
        Err(error) => error,
    }
}

/// Unwraps an `Option` in a test.
pub fn some<T>(value: Option<T>) -> T {
    match value {
        Some(value) => value,
        None => panic!("unexpected None"),
    }
}

pub fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub fn site() -> SiteId {
    ok(SiteId::new("acme.test"))
}

pub fn ctx(token: Option<&str>) -> RequestCtx {
    RequestCtx {
        site: site(),
        credential: token.map(Credential::new),
        idempotency_key: None,
        language: None,
        request_id: RequestId::generate(),
        peer: None,
    }
}

pub fn body(user: &str, password: &str) -> LoginBody {
    LoginBody {
        user: user.to_owned(),
        password: password.to_owned(),
        totp: None,
        recovery_code: None,
    }
}

/// A user as the in-memory directory stores it: credentials as text/bytes, the way a real store
/// would hold them.
#[derive(Clone)]
pub struct StoredUser {
    pub id: RecordId,
    pub user_name: String,
    pub email: Option<String>,
    pub full_name: String,
    pub state: UserState,
    pub password_hash: Option<String>,
    pub totp_base32: Option<String>,
    pub recovery: Vec<[u8; 32]>,
    pub totp_last_step: Option<u64>,
}

impl StoredUser {
    pub fn new(user_name: &str, state: UserState) -> Self {
        Self {
            id: RecordId::new_v7(),
            user_name: user_name.to_owned(),
            email: None,
            full_name: user_name.to_owned(),
            state,
            password_hash: None,
            totp_base32: None,
            recovery: Vec::new(),
            totp_last_step: None,
        }
    }

    pub fn with_password(mut self, hasher: &dyn PasswordHasher, password: &str) -> Self {
        self.password_hash = Some(
            hasher
                .hash(&Password::new(password))
                .phc_string()
                .to_owned(),
        );
        self
    }

    pub fn rebuild(&self) -> Result<UserRecord, LoginError> {
        let password_hash = match &self.password_hash {
            Some(phc) => Some(PasswordHash::parse(phc).map_err(|error| {
                LoginError::new(LoginErrorKind::StoreFailed, error.to_string())
            })?),
            None => None,
        };
        let totp_secret = match &self.totp_base32 {
            Some(text) => Some(TotpSecret::from_base32(text).map_err(|error| {
                LoginError::new(LoginErrorKind::StoreFailed, error.to_string())
            })?),
            None => None,
        };
        let recovery_digests = self
            .recovery
            .iter()
            .map(|bytes| StoredDigest::from_bytes(bytes))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| LoginError::new(LoginErrorKind::StoreFailed, error.to_string()))?;
        Ok(UserRecord {
            id: self.id,
            user_name: self.user_name.clone(),
            email: self.email.clone(),
            full_name: self.full_name.clone(),
            state: self.state,
            password_hash,
            totp_secret,
            recovery_digests,
            totp_last_step: self.totp_last_step,
        })
    }
}

#[derive(Default)]
pub struct MemDirectory {
    pub users: Mutex<HashMap<RecordId, StoredUser>>,
}

impl MemDirectory {
    pub fn add(&self, user: StoredUser) -> RecordId {
        let id = user.id;
        lock(&self.users).insert(id, user);
        id
    }

    pub fn mutate(&self, id: RecordId, change: impl FnOnce(&mut StoredUser)) {
        if let Some(user) = lock(&self.users).get_mut(&id) {
            change(user);
        }
    }
}

impl UserDirectory for MemDirectory {
    fn find_by_identifier<'a>(
        &'a self,
        identifier: &'a str,
    ) -> BoxFuture<'a, Result<Option<UserRecord>, LoginError>> {
        Box::pin(async move {
            let wanted = identifier.to_lowercase();
            let found = lock(&self.users)
                .values()
                .find(|user| {
                    user.user_name.to_lowercase() == wanted
                        || user.email.as_deref().map(str::to_lowercase).as_deref()
                            == Some(wanted.as_str())
                })
                .cloned();
            found.map(|user| user.rebuild()).transpose()
        })
    }

    fn find_by_id<'a>(
        &'a self,
        id: RecordId,
    ) -> BoxFuture<'a, Result<Option<UserRecord>, LoginError>> {
        Box::pin(async move {
            let found = lock(&self.users).get(&id).cloned();
            found.map(|user| user.rebuild()).transpose()
        })
    }

    fn set_password_hash<'a>(
        &'a self,
        id: RecordId,
        hash: PasswordHash,
    ) -> BoxFuture<'a, Result<(), LoginError>> {
        Box::pin(async move {
            self.mutate(id, |user| {
                user.password_hash = Some(hash.phc_string().to_owned());
            });
            Ok(())
        })
    }

    fn record_totp_step<'a>(
        &'a self,
        id: RecordId,
        step: u64,
    ) -> BoxFuture<'a, Result<(), LoginError>> {
        Box::pin(async move {
            self.mutate(id, |user| user.totp_last_step = Some(step));
            Ok(())
        })
    }

    fn consume_recovery_digest<'a>(
        &'a self,
        id: RecordId,
        index: usize,
    ) -> BoxFuture<'a, Result<(), LoginError>> {
        Box::pin(async move {
            self.mutate(id, |user| {
                if index < user.recovery.len() {
                    user.recovery.remove(index);
                }
            });
            Ok(())
        })
    }

    fn set_second_factor<'a>(
        &'a self,
        id: RecordId,
        totp: Option<TotpSecret>,
        recovery: Vec<StoredDigest>,
    ) -> BoxFuture<'a, Result<(), LoginError>> {
        Box::pin(async move {
            self.mutate(id, |user| {
                user.totp_base32 = totp.map(|secret| secret.to_base32());
                user.recovery = recovery.iter().map(|digest| *digest.as_bytes()).collect();
            });
            Ok(())
        })
    }
}

#[derive(Clone)]
pub struct StoredKey {
    pub key_id: String,
    pub digest: [u8; 32],
    pub owner: RecordId,
    pub state: KeyState,
    pub expires_at: Option<OffsetDateTime>,
}

#[derive(Default)]
pub struct MemKeyStore {
    pub keys: Mutex<HashMap<String, StoredKey>>,
}

impl MemKeyStore {
    pub fn add(&self, key: StoredKey) {
        lock(&self.keys).insert(key.key_id.clone(), key);
    }
}

impl KeyStore for MemKeyStore {
    fn find_by_key_id<'a>(
        &'a self,
        key_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<KeyRecord>, LoginError>> {
        Box::pin(async move {
            let found = lock(&self.keys).get(key_id).cloned();
            found
                .map(|stored| {
                    Ok(KeyRecord {
                        key_id: stored.key_id,
                        digest: spot_auth::TokenDigest::from_bytes(&stored.digest).map_err(
                            |error| LoginError::new(LoginErrorKind::StoreFailed, error.to_string()),
                        )?,
                        owner: stored.owner,
                        state: stored.state,
                        expires_at: stored.expires_at,
                    })
                })
                .transpose()
        })
    }
}

#[derive(Default)]
pub struct MemEvents {
    pub events: Mutex<Vec<spot_login::IdentityEvent>>,
    pub attempts: Mutex<Vec<spot_login::LoginAttempt>>,
}

impl spot_login::IdentityEvents for MemEvents {
    fn emit<'a>(
        &'a self,
        event: spot_login::IdentityEvent,
    ) -> BoxFuture<'a, Result<(), LoginError>> {
        Box::pin(async move {
            lock(&self.events).push(event);
            Ok(())
        })
    }

    fn record_login_attempt<'a>(
        &'a self,
        attempt: spot_login::LoginAttempt,
    ) -> BoxFuture<'a, Result<(), LoginError>> {
        Box::pin(async move {
            lock(&self.attempts).push(attempt);
            Ok(())
        })
    }
}

/// A hasher that counts verification calls, so a test can prove equal work.
pub struct CountingHasher {
    pub inner: Argon2Hasher,
    pub verifies: AtomicUsize,
}

impl CountingHasher {
    pub fn new() -> Self {
        Self {
            inner: Argon2Hasher::new(HashParams::for_tests()),
            verifies: AtomicUsize::new(0),
        }
    }

    pub fn verify_count(&self) -> usize {
        self.verifies.load(Ordering::SeqCst)
    }
}

impl PasswordHasher for CountingHasher {
    fn hash(&self, password: &Password) -> PasswordHash {
        self.inner.hash(password)
    }

    fn verify(&self, password: &Password, stored: &PasswordHash) -> bool {
        self.verifies.fetch_add(1, Ordering::SeqCst);
        self.inner.verify(password, stored)
    }
}

pub struct Fixture {
    pub service: Arc<LoginService>,
    pub dir: Arc<MemDirectory>,
    pub keys: Arc<MemKeyStore>,
    pub events: Arc<MemEvents>,
    pub clock: Arc<ManualClock>,
    pub hasher: Arc<CountingHasher>,
}

pub fn fixture_at(clock: ManualClock, config: LoginConfig) -> Fixture {
    let dir = Arc::new(MemDirectory::default());
    let keys = Arc::new(MemKeyStore::default());
    let events = Arc::new(MemEvents::default());
    let hasher = Arc::new(CountingHasher::new());
    let clock = Arc::new(clock);
    let store: Arc<dyn spot_sessions::SessionStore> = Arc::new(InMemorySessionStore::new());
    let throttle: Arc<dyn ThrottleStore> = Arc::new(InMemoryThrottleStore::new());
    let service = Arc::new(ok(LoginService::new(
        config,
        store,
        throttle,
        dir.clone(),
        keys.clone(),
        events.clone(),
        clock.clone(),
        hasher.clone(),
    )));
    Fixture {
        service,
        dir,
        keys,
        events,
        clock,
        hasher,
    }
}

pub fn fixture() -> Fixture {
    fixture_at(
        ManualClock::new(ok(OffsetDateTime::from_unix_timestamp(1_000_000))),
        LoginConfig::for_tests(),
    )
}

pub fn assert_code(error: &spot_http::api::ApiError, code: &str) {
    assert_eq!(error.code.as_str(), code, "unexpected error: {error:?}");
}

pub fn throttle_config(backoff_after: u32, lockout_after: u32) -> ThrottleConfig {
    ThrottleConfig {
        backoff_after,
        lockout_after,
        window: Duration::from_secs(900),
        backoff_base: Duration::from_secs(1),
        backoff_multiplier: 2,
        backoff_cap: Duration::from_secs(900),
        lockout_duration: Duration::from_secs(1800),
    }
}
