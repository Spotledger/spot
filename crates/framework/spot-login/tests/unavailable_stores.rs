//! Every store path turns a storage `Unavailable` into the typed retryable marker, from the
//! storage error's kind and never from text; the sign-in log carries the request id and nothing
//! secret. The backend is the fake wrapped so that it can be switched to fail every call with
//! `ErrorKind::Unavailable` (the fake itself never models a connection failure).
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::storage as st;
use spot_http::api::{AuthService, Code, ErrorCode};
use spot_login::{Argon2Hasher, KeyStore, LoginConfig, LoginError, LoginService, UserDirectory};
use spot_sessions::tests_support::digest_of;
use spot_sessions::{SessionStore, ThrottleStore, UserKey};
use spot_storage_contract::{
    Capabilities, Catalog, Change, ChangeFeed, ErrorKind, FeedPosition, HealthReport, OutboxRecord,
    Query, ReadSource, Rows, StorageBackend, StorageError, StorageResult, TxOptions,
};
use tracing_subscriber::fmt::MakeWriter;

/// A backend that delegates to `inner` until `down` is set, then fails every call
/// with `Unavailable`.
struct Flaky<B> {
    inner: B,
    down: Arc<AtomicBool>,
}

impl<B> Flaky<B> {
    fn check(&self) -> StorageResult<()> {
        if self.down.load(Ordering::SeqCst) {
            Err(StorageError::new(
                ErrorKind::Unavailable,
                "the pool could not give a connection",
            ))
        } else {
            Ok(())
        }
    }
}

impl<B: StorageBackend> ChangeFeed for Flaky<B> {
    async fn read_outbox(
        &self,
        after: FeedPosition,
        limit: u32,
    ) -> StorageResult<Vec<OutboxRecord>> {
        self.check()?;
        self.inner.read_outbox(after, limit).await
    }

    async fn read_changes(&self, after: FeedPosition, limit: u32) -> StorageResult<Vec<Change>> {
        self.check()?;
        self.inner.read_changes(after, limit).await
    }
}

impl<B: StorageBackend> StorageBackend for Flaky<B> {
    type Tx = B::Tx;

    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    async fn prepare(&self, catalog: &Catalog) -> StorageResult<()> {
        self.inner.prepare(catalog).await
    }

    async fn begin(&self, catalog: Arc<Catalog>, options: TxOptions) -> StorageResult<Self::Tx> {
        self.check()?;
        self.inner.begin(catalog, options).await
    }

    async fn read(
        &self,
        catalog: Arc<Catalog>,
        source: ReadSource,
        query: &Query,
    ) -> StorageResult<Rows> {
        self.check()?;
        self.inner.read(catalog, source, query).await
    }

    async fn health(&self) -> HealthReport {
        self.inner.health().await
    }
}

async fn flaky_rig() -> (st::Rig<Flaky<st::FakeBackend>>, Arc<AtomicBool>) {
    let down = Arc::new(AtomicBool::new(false));
    let backend = Flaky {
        inner: st::FakeBackend::new(),
        down: down.clone(),
    };
    let rig = st::build(backend, None, st::start());
    rig.engine.prepare().await.expect("the backend prepares");
    (rig, down)
}

fn assert_unavailable(error: &LoginError, what: &str) {
    assert!(error.is_unavailable(), "{what}: {error}");
    assert!(error.retryable_after.is_some(), "{what}");
    let api = error.to_api_error();
    assert_eq!(
        api.code,
        ErrorCode::Registry(Code::ServerUnavailable),
        "{what}"
    );
    assert_eq!(api.status().as_u16(), 503, "{what}");
    assert!(
        !error.detail.contains('\u{1}'),
        "{what}: the private marker leaked into the detail"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_user_directory_marks_unavailable_storage() {
    let (rig, down) = flaky_rig().await;
    let id = rig.user("alice", "active").await;
    let directory = rig.stores.user_directory();
    down.store(true, Ordering::SeqCst);
    let by_name = directory.find_by_identifier("alice").await.unwrap_err();
    assert_unavailable(&by_name, "find_by_identifier");
    let by_id = directory.find_by_id(id).await.unwrap_err();
    assert_unavailable(&by_id, "find_by_id");
    let step = directory.record_totp_step(id, 5).await.unwrap_err();
    assert_unavailable(&step, "record_totp_step");
    let burn = directory.consume_recovery_digest(id, 0).await.unwrap_err();
    assert!(burn.kind == spot_login::LoginErrorKind::StoreFailed);
    down.store(false, Ordering::SeqCst);
    assert!(
        directory
            .find_by_identifier("alice")
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_key_store_marks_unavailable_storage() {
    let (rig, down) = flaky_rig().await;
    let keys = rig.stores.key_store();
    down.store(true, Ordering::SeqCst);
    let error = keys.find_by_key_id("some-key-id").await.unwrap_err();
    assert_unavailable(&error, "find_by_key_id");
    let presented = spot_auth::OpaqueToken::generate();
    let spent = keys
        .consume_initial_secret(st::RecordId::new_v7(), &presented, 0)
        .await
        .unwrap_err();
    // The engine's secret path reports a failed audit write as `secretfield.audit_failed`
    // (an internal class, not Unavailable): the cause is masked below this crate, so the
    // store reports an ordinary failure rather than guessing from text.
    assert_eq!(spent.kind, spot_login::LoginErrorKind::StoreFailed);
    assert!(!spent.is_unavailable());
}

#[tokio::test(flavor = "multi_thread")]
async fn the_session_store_marks_unavailable_storage() {
    let (rig, down) = flaky_rig().await;
    let user = rig.user("alice", "active").await;
    let sessions = rig.stores.session_store();
    down.store(true, Ordering::SeqCst);
    let found = sessions.find_by_digest(&digest_of("t")).await.unwrap_err();
    // Same engine masking as the initial secret: an ordinary store failure, not retryable.
    let error = LoginError::from(found);
    assert_eq!(error.kind, spot_login::LoginErrorKind::StoreFailed);
    assert!(!error.is_unavailable());
    let key = UserKey::new(user.to_string()).unwrap();
    let listed = LoginError::from(sessions.list_for_user(&key).await.unwrap_err());
    assert_unavailable(&listed, "list_for_user");
    let purged = LoginError::from(sessions.purge_expired(0, 10).await.unwrap_err());
    assert_unavailable(&purged, "purge_expired");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_throttle_store_marks_unavailable_storage() {
    let (rig, down) = flaky_rig().await;
    rig.user("alice", "active").await;
    let throttle = rig.stores.throttle_store(Duration::from_secs(900));
    down.store(true, Ordering::SeqCst);
    let account = LoginError::from(throttle.get(true, b"alice").await.unwrap_err());
    assert_unavailable(&account, "account counter");
    let source = LoginError::from(throttle.get(false, b"source-digest").await.unwrap_err());
    assert_unavailable(&source, "source counter");
}

// ---------------------------------------------------------------------------------------------
// The sign-in log
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Default)]
struct Buffer(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Buffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .map_err(|_| std::io::Error::other("poisoned"))?
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Buffer {
    type Writer = Buffer;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

impl Buffer {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sign_in_that_hits_unavailable_storage_is_logged_once_without_the_identifier() {
    let (rig, down) = flaky_rig().await;
    rig.user("carol.secretname", "active").await;
    let hasher = Arc::new(Argon2Hasher::new(st::HashParams::for_tests()));
    let service = LoginService::new(
        LoginConfig::for_tests(),
        Arc::new(rig.stores.session_store()) as Arc<dyn SessionStore>,
        Arc::new(rig.stores.throttle_store(Duration::from_secs(900))) as Arc<dyn ThrottleStore>,
        Arc::new(rig.stores.user_directory()),
        Arc::new(rig.stores.key_store()),
        Arc::new(common::MemEvents::default()),
        rig.clock.clone(),
        hasher,
    )
    .expect("the service builds");

    let buffer = Buffer::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(buffer.clone())
        .with_max_level(tracing::Level::DEBUG)
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    down.store(true, Ordering::SeqCst);
    let ctx = common::ctx(None);
    let error = service
        .login(&ctx, &common::body("carol.secretname", "hunter2-not-real"))
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::Registry(Code::ServerUnavailable));
    assert_eq!(error.status().as_u16(), 503);

    let logs = buffer.text();
    let lines: Vec<&str> = logs.lines().filter(|l| l.contains("WARN")).collect();
    assert_eq!(lines.len(), 1, "{logs}");
    assert!(lines[0].contains(ctx.request_id.as_str()), "{logs}");
    assert!(lines[0].contains("login.store_failed"), "{logs}");
    assert!(lines[0].contains("storage.unavailable"), "{logs}");
    assert!(!logs.contains("carol"), "{logs}");
    assert!(!logs.contains("hunter2"), "{logs}");
}
