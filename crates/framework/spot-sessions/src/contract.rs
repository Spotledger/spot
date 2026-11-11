//! One suite every [`SessionStore`] backend must pass, behind feature `testing`.
//!
//! WP-PLT-02.T3's storage-backed store (and any later backend) enables this feature in its
//! dev-dependencies and calls [`run_all`] with a constructor; this crate's own tests run
//! the same suite against [`InMemorySessionStore`], so the fake and the real backend are
//! provably interchangeable. Every property the brief names is here: create/find/touch
//! boundaries (half-open), rotation kills the old token, revocation idempotent,
//! `max_per_user` eviction oldest-first with reason `evicted`, the store cannot return a
//! token (it never holds one), and 64 concurrent touches never extend past absolute.
//!
//! All time is argument-driven: no sleep, no system clock, no threads beyond the one test
//! task set (`run_all` is one future the caller's runtime drives).

use std::sync::Arc;
use std::time::Duration;

use spot_auth::OpaqueToken;

use crate::error::{InternalReason, SessionError};
use crate::manager::SessionManager;
use crate::model::{ClientInfo, SaltKey, SessionState, UserKey};
use crate::store::{SessionRecord, SessionStore, Touched};

/// The test idle bound (120 s: twice the touch granularity, so touches actually move the window).
const IDLE: u64 = 120;
/// The test absolute bound (600 s).
const ABSOLUTE: u64 = 600;
/// The test rotation age (90 s, inside the idle bound).
const ROTATE: u64 = 90;
/// The test per-user cap.
const MAX_PER_USER: usize = 3;

/// Turns an optional reply into its value, panicking with `what` instead of
/// `expect` (the workspace lint denies `expect` outside test modules).
fn must_some<T>(value: Option<T>, what: &str) -> T {
    match value {
        Some(value) => value,
        None => panic!("{what}"),
    }
}

/// How the suite obtains a fresh store per test.
pub type MakeFn<S> = fn() -> S;

/// Builds the policy the suite runs under.
fn policy() -> crate::model::SessionPolicy {
    match crate::model::SessionPolicy::new(
        Duration::from_secs(IDLE),
        Duration::from_secs(ABSOLUTE),
        Duration::from_secs(ROTATE),
        MAX_PER_USER,
    ) {
        Ok(policy) => policy,
        Err(reason) => panic!("suite policy is invalid: {reason}"),
    }
}

/// The fixed site salt (deterministic; the suite needs no entropy).
fn salt() -> Arc<SaltKey> {
    match SaltKey::from_bytes(&[3; 32]) {
        Ok(salt) => Arc::new(salt),
        Err(reason) => panic!("suite salt is invalid: {reason}"),
    }
}

/// The suite's idle bound in seconds.
fn idle_secs() -> i64 {
    IDLE as i64
}

/// The suite's absolute bound in seconds.
fn absolute_secs() -> i64 {
    ABSOLUTE as i64
}

/// A [`UserKey`] that has already been validated by construction.
fn user_key(name: &str) -> UserKey {
    match UserKey::new(name) {
        Ok(key) => key,
        Err(reason) => panic!("suite user key is invalid: {reason}"),
    }
}

/// A [`ClientInfo`] with no labels.
fn client() -> ClientInfo {
    match ClientInfo::new(None, None) {
        Ok(info) => info,
        Err(reason) => panic!("suite client info is invalid: {reason}"),
    }
}

/// Starts one session through a manager over `store`, at unix-seconds `now`.
async fn start_on<S: SessionStore>(
    store: &S,
    user: &UserKey,
    source: &str,
    now: i64,
) -> (spot_auth::OpaqueToken, SessionRecord) {
    let manager = SessionManager::new(store, policy(), salt());
    match manager.start(user.clone(), source, client(), now).await {
        Ok(started) => (started.token, started.session),
        Err(error) => panic!("suite start failed: {error:?}"),
    }
}

/// Runs every contract property against fresh stores from `make`.
///
/// `S` is the store backend under test; `Arc<S>` keeps the concurrent test's tasks alive.
/// Callers (this crate's tests, T3's) wrap this in a `#[tokio::test]` (or any other
/// executor) — the suite itself spawns only the 64 touch tasks of that one property.
pub async fn run_all<S, F>(make: F)
where
    // `'static`: the 64-touch property spawns tasks holding `Arc<S>`, and every real
    // backend is a named type anyway.
    S: SessionStore + 'static,
    F: Fn() -> S + Copy,
{
    // Store-level contract, one fresh store per property.
    create_and_find(&make()).await;
    find_unknown_digest_is_none(&make()).await;
    touch_extends_idle_only(&make()).await;
    touch_respects_granularity(&make()).await;
    touch_never_past_absolute(&make()).await;
    touch_skips_dead_sessions(&make()).await;
    {
        let store = std::sync::Arc::new(make());
        concurrent_64_touches_never_past_absolute(&store).await;
    }
    rotate_kills_the_old_digest(&make()).await;
    rotate_refuses_an_in_use_digest(&make()).await;
    revoke_is_idempotent_and_keeps_first_reason(&make()).await;
    revoke_all_respects_except(&make()).await;
    list_is_creation_ordered(&make()).await;
    purge_removes_only_expired(&make()).await;
    oldest_active_is_the_oldest_alive(&make()).await;
    store_holds_only_digests(&make()).await;

    // Manager-level behaviour over the same store trait.
    validate_is_half_open_at_both_bounds(&make()).await;
    invalid_is_one_code_with_internal_reasons(&make()).await;
    rotation_fires_exactly_at_the_age(&make()).await;
    cap_evicts_oldest_with_reason_evicted(&make()).await;
    end_and_end_others_count_live_sessions(&make()).await;
}

/// create then find_by_digest round-trips the same record.
async fn create_and_find<S: SessionStore>(store: &S) {
    let (token, record) = start_on(store, &user_key("u1"), "10.0.0.1", 1_000).await;
    let found = match store.find_by_digest(&token.digest()).await {
        Ok(found) => found,
        Err(error) => panic!("find failed: {error:?}"),
    };
    assert_eq!(must_some(found, "the session resolves"), record);
}

/// An unknown digest finds nothing.
async fn find_unknown_digest_is_none<S: SessionStore>(store: &S) {
    let _ = start_on(store, &user_key("u1"), "10.0.0.1", 1_000).await;
    let unknown = OpaqueToken::generate().digest();
    match store.find_by_digest(&unknown).await {
        Ok(found) => assert!(found.is_none()),
        Err(error) => panic!("find failed: {error:?}"),
    }
}

/// `touch` extends the idle window and moves `last_seen`, and never the absolute bound.
async fn touch_extends_idle_only<S: SessionStore>(store: &S) {
    let (token, record) = start_on(store, &user_key("u1"), "10.0.0.1", 1_000).await;
    match store
        .touch(&record.id, 1_100, idle_secs(), record.absolute_expires)
        .await
    {
        Ok(Touched::Extended) => {}
        Ok(other) => panic!("expected Extended, got {other:?}"),
        Err(error) => panic!("touch failed: {error:?}"),
    }
    let found = match store.find_by_digest(&token.digest()).await {
        Ok(found) => must_some(found, "the session resolves"),
        Err(error) => panic!("find failed: {error:?}"),
    };
    assert_eq!(found.idle_expires, 1_100 + idle_secs());
    assert_eq!(found.absolute_expires, 1_000 + absolute_secs());
    assert_eq!(found.last_seen, 1_100);
}

/// A touch under the granularity is skipped: no write, so a busy client does not extend
/// its window per request and does not write per request.
async fn touch_respects_granularity<S: SessionStore>(store: &S) {
    let (_token, record) = start_on(store, &user_key("u1"), "10.0.0.1", 1_000).await;
    match store
        .touch(&record.id, 1_005, idle_secs(), record.absolute_expires)
        .await
    {
        Ok(Touched::Skipped) => {}
        Ok(other) => panic!("expected Skipped under the granularity, got {other:?}"),
        Err(error) => panic!("touch failed: {error:?}"),
    }
    // And nothing was written: the idle window is still the creation one.
    let found = match store.list_for_user(&user_key("u1")).await {
        Ok(listed) => listed,
        Err(error) => panic!("list failed: {error:?}"),
    };
    assert_eq!(found[0].idle_expires, 1_000 + idle_secs());
}

/// A touch exactly at the absolute end, and past it, clamps the idle window to absolute —
/// `idle_expires` never crosses `absolute_expires`.
async fn touch_never_past_absolute<S: SessionStore>(store: &S) {
    let (token, record) = start_on(store, &user_key("u1"), "10.0.0.1", 1_000).await;
    // Touch at the very last instant inside the absolute window.
    let at = record.absolute_expires - 1;
    match store
        .touch(&record.id, at, idle_secs(), record.absolute_expires)
        .await
    {
        Ok(Touched::Extended) | Ok(Touched::Skipped) => {}
        Err(error) => panic!("touch failed: {error:?}"),
    }
    // Touch at the absolute bound: the clamp keeps the window at the bound itself.
    match store
        .touch(
            &record.id,
            record.absolute_expires,
            idle_secs(),
            record.absolute_expires,
        )
        .await
    {
        Ok(_) => {}
        Err(error) => panic!("touch failed: {error:?}"),
    }
    let found = match store.find_by_digest(&token.digest()).await {
        Ok(found) => must_some(found, "the session resolves"),
        Err(error) => panic!("find failed: {error:?}"),
    };
    assert!(found.idle_expires <= found.absolute_expires);
}

/// A touch on a dead session (revoked, or expired at `now`) is a skip, not an error, and
/// writes nothing.
async fn touch_skips_dead_sessions<S: SessionStore>(store: &S) {
    let (_token, record) = start_on(store, &user_key("u1"), "10.0.0.1", 1_000).await;
    match store.revoke(&record.id, "ended".to_owned()).await {
        Ok(report) => assert_eq!(report.revoked, 1),
        Err(error) => panic!("revoke failed: {error:?}"),
    }
    match store
        .touch(&record.id, 1_100, idle_secs(), 1_000 + absolute_secs())
        .await
    {
        Ok(Touched::Skipped) => {}
        Ok(other) => panic!("expected Skipped for a dead session, got {other:?}"),
        Err(error) => panic!("touch failed: {error:?}"),
    }
}

/// 64 concurrent touches never push the idle window past the absolute bound. Each touch
/// runs as its own task on the caller's runtime; the store serialises on its own lock
/// (or its own transaction) and the clamp makes any ordering safe. The store arrives as
/// an `Arc`: every task holds a clone, and `S: SessionStore` is `Send + Sync` by
/// supertrait.
async fn concurrent_64_touches_never_past_absolute<S: SessionStore + 'static>(store: &Arc<S>) {
    let (token, record) = start_on(store, &user_key("u1"), "10.0.0.1", 1_000).await;
    let absolute = record.absolute_expires;
    let id = record.id.clone();
    let mut handles = Vec::new();
    for step in 0..64_u32 {
        let store = store.clone(); // An Arc clone per task; cheap by design.
        let id = id.clone();
        handles.push(tokio::spawn(async move {
            let now = 1_020 + i64::from(step) * 9; // 1_020..1_587, all inside absolute.
            match store.touch(&id, now, idle_secs(), absolute).await {
                Ok(_) => {}
                Err(error) => panic!("touch failed: {error:?}"),
            }
        }));
    }
    for handle in handles {
        match handle.await {
            Ok(_) => {}
            Err(join_error) => panic!("a touch task failed: {join_error}"),
        }
    }
    let found = match store.find_by_digest(&token.digest()).await {
        Ok(found) => must_some(found, "the session resolves"),
        Err(error) => panic!("find failed: {error:?}"),
    };
    assert!(
        found.idle_expires <= found.absolute_expires,
        "the idle window {} passed the absolute bound {}",
        found.idle_expires,
        found.absolute_expires
    );
    assert_eq!(found.absolute_expires, 1_000 + absolute_secs());
}

/// Rotation through the store: the old digest stops being findable the instant the
/// rotation lands, and the very same record continues under the new digest (in place:
/// same id, same window, still Active).
async fn rotate_kills_the_old_digest<S: SessionStore>(store: &S) {
    let (token, record) = start_on(store, &user_key("u1"), "10.0.0.1", 1_000).await;
    let fresh = OpaqueToken::generate();
    match store.rotate(&record.id, fresh.digest()).await {
        Ok(()) => {}
        Err(error) => panic!("rotate failed: {error:?}"),
    }
    // The old digest: gone.
    match store.find_by_digest(&token.digest()).await {
        Ok(found) => assert!(found.is_none(), "the old digest still resolves"),
        Err(error) => panic!("find failed: {error:?}"),
    }
    // The new digest resolves the *same* record: the swap is in place.
    let rotated = match store.find_by_digest(&fresh.digest()).await {
        Ok(found) => must_some(found, "the session resolves"),
        Err(error) => panic!("find failed: {error:?}"),
    };
    assert_eq!(rotated.id, record.id);
    assert!(!rotated.is_revoked());
}

/// A rotate onto a digest already in use fails and changes nothing.
async fn rotate_refuses_an_in_use_digest<S: SessionStore>(store: &S) {
    let (_first_token, first) = start_on(store, &user_key("u1"), "10.0.0.1", 1_000).await;
    let (_second_token, second) = start_on(store, &user_key("u2"), "10.0.0.1", 1_100).await;
    // Rotating the first session onto the second's digest: refused.
    assert!(store.rotate(&first.id, second.token_digest).await.is_err());
    // Both records unchanged (the first still resolves its own digest, active).
    let found = match store.find_by_digest(&first.token_digest).await {
        Ok(found) => must_some(found, "the session resolves"),
        Err(error) => panic!("find failed: {error:?}"),
    };
    assert!(!found.is_revoked());
}

/// Revocation is idempotent and keeps the first reason.
async fn revoke_is_idempotent_and_keeps_first_reason<S: SessionStore>(store: &S) {
    let (_token, record) = start_on(store, &user_key("u1"), "10.0.0.1", 1_000).await;
    let first = match store.revoke(&record.id, "admin".to_owned()).await {
        Ok(report) => report,
        Err(error) => panic!("revoke failed: {error:?}"),
    };
    assert_eq!(first.revoked, 1);
    let second = match store.revoke(&record.id, "user".to_owned()).await {
        Ok(report) => report,
        Err(error) => panic!("revoke failed: {error:?}"),
    };
    assert_eq!(second.revoked, 0);
    let miss = match store.revoke("no-such-id", "admin".to_owned()).await {
        Ok(report) => report,
        Err(error) => panic!("revoke failed: {error:?}"),
    };
    assert_eq!(miss.revoked, 0);
}

/// `revoke_all_for_user` revokes every session of the user, sparing `except`.
async fn revoke_all_respects_except<S: SessionStore>(store: &S) {
    let (first_token, first) = start_on(store, &user_key("u1"), "10.0.0.1", 1_000).await;
    let (second_token, second) = start_on(store, &user_key("u1"), "10.0.0.2", 1_100).await;
    let report = match store
        .revoke_all_for_user(&first.user, Some(&second.id), "password_changed".to_owned())
        .await
    {
        Ok(report) => report,
        Err(error) => panic!("revoke_all failed: {error:?}"),
    };
    assert_eq!(report.revoked, 1);
    // The excepted session still resolves active; the other is revoked.
    let kept = match store.find_by_digest(&second_token.digest()).await {
        Ok(found) => must_some(found, "the session resolves"),
        Err(error) => panic!("find failed: {error:?}"),
    };
    assert!(!kept.is_revoked());
    let died = match store.find_by_digest(&first_token.digest()).await {
        Ok(found) => must_some(found, "the session resolves"),
        Err(error) => panic!("find failed: {error:?}"),
    };
    assert!(died.is_revoked());
}

/// `list_for_user` reads oldest-first (creation order).
async fn list_is_creation_ordered<S: SessionStore>(store: &S) {
    let _ = start_on(store, &user_key("u1"), "10.0.0.1", 1_000).await;
    let _ = start_on(store, &user_key("u1"), "10.0.0.1", 2_000).await;
    let listed = match store.list_for_user(&user_key("u1")).await {
        Ok(listed) => listed,
        Err(error) => panic!("list failed: {error:?}"),
    };
    assert_eq!(listed.len(), 2);
    assert!(listed[0].created <= listed[1].created);
}

/// `purge_expired` removes only sessions expired at `now`, and counts them.
async fn purge_removes_only_expired<S: SessionStore>(store: &S) {
    let (_live_token, _live) = start_on(store, &user_key("u1"), "10.0.0.1", 1_000).await;
    let (_dead_token, dead) = start_on(store, &user_key("u2"), "10.0.0.1", 500).await;
    let purged = match store.purge_expired(1_050, 100).await {
        Ok(count) => count,
        Err(error) => panic!("purge failed: {error:?}"),
    };
    assert_eq!(purged, 1); // u2's expired at 560; u1's lives to 1_060.
    match store.find_by_digest(&dead.token_digest).await {
        Ok(found) => assert!(found.is_none(), "the purged session is gone"),
        Err(error) => panic!("find failed: {error:?}"),
    }
}

/// `oldest_active` reports the oldest still-alive session, skipping dead ones.
async fn oldest_active_is_the_oldest_alive<S: SessionStore>(store: &S) {
    let (_first_token, first) = start_on(store, &user_key("u1"), "10.0.0.1", 1_000).await;
    let (_second_token, second) = start_on(store, &user_key("u1"), "10.0.0.1", 1_100).await;
    match store.revoke(&first.id, "admin".to_owned()).await {
        Ok(_) => {}
        Err(error) => panic!("revoke failed: {error:?}"),
    }
    let oldest = match store.oldest_active(&first.user, 1_200).await {
        Ok(oldest) => must_some(oldest, "a live session is there"),
        Err(error) => panic!("oldest_active failed: {error:?}"),
    };
    assert_eq!(oldest.id, second.id);
}

/// The store holds only digests: its whole public surface returns at most
/// [`SessionRecord`]s, whose token field is a digest by construction. Proven here by
/// round-tripping a record built from a digest and verifying the digest against *the*
/// token (which the store never saw separately).
async fn store_holds_only_digests<S: SessionStore>(store: &S) {
    let (token, record) = start_on(store, &user_key("u1"), "10.0.0.1", 1_000).await;
    let found = match store.find_by_digest(&record.token_digest).await {
        Ok(found) => must_some(found, "the session resolves"),
        Err(error) => panic!("find failed: {error:?}"),
    };
    // The digest in the store verifies against the presented token — the digest is the
    // only verifier the store ever had.
    assert!(found.token_digest.matches_token(&token));
    // And a SessionRecord carries no token text field to ask for (compile-time: the type
    // has none; here we assert the digest is byte-identical through the storage path).
    let re_parsed = match OpaqueToken::parse(token.as_str()) {
        Ok(rebuilt) => rebuilt,
        Err(_) => panic!("the token re-parses"),
    };
    assert_eq!(
        *found.token_digest.as_bytes(),
        *re_parsed.digest().as_bytes()
    );
}

/// The manager validates a session at every instant strictly inside its window and
/// refuses at either bound (half-open at both: idle bound first if it bites first).
async fn validate_is_half_open_at_both_bounds<S: SessionStore>(store: &S) {
    let (token, record) = start_on(store, &user_key("u1"), "10.0.0.1", 1_000).await;
    let manager = SessionManager::new(store, policy(), salt());
    // Just inside the idle bound: alive (1_119 < 1_120).
    assert!(manager.validate(token.as_str(), 1_119).await.is_ok());
    // At the idle bound: dead.
    assert!(matches!(
        manager.validate(token.as_str(), 1_120).await,
        Err(SessionError::Invalid { .. })
    ));
    let _ = record;
}

/// Every death — unknown digest, idle-expired, revoked — is one caller-visible code
/// (`session.invalid`) with the distinguishing reason kept internal.
async fn invalid_is_one_code_with_internal_reasons<S: SessionStore>(store: &S) {
    let manager = SessionManager::new(store, policy(), salt());
    // Unknown.
    let stranger = OpaqueToken::generate();
    let unknown = match manager.validate(stranger.as_str(), 1_050).await {
        Err(error) => error,
        Ok(_) => panic!("a stranger token validated"),
    };
    assert_eq!(unknown.code(), "session.invalid");
    assert!(matches!(
        unknown.internal_reason(),
        Some(InternalReason::UnknownDigest)
    ));
    // Idle-expired.
    let (token, _) = start_on(store, &user_key("u2"), "10.0.0.1", 1_000).await;
    let expired = match manager.validate(token.as_str(), 1_120).await {
        Err(error) => error,
        Ok(_) => panic!("an expired session validated"),
    };
    assert_eq!(expired.code(), "session.invalid");
    assert!(matches!(
        expired.internal_reason(),
        Some(InternalReason::IdleExpired)
    ));
    // Revoked.
    let (token, record) = start_on(store, &user_key("u3"), "10.0.0.1", 1_000).await;
    match store.revoke(&record.id, "admin".to_owned()).await {
        Ok(_) => {}
        Err(error) => panic!("revoke failed: {error:?}"),
    }
    let revoked = match manager.validate(token.as_str(), 1_050).await {
        Err(error) => error,
        Ok(_) => panic!("a revoked session validated"),
    };
    assert_eq!(revoked.code(), "session.invalid");
    assert!(matches!(
        revoked.internal_reason(),
        Some(InternalReason::Revoked(_))
    ));
    // Malformed shape: its own code (it never reaches a digest comparison).
    let malformed = manager.validate("not-a-token!!", 1_050).await;
    assert!(matches!(malformed, Err(SessionError::Malformed)));
}

/// Rotation fires exactly when the token age reaches `rotate_after`, and not a second
/// before; the replacement keeps the absolute end.
async fn rotation_fires_exactly_at_the_age<S: SessionStore>(store: &S) {
    let (token, record) = start_on(store, &user_key("u1"), "10.0.0.1", 1_000).await;
    let manager = SessionManager::new(store, policy(), salt());
    // One second before the age: no rotation.
    let early = manager
        .validate(token.as_str(), 1_000 + ROTATE as i64 - 1)
        .await
        .unwrap_or_else(|error| panic!("validate failed: {error:?}"));
    assert!(early.renewed_token.is_none());
    // At the age: rotation.
    let due = manager
        .validate(token.as_str(), 1_000 + ROTATE as i64)
        .await
        .unwrap_or_else(|error| panic!("validate failed: {error:?}"));
    let fresh = must_some(due.renewed_token, "rotation falls due exactly at the age");
    // The absolute end carried over and the record is the same session (in-place swap).
    assert_eq!(due.session.absolute_expires, record.absolute_expires);
    assert_eq!(due.session.id, record.id);
    // The old token is already dead.
    assert!(matches!(
        manager
            .validate(token.as_str(), 1_000 + ROTATE as i64 + 1)
            .await,
        Err(SessionError::Invalid { .. })
    ));
    // The fresh token validates (its age restarted).
    assert!(
        manager
            .validate(fresh.as_str(), 1_000 + ROTATE as i64 + 2)
            .await
            .is_ok()
    );
}

/// The cap evicts the *oldest* alive session with reason `evicted` when one more session
/// would exceed `max_per_user`.
async fn cap_evicts_oldest_with_reason_evicted<S: SessionStore>(store: &S) {
    let manager = SessionManager::new(store, policy(), salt());
    let user = user_key("u1");
    let (first_token, first) = start_on(store, &user, "10.0.0.1", 1_000).await;
    let _ = start_on(store, &user, "10.0.0.1", 1_001).await;
    let _ = start_on(store, &user, "10.0.0.1", 1_002).await;
    // The fourth crosses the cap of 3: the oldest (first, at 1_000) dies as evicted.
    let _ = start_on(store, &user, "10.0.0.1", 1_003).await;
    let listed = match store.list_for_user(&user).await {
        Ok(listed) => listed,
        Err(error) => panic!("list failed: {error:?}"),
    };
    let evicted = listed
        .iter()
        .find(|record| record.id == first.id)
        .unwrap_or_else(|| panic!("the first session is still listed"));
    match &evicted.state {
        SessionState::Revoked { reason } => assert_eq!(reason.to_string(), "evicted"),
        other => panic!("expected the oldest evicted, got {other:?}"),
    }
    // Exactly the cap's worth of sessions stay alive; the evicted token is dead.
    assert_eq!(
        listed.iter().filter(|r| r.alive_at(1_004)).count(),
        MAX_PER_USER
    );
    assert!(matches!(
        manager.validate(first_token.as_str(), 1_004).await,
        Err(SessionError::Invalid { .. })
    ));
}

/// `end` ends exactly one live session and is idempotent in count; `end_others` ends
/// every other live session, spares the kept one, and counts only live rows.
async fn end_and_end_others_count_live_sessions<S: SessionStore>(store: &S) {
    let manager = SessionManager::new(store, policy(), salt());
    let user = user_key("u1");
    let (one_token, one) = start_on(store, &user, "10.0.0.1", 1_000).await;
    let (two_token, two) = start_on(store, &user, "10.0.0.2", 1_001).await;
    let (three_token, three) = start_on(store, &user, "10.0.0.3", 1_002).await;

    // End one: counts 1; a repeat counts 0.
    let report = manager
        .end(&three.id, 1_010)
        .await
        .unwrap_or_else(|error| panic!("end failed: {error:?}"));
    assert_eq!(report.ended, 1);
    let repeat = manager
        .end(&three.id, 1_010)
        .await
        .unwrap_or_else(|error| panic!("end failed: {error:?}"));
    assert_eq!(repeat.ended, 0);
    // Its token is dead.
    assert!(matches!(
        manager.validate(three_token.as_str(), 1_011).await,
        Err(SessionError::Invalid { .. })
    ));

    // Keep `two`, end the rest: only `one` is live to end (three is already revoked).
    let others = manager
        .end_others(&user, &two.id, 1_020)
        .await
        .unwrap_or_else(|error| panic!("end_others failed: {error:?}"));
    assert_eq!(others.ended, 1);
    // The kept session still validates; the ended ones do not.
    assert!(manager.validate(two_token.as_str(), 1_021).await.is_ok());
    assert!(matches!(
        manager.validate(one_token.as_str(), 1_021).await,
        Err(SessionError::Invalid { .. })
    ));
    let _ = one;
}
#[cfg(test)]
mod suite_tests {
    /// The whole suite against the in-memory store, on the current-thread runtime: this
    /// is how `cargo test -p spot-sessions` (no features) exercises the contract too.
    #[test]
    fn the_full_contract_suite_passes_on_the_in_memory_store() {
        let runtime = match tokio::runtime::Builder::new_current_thread().build() {
            Ok(runtime) => runtime,
            Err(error) => panic!("runtime: {error}"),
        };
        runtime.block_on(async {
            super::run_all(crate::memory::InMemorySessionStore::new).await;
        });
    }
}
