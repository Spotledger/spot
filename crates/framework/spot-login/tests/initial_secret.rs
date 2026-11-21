//! Recorded security finding (the token-shaped wrong password does the same hash work) and a recorded security finding (the user state
//! is checked before the one-time initial secret is spent).
//!
//! Every case runs on the in-memory storage backend.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::storage as st;
use common::{CountingHasher, MemEvents};
use spot_http::api::{ApiError, LoginBody, Session};
use spot_login::{LoginConfig, LoginService};
use spot_storage_contract::{StorageBackend, Value};
use time::OffsetDateTime;

const NOW: i64 = 60;
const PASSWORD: &str = "correct horse battery staple 7731";

fn at(unix: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(unix).expect("a valid instant")
}

fn service<B: StorageBackend>(rig: &st::Rig<B>, hasher: Arc<CountingHasher>) -> LoginService {
    let config = LoginConfig {
        cache_ttl: Duration::ZERO,
        ..LoginConfig::for_tests()
    };
    rig.service_with(config, Arc::new(MemEvents::default()), hasher)
}

async fn login(
    service: &LoginService,
    source: usize,
    user: &str,
    password: &str,
) -> Result<Session, ApiError> {
    service
        .login_with_source(
            &common::ctx(None),
            &LoginBody {
                user: user.to_owned(),
                password: password.to_owned(),
                totp: None,
                recovery_code: None,
            },
            &format!("10.2.0.{source}"),
        )
        .await
}

fn code(result: &Result<Session, ApiError>) -> String {
    match result {
        Ok(_) => "ok".to_owned(),
        Err(error) => error.code.as_str().to_owned(),
    }
}

// Recorded security finding: a disabled, anonymised or invited user is refused without spending the secret, so the
// secret still works once the account may sign in.
async fn case_refusal_leaves_the_secret_unspent<B: StorageBackend>(rig: st::Rig<B>, state: &str) {
    let hasher = Arc::new(CountingHasher::new());
    let service = service(&rig, hasher);
    let id = rig.user("ada", state).await;
    let secret = st::OpaqueToken::generate();
    rig.stores
        .key_store()
        .set_initial_password(id, &secret, Duration::from_secs(600), NOW)
        .await
        .expect("issued");

    let refused = login(&service, 1, "ada", secret.as_str()).await;
    assert_eq!(code(&refused), "auth.invalid_credentials", "{state}");

    rig.poke(st::USER, id, "state", Value::Text("active".to_owned()))
        .await;
    let session = login(&service, 2, "ada", secret.as_str()).await;
    assert!(
        session.is_ok(),
        "{state}: the refused attempt left the secret unspent: {:?}",
        code(&session)
    );
    let again = login(&service, 3, "ada", secret.as_str()).await;
    assert_eq!(code(&again), "auth.invalid_credentials", "now single use");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_disabled_user_does_not_burn_the_initial_secret() {
    case_refusal_leaves_the_secret_unspent(st::fake_at(at(NOW)).await, "disabled").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_invited_user_does_not_burn_the_initial_secret() {
    case_refusal_leaves_the_secret_unspent(st::fake_at(at(NOW)).await, "invited").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_anonymised_user_does_not_burn_the_initial_secret() {
    case_refusal_leaves_the_secret_unspent(st::fake_at(at(NOW)).await, "anonymised").await;
}

// Recorded security finding: a token-shaped wrong password and an ordinary wrong password cost the same number of
// password-hash operations (counted, not timed), for a user with and without a password.
async fn case_equal_hash_work<B: StorageBackend>(rig: st::Rig<B>) {
    let hasher = Arc::new(CountingHasher::new());
    let service = service(&rig, hasher.clone());
    let with = rig.user("ada", "active").await;
    rig.set_password(with, PASSWORD).await;
    rig.user("grace", "active").await; // no password: the dummy hash is verified instead
    let token_shaped = st::OpaqueToken::generate();

    for (source, name) in [(10, "ada"), (20, "grace")] {
        let before = hasher.verify_count();
        let ordinary = login(&service, source, name, "not the password").await;
        let ordinary_cost = hasher.verify_count() - before;
        let before = hasher.verify_count();
        let shaped = login(&service, source + 1, name, token_shaped.as_str()).await;
        let shaped_cost = hasher.verify_count() - before;
        assert_eq!(code(&ordinary), "auth.invalid_credentials");
        assert_eq!(code(&shaped), "auth.invalid_credentials");
        assert_eq!(
            ordinary_cost, 1,
            "{name}: an ordinary wrong password does one hash"
        );
        assert_eq!(
            shaped_cost, ordinary_cost,
            "{name}: a token-shaped one does the same"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_token_shaped_wrong_password_does_the_same_hash_work() {
    case_equal_hash_work(st::fake_at(at(NOW)).await).await;
}
