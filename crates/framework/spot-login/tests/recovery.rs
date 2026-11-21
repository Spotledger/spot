//! WP-PLT-02.T10: recovery codes live in `recovery_code` records and sessions carry a source
//! digest and an end reason. Every case runs on the in-memory backend and on PostgreSQL
//! (`SPOT_TEST_PG_URL`).
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use std::sync::Arc;

use common::storage as st;
use common::{CountingHasher, MemEvents};
use spot_auth::{StoredDigest, TotpSecret, generate_recovery_codes};
use spot_http::api::{ApiError, LoginBody, Session};
use spot_login::{LoginConfig, LoginErrorKind, LoginService, UserDirectory};
use spot_sessions::model::SessionEndReason;
use spot_sessions::{SessionPolicy, SessionState, SessionStore};
use spot_storage_contract::StorageBackend;
use time::OffsetDateTime;

const NOW: i64 = 1_000_000;
const PASSWORD: &str = "correct horse battery staple 7731";

fn at(unix: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(unix).expect("a valid instant")
}

fn service<B: StorageBackend>(rig: &st::Rig<B>) -> Arc<LoginService> {
    Arc::new(rig.service_with(
        LoginConfig {
            cache_ttl: std::time::Duration::ZERO,
            ..LoginConfig::for_tests()
        },
        Arc::new(MemEvents::default()),
        Arc::new(CountingHasher::new()),
    ))
}

async fn enrol<B: StorageBackend>(
    rig: &st::Rig<B>,
    user: st::RecordId,
    count: usize,
) -> Vec<String> {
    let codes = generate_recovery_codes(count);
    let digests: Vec<StoredDigest> = codes.iter().map(|code| code.digest()).collect();
    rig.stores
        .user_directory()
        .set_second_factor(user, Some(TotpSecret::generate()), digests)
        .await
        .expect("the factor and its codes are stored");
    codes.iter().map(|code| code.display()).collect()
}

async fn sign_in(service: &LoginService, code: &str, source: &str) -> Result<Session, ApiError> {
    service
        .login_with_source(
            &common::ctx(None),
            &LoginBody {
                user: "ada".to_owned(),
                password: PASSWORD.to_owned(),
                totp: None,
                recovery_code: Some(code.to_owned()),
            },
            source,
        )
        .await
}

async fn rig_with_ada<B: StorageBackend>(rig: &st::Rig<B>) -> st::RecordId {
    let id = rig.user("ada", "active").await;
    rig.set_password(id, PASSWORD).await;
    id
}

// A recovery code signs in once, in order, and a second use is refused as a wrong code.
async fn case_a_code_works_once<B: StorageBackend>(rig: st::Rig<B>) {
    let id = rig_with_ada(&rig).await;
    let codes = enrol(&rig, id, 10).await;
    let service = service(&rig);
    assert!(sign_in(&service, &codes[3], "10.0.0.1").await.is_ok());
    let again = sign_in(&service, &codes[3], "10.0.0.2").await.unwrap_err();
    assert_eq!(again.code.as_str(), "auth.totp_invalid");
    // Another code of the batch is untouched by that.
    assert!(sign_in(&service, &codes[4], "10.0.0.3").await.is_ok());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_recovery_code_works_once_on_the_fake_backend() {
    case_a_code_works_once(st::fake_at(at(NOW)).await).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_recovery_code_works_once_on_postgres() {
    if let Some(rig) = st::postgres_at(at(NOW)).await {
        case_a_code_works_once(rig).await;
    }
}

// Two parallel attempts with one code: exactly one succeeds.
async fn case_concurrent_use<B: StorageBackend + 'static>(rig: st::Rig<B>) {
    let id = rig_with_ada(&rig).await;
    let codes = enrol(&rig, id, 3).await;
    let service = service(&rig);
    let mut tasks = Vec::new();
    for n in 0..4 {
        let service = service.clone();
        let code = codes[0].clone();
        tasks.push(tokio::spawn(async move {
            sign_in(&service, &code, &format!("10.0.1.{n}"))
                .await
                .is_ok()
        }));
    }
    let mut won = 0;
    for task in tasks {
        if task.await.unwrap() {
            won += 1;
        }
    }
    assert_eq!(won, 1, "one code, one sign-in, however many race for it");
}

#[tokio::test(flavor = "multi_thread")]
async fn two_concurrent_uses_of_one_code_succeed_once_on_the_fake_backend() {
    case_concurrent_use(st::fake_at(at(NOW)).await).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn two_concurrent_uses_of_one_code_succeed_once_on_postgres() {
    if let Some(rig) = st::postgres_at(at(NOW)).await {
        case_concurrent_use(rig).await;
    }
}

// Regenerating replaces the batch; removing the factor deletes it.
async fn case_regenerate_and_remove<B: StorageBackend>(rig: st::Rig<B>) {
    let id = rig_with_ada(&rig).await;
    let old = enrol(&rig, id, 3).await;
    let new = enrol(&rig, id, 3).await;
    let service = service(&rig);
    let refused = sign_in(&service, &old[0], "10.0.2.1").await.unwrap_err();
    assert_eq!(
        refused.code.as_str(),
        "auth.totp_invalid",
        "the old batch is dead"
    );
    assert!(sign_in(&service, &new[0], "10.0.2.2").await.is_ok());
    let directory = rig.stores.user_directory();
    assert_eq!(
        directory
            .find_by_id(id)
            .await
            .unwrap()
            .unwrap()
            .recovery_digests
            .len(),
        3
    );

    directory
        .set_second_factor(id, None, Vec::new())
        .await
        .expect("the factor is removed");
    let record = directory.find_by_id(id).await.unwrap().unwrap();
    assert!(
        record.totp_secret.is_none(),
        "no second factor after removal"
    );
    assert!(record.recovery_digests.is_empty(), "the batch went with it");
    // With no factor the password alone signs in, and no code is a factor.
    assert!(sign_in(&service, &new[1], "10.0.2.3").await.is_ok());
}

#[tokio::test(flavor = "multi_thread")]
async fn regenerate_invalidates_the_old_batch_on_the_fake_backend() {
    case_regenerate_and_remove(st::fake_at(at(NOW)).await).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn regenerate_invalidates_the_old_batch_on_postgres() {
    if let Some(rig) = st::postgres_at(at(NOW)).await {
        case_regenerate_and_remove(rig).await;
    }
}

// The digest is a sealed secret: no column holds it, and no read path shows it.
async fn case_digest_is_never_readable<B: StorageBackend>(rig: st::Rig<B>) {
    let id = rig_with_ada(&rig).await;
    let codes = generate_recovery_codes(2);
    let digests: Vec<StoredDigest> = codes.iter().map(|code| code.digest()).collect();
    let digest_hex: Vec<String> = digests
        .iter()
        .map(|d| d.as_bytes().iter().map(|b| format!("{b:02x}")).collect())
        .collect();
    let display: Vec<String> = codes.iter().map(|code| code.display()).collect();
    rig.stores
        .user_directory()
        .set_second_factor(id, Some(TotpSecret::generate()), digests)
        .await
        .unwrap();
    let record = rig
        .stores
        .user_directory()
        .find_by_id(id)
        .await
        .unwrap()
        .unwrap();
    let shown = format!("{record:?}");
    for text in digest_hex.iter().chain(display.iter()) {
        assert!(
            !shown.contains(text),
            "a read path showed a digest or a code"
        );
    }
    let ids = rig_recovery_ids(&rig, id).await;
    assert_eq!(ids.len(), 2);
    for row_id in ids {
        let row = rig.raw("recovery_code", row_id).await;
        let text = format!("{row:?}");
        for needle in digest_hex.iter().chain(display.iter()) {
            assert!(
                !text.contains(needle),
                "a column holds the digest or the code"
            );
        }
        assert!(
            rig.bytes("recovery_code", row_id, "digest__sealed")
                .await
                .is_some(),
            "the digest is sealed"
        );
    }
}

async fn rig_recovery_ids<B: StorageBackend>(
    rig: &st::Rig<B>,
    user: st::RecordId,
) -> Vec<st::RecordId> {
    use spot_storage_contract::{Expr, Query, QueryPath, Selection};
    let entity = st::n("recovery_code");
    let alias = st::n("c");
    let mut query = Query::from(&entity, &alias, QueryPath::Report);
    query.select = vec![Selection {
        expr: Expr::field(&alias, &st::n("id")),
        alias: st::n("v"),
    }];
    query.filter = Some(Expr::field(&alias, &st::n("user")).eq(Expr::value(st::Value::Id(user))));
    let mut tx = rig
        .engine
        .backend()
        .begin(
            rig.catalog.clone(),
            st::TxOptions::new(st::n("test_ids"), st::Isolation::ReadCommitted).read_only(),
        )
        .await
        .unwrap();
    let rows = st::QueryExecutor::query(&mut tx, &query).await.unwrap();
    let _ = st::Transaction::rollback(tx).await;
    rows.rows
        .into_iter()
        .flatten()
        .filter_map(|value| match value {
            st::Value::Id(id) => Some(id),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_digest_is_never_readable_on_the_fake_backend() {
    case_digest_is_never_readable(st::fake_at(at(NOW)).await).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_digest_is_never_readable_on_postgres() {
    if let Some(rig) = st::postgres_at(at(NOW)).await {
        case_digest_is_never_readable(rig).await;
    }
}

// Each of the eight end causes is stored and read back, and the source digest is kept.
async fn case_end_reasons<B: StorageBackend>(rig: st::Rig<B>) {
    let causes = [
        ("ended", "sign_out"),
        ("revoked_by_user", "revoked_by_user"),
        ("revoked_by_admin", "revoked_by_admin"),
        ("deactivated", "leaver"),
        ("sso_logout", "sso_logout"),
        ("password_changed", "password_change"),
        ("idle", "idle"),
        ("absolute", "absolute"),
    ];
    let store = rig.stores.session_store();
    for (n, (reason, stored)) in causes.into_iter().enumerate() {
        let user = rig.user(&format!("user{n}"), "active").await;
        let started = rig
            .manager(SessionPolicy::DEFAULT)
            .start(
                st::user_key(user),
                spot_core::Audience::Staff,
                "10.0.0.1",
                st::client(),
                NOW,
            )
            .await
            .expect("a session starts");
        let id = started.session.id.clone();
        let row = rig.raw(st::SESSION, st::record_id(&id)).await;
        // The source digest is a keyed digest as base16, never the address.
        let shown = format!("{row:?}");
        assert!(!shown.contains("10.0.0.1"), "the address reached the row");
        let digest = common::storage::text_of(&row, "source_digest").expect("a source digest");
        assert_eq!(digest.len(), 64);
        assert!(common::storage::text_of(&row, "end_reason").is_none());

        assert_eq!(
            store.revoke(&id, reason.to_owned()).await.unwrap().revoked,
            1
        );
        let row = rig.raw(st::SESSION, st::record_id(&id)).await;
        assert_eq!(
            common::storage::text_of(&row, "end_reason").as_deref(),
            Some(stored),
            "{reason}"
        );
        // Read back through the store: revoked, with the source digest intact.
        let listed = store.list_for_user(&st::user_key(user)).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert!(
            matches!(listed[0].state, SessionState::Revoked { .. }),
            "{reason}"
        );
        assert_eq!(listed[0].source, started.session.source, "{reason}");
        // The first end stands.
        assert_eq!(
            store.revoke(&id, "ended".to_owned()).await.unwrap().revoked,
            0
        );
        let row = rig.raw(st::SESSION, st::record_id(&id)).await;
        assert_eq!(
            common::storage::text_of(&row, "end_reason").as_deref(),
            Some(stored)
        );
    }
    let evicted = SessionState::Revoked {
        reason: SessionEndReason::Evicted,
    };
    assert_eq!(evicted.clone(), evicted);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_carries_its_end_reason_for_each_cause_on_the_fake_backend() {
    case_end_reasons(st::fake_at(at(NOW)).await).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_carries_its_end_reason_for_each_cause_on_postgres() {
    if let Some(rig) = st::postgres_at(at(NOW)).await {
        case_end_reasons(rig).await;
    }
}

// A session written before the columns existed (no digest, no reason) still reads: an unknown
// source, and an end with no reason is a plain end.
async fn case_legacy_session<B: StorageBackend>(rig: st::Rig<B>) {
    let user = rig.user("old", "active").await;
    let started = rig
        .manager(SessionPolicy::DEFAULT)
        .start(
            st::user_key(user),
            spot_core::Audience::Staff,
            "10.0.0.9",
            st::client(),
            NOW,
        )
        .await
        .unwrap();
    let id = st::record_id(&started.session.id);
    rig.poke(st::SESSION, id, "source_digest", st::Value::Null)
        .await;
    rig.poke(st::SESSION, id, "end_reason", st::Value::Null)
        .await;
    let store = rig.stores.session_store();
    let listed = store.list_for_user(&st::user_key(user)).await.unwrap();
    assert_eq!(listed[0].source.as_bytes(), &[0u8; 32], "an unknown source");
    assert!(
        store
            .revoke(&started.session.id, "ended".to_owned())
            .await
            .unwrap()
            .revoked
            == 1
    );
    assert_eq!(
        LoginErrorKind::RecoveryCodeUsed.code(),
        "login.recovery_code_used"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_from_before_the_upgrade_reads_as_unknown_source_on_the_fake_backend() {
    case_legacy_session(st::fake_at(at(NOW)).await).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_from_before_the_upgrade_reads_as_unknown_source_on_postgres() {
    if let Some(rig) = st::postgres_at(at(NOW)).await {
        case_legacy_session(rig).await;
    }
}
