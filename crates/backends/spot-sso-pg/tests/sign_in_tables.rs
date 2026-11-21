//! The sign-in tables moved here from the host (WP-IAM-05.T18): the SAML provider rows, the tenant
//! policy row and the hand-over and session-index tables, one test per moved query.
//!
//! The database cases skip cleanly without `SPOT_TEST_PG_URL` (and fail with `SPOT_REQUIRE_PG=1`,
//! which CI sets). The fail-closed cases need no server.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{dead_pool, pool};
use spot_postgres::testing::TestDb;
use spot_sso::{SignInMode, SignInPolicy};
use spot_sso_pg::{
    HandoverUnavailable, PgHandoverTables, PgPolicyStore, PgSamlRows, PolicyStoreError,
    SamlRowError,
};

const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

// ---- SAML provider rows ----------------------------------------------------------------------

#[tokio::test]
async fn saml_insert_get_list_update_and_delete_by_version() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let pool = pool(&db, 2, "sso_saml_rows");
    let rows = PgSamlRows::new(pool.clone(), "t").unwrap();
    rows.prepare().await.unwrap();
    rows.prepare().await.unwrap();
    // insert: version 1; the same id again exists.
    assert_eq!(rows.insert("okta", "staff", "{\"a\":1}").await, Ok(1));
    assert_eq!(
        rows.insert("okta", "staff", "{}").await,
        Err(SamlRowError::Exists)
    );
    rows.insert("azure", "partner", "{\"b\":2}").await.unwrap();
    // get: the row as stored; an unknown id is None.
    let got = rows.get("okta").await.unwrap().unwrap();
    assert_eq!(
        (got.id.as_str(), got.version, got.audience.as_str()),
        ("okta", 1, "staff")
    );
    assert_eq!(got.config, "{\"a\":1}");
    assert!(rows.get("missing").await.unwrap().is_none());
    // list: ordered by id.
    let ids: Vec<String> = rows
        .list()
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(ids, ["azure", "okta"]);
    // update: only the stored version wins.
    assert_eq!(
        rows.update("okta", "staff", "{\"a\":2}", 7).await,
        Err(SamlRowError::Stale)
    );
    assert_eq!(rows.update("okta", "staff", "{\"a\":2}", 1).await, Ok(2));
    assert_eq!(
        rows.update("okta", "staff", "{\"a\":3}", 1).await,
        Err(SamlRowError::Stale)
    );
    // delete: only the stored version wins; a second delete is stale.
    assert_eq!(rows.delete("okta", 1).await, Err(SamlRowError::Stale));
    assert_eq!(rows.delete("okta", 2).await, Ok(()));
    assert_eq!(rows.delete("okta", 2).await, Err(SamlRowError::Stale));
}

#[tokio::test]
async fn saml_rows_belong_to_their_tenant() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let pool = pool(&db, 2, "sso_saml_tenants");
    let a = PgSamlRows::new(pool.clone(), "a").unwrap();
    let b = PgSamlRows::new(pool, "b").unwrap();
    a.prepare().await.unwrap();
    a.insert("okta", "staff", "{}").await.unwrap();
    assert!(b.get("okta").await.unwrap().is_none());
    assert!(b.list().await.unwrap().is_empty());
    assert_eq!(
        b.update("okta", "staff", "{}", 1).await,
        Err(SamlRowError::Stale)
    );
    assert_eq!(b.delete("okta", 1).await, Err(SamlRowError::Stale));
    assert!(a.get("okta").await.unwrap().is_some());
}

#[tokio::test]
async fn the_saml_table_refuses_an_unknown_audience_and_an_oversize_tenant() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let pool = pool(&db, 2, "sso_saml_check");
    let rows = PgSamlRows::new(pool.clone(), "t").unwrap();
    rows.prepare().await.unwrap();
    // A consumer audience is not a SAML audience: the table's check refuses it.
    assert_eq!(
        rows.insert("market", "consumer", "{}").await,
        Err(SamlRowError::Unavailable)
    );
    assert!(PgSamlRows::new(pool, &"t".repeat(129)).is_none());
}

// ---- tenant policy ---------------------------------------------------------------------------

#[tokio::test]
async fn the_policy_is_local_until_set_and_changes_by_version() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let pool = pool(&db, 2, "sso_policy_rows");
    let store = PgPolicyStore::new(pool.clone(), "t").unwrap();
    store.prepare().await.unwrap();
    store.prepare().await.unwrap();
    // No row: local, version 0.
    let (policy, version) = store.read().await.unwrap();
    assert_eq!((policy, version), (SignInPolicy::local(), 0));
    let optional = SignInPolicy {
        mode: SignInMode::SsoOptional,
        ..SignInPolicy::local()
    };
    let required = SignInPolicy {
        mode: SignInMode::SsoRequired,
        ..SignInPolicy::local()
    };
    // First write (expected 0) creates version 1; a second first write is stale.
    assert_eq!(store.write(&optional, 0).await, Ok(1));
    assert_eq!(
        store.write(&optional, 0).await,
        Err(PolicyStoreError::Stale)
    );
    assert_eq!(store.read().await.unwrap(), (optional.clone(), 1));
    // Compare-and-set on the version.
    assert_eq!(
        store.write(&required, 5).await,
        Err(PolicyStoreError::Stale)
    );
    assert_eq!(store.write(&required, 1).await, Ok(2));
    assert_eq!(store.read().await.unwrap(), (required, 2));
    // Another tenant is still local.
    let other = PgPolicyStore::new(pool, "other").unwrap();
    assert_eq!(other.read().await.unwrap(), (SignInPolicy::local(), 0));
}

#[tokio::test]
async fn an_unreadable_policy_row_is_unavailable_never_local() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let pool = pool(&db, 2, "sso_policy_unreadable");
    let store = PgPolicyStore::new(pool, "t").unwrap();
    store.prepare().await.unwrap();
    db.raw_client()
        .await
        .execute(
            "INSERT INTO spot_sso_policy (tenant, version, policy) VALUES ('t', 1, '{\"mode\":\"nonsense\"}')",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(store.read().await, Err(PolicyStoreError::Unavailable));
}

// ---- hand-over and session index -------------------------------------------------------------

#[tokio::test]
async fn a_handover_is_redeemed_once_and_expires() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let pool = pool(&db, 4, "sso_handover");
    let tables = PgHandoverTables::new(pool.clone(), "t").unwrap();
    tables.prepare().await.unwrap();
    tables.prepare().await.unwrap();
    tables
        .issue(DIGEST, "{\"user\":\"u\"}", 100, 160)
        .await
        .unwrap();
    // Not yet issued, and another tenant, find nothing.
    assert!(
        tables
            .redeem(&DIGEST.replace('0', "f"), 110)
            .await
            .unwrap()
            .is_none()
    );
    let elsewhere = PgHandoverTables::new(pool, "other").unwrap();
    assert!(elsewhere.redeem(DIGEST, 110).await.unwrap().is_none());
    // Redeemed once: the second redemption finds nothing.
    assert_eq!(
        tables.redeem(DIGEST, 110).await.unwrap().as_deref(),
        Some("{\"user\":\"u\"}")
    );
    assert!(tables.redeem(DIGEST, 111).await.unwrap().is_none());
    // The boundary: valid while `now < expires_at`, dead at it.
    tables
        .issue(DIGEST, "{\"user\":\"v\"}", 200, 260)
        .await
        .unwrap();
    assert!(tables.redeem(DIGEST, 260).await.unwrap().is_none());
}

#[tokio::test]
async fn issuing_a_handover_prunes_the_expired_ones_of_its_tenant() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let pool = pool(&db, 2, "sso_handover_prune");
    let tables = PgHandoverTables::new(pool, "t").unwrap();
    tables.prepare().await.unwrap();
    tables.issue(DIGEST, "{}", 0, 10).await.unwrap();
    let other = DIGEST.replace('0', "a");
    tables.issue(&other, "{}", 1_000, 1_060).await.unwrap();
    let count: i64 = db
        .raw_client()
        .await
        .query_one("SELECT count(*) FROM spot_sso_handover", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(count, 1);
}

#[tokio::test]
async fn the_session_index_finds_sessions_by_sid_and_by_user_and_forgets() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let pool = pool(&db, 2, "sso_session_index");
    let tables = PgHandoverTables::new(pool.clone(), "t").unwrap();
    tables.prepare().await.unwrap();
    tables
        .index_session("s1", "u1", "okta", Some("sid-1"), 10)
        .await
        .unwrap();
    tables
        .index_session("s2", "u1", "okta", None, 20)
        .await
        .unwrap();
    tables
        .index_session("s3", "u2", "azure", Some("sid-1"), 30)
        .await
        .unwrap();
    // A repeat of the same session id changes nothing.
    tables
        .index_session("s1", "u9", "okta", None, 40)
        .await
        .unwrap();
    // By sid: only the provider that asked.
    assert_eq!(
        tables.by_sid("okta", "sid-1", 10).await.unwrap(),
        [("s1".to_owned(), "u1".to_owned())]
    );
    // By user: newest first, bounded by the limit.
    assert_eq!(
        tables.by_user("okta", "u1", 10).await.unwrap(),
        [
            ("s2".to_owned(), "u1".to_owned()),
            ("s1".to_owned(), "u1".to_owned())
        ]
    );
    assert_eq!(tables.by_user("okta", "u1", 1).await.unwrap().len(), 1);
    // Another tenant sees nothing; forgetting removes the row.
    let other = PgHandoverTables::new(pool, "other").unwrap();
    assert!(other.by_user("okta", "u1", 10).await.unwrap().is_empty());
    tables.forget("s1").await.unwrap();
    assert!(tables.by_sid("okta", "sid-1", 10).await.unwrap().is_empty());
}

// ---- fail closed -----------------------------------------------------------------------------

#[tokio::test]
async fn a_database_that_cannot_be_reached_is_unavailable_everywhere() {
    let rows = PgSamlRows::new(dead_pool(), "t").unwrap();
    assert_eq!(rows.list().await, Err(SamlRowError::Unavailable));
    assert_eq!(rows.get("x").await, Err(SamlRowError::Unavailable));
    assert_eq!(
        rows.insert("x", "staff", "{}").await,
        Err(SamlRowError::Unavailable)
    );
    assert_eq!(rows.delete("x", 1).await, Err(SamlRowError::Unavailable));
    assert!(rows.prepare().await.is_err());
    let policy = PgPolicyStore::new(dead_pool(), "t").unwrap();
    assert_eq!(policy.read().await, Err(PolicyStoreError::Unavailable));
    assert_eq!(
        policy.write(&SignInPolicy::local(), 0).await,
        Err(PolicyStoreError::Unavailable)
    );
    let tables = PgHandoverTables::new(dead_pool(), "t").unwrap();
    assert_eq!(tables.redeem(DIGEST, 1).await, Err(HandoverUnavailable));
    assert_eq!(
        tables.issue(DIGEST, "{}", 0, 1).await,
        Err(HandoverUnavailable)
    );
    assert_eq!(tables.by_sid("p", "s", 1).await, Err(HandoverUnavailable));
}
