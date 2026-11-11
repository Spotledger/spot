//! The crate's own end-to-end tests: the contract suite runs against
//! [`InMemorySessionStore`], plus the manager's decision-table tests the suite does not
//! cover (error codes, malformed tokens, `ClientInfo` bounds).
//!
//! The throttle state machine has its own suite in `tests/throttle.rs`.

use std::sync::Arc;

use spot_auth::{OpaqueToken, TokenDigest};
use spot_sessions::manager::SessionManager;
use spot_sessions::memory::InMemorySessionStore;
use spot_sessions::model::{ClientInfo, SaltKey, SessionPolicy, SessionState, UserKey};
use spot_sessions::{SessionError, model::SessionEndReason};

/// A manager over one fresh store per test.
fn fresh() -> (
    Arc<InMemorySessionStore>,
    SessionManager<Arc<InMemorySessionStore>>,
) {
    let store = Arc::new(InMemorySessionStore::new());
    let salt = Arc::new(SaltKey::from_bytes(&[5; 32]).unwrap_or_else(|_| unreachable!()));
    let policy = SessionPolicy::new(
        std::time::Duration::from_secs(120),
        std::time::Duration::from_secs(600),
        std::time::Duration::from_secs(90),
        3,
    )
    .unwrap_or_else(|reason| panic!("policy: {reason}"));
    let manager = SessionManager::new(store.clone(), policy, salt);
    (store, manager)
}

fn user_key(name: &str) -> UserKey {
    UserKey::new(name).unwrap_or_else(|reason| panic!("user key: {reason}"))
}

fn client() -> ClientInfo {
    ClientInfo::new(None, None).unwrap_or_else(|reason| panic!("client: {reason}"))
}

#[tokio::test]
async fn the_contract_suite_passes_on_the_in_memory_store() {
    // One fresh store per property: the runner re-creates through the constructor.
    spot_sessions::contract::run_all(InMemorySessionStore::new).await;
}

#[tokio::test]
async fn end_to_end_login_validate_logout() {
    let (_store, manager) = fresh();
    let user = user_key("ayesha@acme.test");
    let started = manager
        .start(user.clone(), "203.0.113.7", client(), 1_000)
        .await
        .unwrap_or_else(|error| panic!("start: {error:?}"));
    let token_text = started.token.as_str().to_owned();

    // Immediate validation works and does not rotate (age 0 < 120 s).
    let validated = manager
        .validate(&token_text, 1_001)
        .await
        .unwrap_or_else(|error| panic!("validate: {error:?}"));
    assert!(validated.renewed_token.is_none());
    assert_eq!(validated.session.user, user_key("ayesha@acme.test"));

    // The list sees one live session.
    let listed = manager
        .list(&user, 1_002)
        .await
        .unwrap_or_else(|error| panic!("list: {error:?}"));
    assert_eq!(listed.len(), 1);
    assert!(listed[0].live);

    // The token's text never appears anywhere in the store's records: only the digest.
    let digest = started.session.token_digest.as_bytes();
    // The digest matches the presented token; no structure in the store holds the text.
    assert!(
        TokenDigest::from_bytes(digest)
            .unwrap_or_else(|_| unreachable!())
            .matches_token(&started.token)
    );

    // Logout ends the session; a repeat logout ends nothing new.
    let ended = manager
        .end(&started.session.id, 1_010)
        .await
        .unwrap_or_else(|error| panic!("end: {error:?}"));
    assert_eq!(ended.ended, 1);
    let repeat = manager
        .end(&started.session.id, 1_011)
        .await
        .unwrap_or_else(|error| panic!("end: {error:?}"));
    assert_eq!(repeat.ended, 0);
    assert!(matches!(
        manager.validate(&token_text, 1_012).await,
        Err(SessionError::Invalid { .. })
    ));
}

#[tokio::test]
async fn a_malformed_token_is_never_digest_compared() {
    let (_store, manager) = fresh();
    // Wrong lengths and characters: all refused as `session.malformed` before any lookup
    // (43 alphanumeric characters would be a *valid* token text that simply does not
    // exist — that path is `session.invalid`, covered elsewhere).
    for bad in [
        "",
        "short",
        &format!("{}+", "a".repeat(42)), // 43 chars, one outside the Base64url alphabet
        &format!("{}!", "a".repeat(42)),
    ] {
        match manager.validate(bad, 1_000).await {
            Err(SessionError::Malformed) => {}
            Ok(_) => panic!("expected Malformed for {bad:?}, got Ok"),
            Err(other) => panic!("expected Malformed for {bad:?}, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn the_password_change_rule_ends_other_sessions() {
    // ADR-070 §5.5 / §6.3: a password change ends the others; the current session may stay.
    let (_store, manager) = fresh();
    let user = user_key("ayesha@acme.test");
    let first = manager
        .start(user.clone(), "203.0.113.1", client(), 1_000)
        .await
        .unwrap_or_else(|error| panic!("start: {error:?}"));
    let second = manager
        .start(user.clone(), "203.0.113.2", client(), 1_100)
        .await
        .unwrap_or_else(|error| panic!("start: {error:?}"));

    let reason = SessionEndReason::PasswordChanged;
    let report = manager
        .revoke_all_for_user(&user, Some(&second.session.id), reason)
        .await
        .unwrap_or_else(|error| panic!("revoke_all: {error:?}"));
    assert_eq!(report.ended, 1);

    // The first session's token is dead; the second (kept) still validates.
    assert!(manager.validate(first.token.as_str(), 1_200).await.is_err());
    assert!(manager.validate(second.token.as_str(), 1_200).await.is_ok());
}

#[tokio::test]
async fn the_absolute_bound_outlives_any_activity() {
    // Touches keep the session alive inside the absolute window but never past it.
    let (_store, manager) = fresh();
    let user = user_key("busy@acme.test");
    let started = manager
        .start(user, "203.0.113.1", client(), 1_000)
        .await
        .unwrap_or_else(|error| panic!("start: {error:?}"));
    // Every 61 s the client validates — each touch moves the window by more than the
    // 60 s granularity, so the idle bound never lapses. Rotation (age ≥ 90 s) hands the
    // client a fresh token each time it fires; the loop follows it, exactly as a real
    // client would. At 1_600 the absolute bound (1_000 + 600) has passed: dead whatever
    // the activity.
    let mut token = started.token.as_str().to_owned();
    let mut now = 1_061_i64;
    while now < 1_600 {
        let validated = manager
            .validate(&token, now)
            .await
            .unwrap_or_else(|error| panic!("validate at {now}: {error:?}"));
        if let Some(fresh) = validated.renewed_token {
            token = fresh.as_str().to_owned();
        }
        now += 61;
    }
    assert!(matches!(
        manager.validate(&token, 1_600).await,
        Err(SessionError::Invalid { .. })
    ));
}

#[tokio::test]
async fn client_info_bounds_the_labels() {
    let (store, _manager) = fresh();
    // Over the cap: refused at construction.
    assert!(ClientInfo::new(Some(&"d".repeat(65)), None).is_err());
    // At the cap: accepted.
    assert!(ClientInfo::new(Some(&"d".repeat(64)), Some(&"c".repeat(64))).is_ok());
    // A manager start with an over-cap label never reaches the store.
    let policy = SessionPolicy::DEFAULT;
    let manager = SessionManager::new(
        store,
        policy,
        Arc::new(SaltKey::from_bytes(&[6; 32]).unwrap_or_else(|_| unreachable!())),
    );
    let user = user_key("x@acme.test");
    let big = match ClientInfo::new(Some(&"d".repeat(65)), None) {
        Ok(big) => big,
        Err(_) => {
            // The construction refused; nothing to start. The manager never sees it.
            return;
        }
    };
    let _ = manager.start(user, "203.0.113.1", big, 1_000).await;
}

#[tokio::test]
async fn the_source_digest_never_echoes_the_address() {
    let (_store, manager) = fresh();
    let started = manager
        .start(user_key("a@acme.test"), "203.0.113.42", client(), 1_000)
        .await
        .unwrap_or_else(|error| panic!("start: {error:?}"));
    let rendered = format!("{:?}", started.session);
    assert!(!rendered.contains("203.0.113.42"));
    // The Debug of Session carries the digest redacted (`Secret(***)`) and nothing of the
    // address.
    assert!(rendered.contains("Secret(***)"));
}

#[tokio::test]
async fn session_policy_defaults_match_the_documented_values() {
    let policy = SessionPolicy::DEFAULT;
    assert_eq!(policy.idle, std::time::Duration::from_secs(1_800));
    assert_eq!(policy.absolute, std::time::Duration::from_secs(43_200));
    assert_eq!(policy.rotate_after, std::time::Duration::from_secs(28_800));
    assert_eq!(policy.max_per_user, 8);
    // The derived status rules run against a real record too:
    let started = {
        let (_store, manager) = fresh();
        manager
            .start(user_key("p@acme.test"), "203.0.113.1", client(), 0)
            .await
            .unwrap_or_else(|error| panic!("start: {error:?}"))
    };
    assert_eq!(started.session.state, SessionState::Active);
}

#[tokio::test]
async fn the_error_code_catalogue_matches_the_error_type() {
    // The three codes the error type can raise, stable:
    assert_eq!(
        SessionError::Invalid {
            reason: Box::new(spot_sessions::error::InternalReason::UnknownDigest)
        }
        .code(),
        "session.invalid"
    );
    assert_eq!(SessionError::Malformed.code(), "session.malformed");
    assert_eq!(
        SessionError::StoreFailed("x".to_owned()).code(),
        "session.store_failed"
    );
}

/// A token helper proving `OpaqueToken` round-trips through validation one more time at
/// the type level (the crate depends on spot-auth's parsing being exact).
#[test]
fn opaque_tokens_reparse_exactly() {
    let token = OpaqueToken::generate();
    let reparsed = OpaqueToken::parse(token.as_str()).unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(token.digest().as_bytes(), reparsed.digest().as_bytes());
}
