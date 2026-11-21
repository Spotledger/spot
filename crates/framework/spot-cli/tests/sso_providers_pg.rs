//! WP-IAM-05.T16 over a real PostgreSQL: the SAML provider store, the policy store, the one-time
//! hand-over, the single-logout session index and the replay store that must survive a restart.
//!
//! Skipped (with the reason printed) when `SPOT_TEST_PG_URL` is unset; `SPOT_REQUIRE_PG=1` makes a
//! missing server loud. Each test makes its own scratch database. Nothing here is faked: a
//! "restart" is a second store value over the same pool, exactly what a rebuilt host holds.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, dead_code)]

mod common;

use common::Env;
use spot_cli::services::signin_policy::PolicySource;
use spot_cli::services::sso::SamlProviders;
use spot_cli::services::sso_identity::handover::{HandoverPayload, Tables};
use spot_cli::services::sso_policy::{PgPolicyStore, PolicyStoreError};
use spot_cli::services::sso_saml_store::{PgSamlStore, SamlStoreError, StoredSaml};
use spot_core::{Audience, SiteId};
use spot_http::api::service::{Credential, RequestCtx};
use spot_http::request::RequestId;
use spot_sso::saml::{IdpConfig, SpConfig};
use spot_sso::{Claim, ProviderId, ReplayStore, SignInMode, SignInPolicy};
use spot_storage_contract::RecordId;

/// The database of a scratch site, opened the way the host opens it.
async fn pool() -> Option<(Env, spot_postgres::PgBackend)> {
    let Some(env) = Env::new("").await else {
        eprintln!(
            "skipped: SPOT_TEST_PG_URL is not set (the sign-in provider tests need PostgreSQL)"
        );
        return None;
    };
    // The site must exist before the backend can be opened on it.
    env.site();
    let loaded = spot_cli::config::load(&env.root.0, false).unwrap();
    let site = loaded
        .sites
        .iter()
        .find(|s| s.site == env.site)
        .unwrap()
        .clone();
    let backend = spot_cli::schema::backend(&loaded, &site).await.unwrap();
    Some((env, backend))
}

fn ctx(site: &str) -> RequestCtx {
    RequestCtx {
        site: SiteId::new(site).unwrap(),
        credential: Some(Credential::new("t")),
        idempotency_key: None,
        language: None,
        request_id: RequestId::generate(),
        peer: None,
    }
}

fn saml(id: &str) -> spot_cli::services::sso::SamlProvider {
    let sp = SpConfig {
        entity_id: "https://erp.acme.example/saml/sp".to_owned(),
        acs_url: format!("https://erp.acme.example/api/v1/auth/sso/{id}/acs"),
        ..SpConfig::default()
    };
    let idp = IdpConfig::new(
        ProviderId::new(id),
        "https://idp.example/entity",
        "https://idp.example/sso",
        vec![vec![0x30, 0x82, 0x01, 0x0a]],
    );
    spot_cli::services::sso::SamlProvider {
        sp,
        idp,
        name: "Acme IdP".to_owned(),
        audience: Audience::Staff,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_saml_store_round_trips_versions_and_isolates_tenants() {
    let Some((env, db)) = pool().await else {
        return;
    };
    let a = PgSamlStore::new(db.pool(), &env.site).unwrap();
    let b = PgSamlStore::new(db.pool(), "another-tenant").unwrap();
    a.prepare().await.unwrap();
    a.prepare().await.unwrap();
    let id = ProviderId::new("okta");

    // Create: version 1; a second create of the id is refused.
    assert_eq!(a.put(&id, &saml("okta"), None).await, Ok(1));
    assert_eq!(
        a.put(&id, &saml("okta"), None).await,
        Err(SamlStoreError::Exists)
    );
    let found = a.find(&id).await.unwrap().unwrap();
    assert_eq!(
        (found.version, found.provider.name.as_str()),
        (1, "Acme IdP")
    );
    assert_eq!(
        StoredSaml::of(&found.provider),
        StoredSaml::of(&saml("okta"))
    );

    // Replace needs the stored version.
    let mut renamed = saml("okta");
    renamed.name = "Renamed".to_owned();
    assert_eq!(
        a.put(&id, &renamed, Some(7)).await,
        Err(SamlStoreError::Stale)
    );
    assert_eq!(a.put(&id, &renamed, Some(1)).await, Ok(2));
    assert_eq!(a.delete(&id, 1).await, Err(SamlStoreError::Stale));

    // Another tenant sees nothing, and cannot delete it.
    assert!(b.find(&id).await.unwrap().is_none());
    assert!(b.all().await.unwrap().is_empty());
    assert_eq!(b.delete(&id, 2).await, Err(SamlStoreError::Stale));
    // The host port answers for its own site only.
    let own = SamlProviders::get(&a, &ctx(&env.site), &id).await.unwrap();
    assert_eq!(own.unwrap().name, "Renamed");
    assert!(
        SamlProviders::get(&a, &ctx("other"), &id)
            .await
            .unwrap()
            .is_none()
    );
    let listed = SamlProviders::list(&a, &ctx(&env.site)).await.unwrap();
    assert_eq!(listed, vec![(id.clone(), "Renamed".to_owned())]);

    assert_eq!(a.delete(&id, 2).await, Ok(()));
    assert!(a.find(&id).await.unwrap().is_none());
    // A consumer SAML provider is not stored.
    let mut consumer = saml("c");
    consumer.audience = Audience::Consumer;
    assert_eq!(
        a.put(&ProviderId::new("c"), &consumer, None).await,
        Err(SamlStoreError::Invalid)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_policy_is_local_until_set_changes_by_version_and_is_per_tenant() {
    let Some((env, db)) = pool().await else {
        return;
    };
    let store = PgPolicyStore::new(db.pool(), &env.site).unwrap();
    store.prepare().await.unwrap();
    let other = PgPolicyStore::new(db.pool(), "another-tenant").unwrap();

    // Never set: local, version 0 (the door allows passwords: an absent row is not an error).
    let (policy, version) = store.read().await.unwrap();
    assert_eq!((policy.mode, version), (SignInMode::Local, 0));

    let optional = SignInPolicy {
        mode: SignInMode::SsoOptional,
        ..SignInPolicy::local()
    };
    assert_eq!(store.write(&optional, 0).await, Ok(1));
    // Two administrators starting from the same version: the second loses.
    assert_eq!(
        store.write(&SignInPolicy::local(), 0).await,
        Err(PolicyStoreError::Stale)
    );
    assert_eq!(store.write(&SignInPolicy::local(), 1).await, Ok(2));

    // The password door reads it fresh, for its own tenant only.
    let door = PolicySource::load(&store, &env.site).await.unwrap();
    assert_eq!(door.mode, spot_login::SigninMode::Local);
    assert!(PolicySource::load(&store, "another-tenant").await.is_err());
    let (theirs, theirs_version) = other.read().await.unwrap();
    assert_eq!((theirs.mode, theirs_version), (SignInMode::Local, 0));
}

fn payload(user: RecordId) -> HandoverPayload {
    HandoverPayload {
        user: user.to_string(),
        provider: "okta".to_owned(),
        audience: "staff".to_owned(),
        method: "oidc".to_owned(),
        sid: Some("sid-1".to_owned()),
        not_on_or_after: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_handover_is_redeemed_once_expires_and_belongs_to_its_tenant() {
    let Some((env, db)) = pool().await else {
        return;
    };
    let tables = Tables::new(db.pool(), &env.site).unwrap();
    tables.prepare().await.unwrap();
    tables.prepare().await.unwrap();
    let elsewhere = Tables::new(db.pool(), "another-tenant").unwrap();
    let user = RecordId::new_v7();
    let now = 1_790_000_000;

    let cookie = tables.issue(&payload(user), now, 60).await.unwrap();
    // Another tenant, a malformed value and an unknown value get nothing and spend nothing.
    assert_eq!(elsewhere.redeem(&cookie, now + 1).await, Ok(None));
    assert_eq!(tables.redeem("not-a-token", now + 1).await, Ok(None));
    // Of two parallel redemptions exactly one gets the payload.
    let (first, second) = tokio::join!(
        tables.redeem(&cookie, now + 1),
        tables.redeem(&cookie, now + 1)
    );
    let won: Vec<_> = [first.unwrap(), second.unwrap()]
        .into_iter()
        .flatten()
        .collect();
    assert_eq!(won.len(), 1);
    assert_eq!(won[0], payload(user));
    assert_eq!(tables.redeem(&cookie, now + 2).await, Ok(None));

    // Expired: not returned (and the boundary instant is expired).
    let late = tables.issue(&payload(user), now, 60).await.unwrap();
    assert_eq!(tables.redeem(&late, now + 60).await, Ok(None));
    let edge = tables.issue(&payload(user), now, 60).await.unwrap();
    assert!(tables.redeem(&edge, now + 59).await.unwrap().is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_session_index_finds_only_the_sessions_of_the_provider_that_asked() {
    let Some((env, db)) = pool().await else {
        return;
    };
    let tables = Tables::new(db.pool(), &env.site).unwrap();
    tables.prepare().await.unwrap();
    let other = Tables::new(db.pool(), "another-tenant").unwrap();
    let (ada, bob) = (RecordId::new_v7(), RecordId::new_v7());
    let now = 1_790_000_000;

    tables
        .index_session("s1", ada, "okta", Some("sid-A"), now)
        .await
        .unwrap();
    tables
        .index_session("s2", ada, "okta", Some("sid-B"), now + 1)
        .await
        .unwrap();
    tables
        .index_session("s3", ada, "entra", Some("sid-A"), now + 2)
        .await
        .unwrap();
    tables
        .index_session("s4", bob, "okta", Some("sid-A"), now + 3)
        .await
        .unwrap();
    tables
        .index_session("s5", ada, "okta", None, now + 4)
        .await
        .unwrap();
    // The same session twice is one row.
    tables
        .index_session("s1", ada, "okta", Some("sid-A"), now)
        .await
        .unwrap();

    let by_sid = tables.by_sid("okta", "sid-A", 10).await.unwrap();
    let ids: Vec<&str> = by_sid.iter().map(|(s, _)| s.as_str()).collect();
    assert_eq!(
        ids,
        ["s4", "s1"],
        "the provider and the sid, newest first, nobody else"
    );
    let by_user = tables.by_user("okta", ada, 10).await.unwrap();
    let ids: Vec<&str> = by_user.iter().map(|(s, _)| s.as_str()).collect();
    assert_eq!(ids, ["s5", "s2", "s1"], "Ada's sessions through okta only");
    assert_eq!(
        tables.by_user("okta", ada, 2).await.unwrap().len(),
        2,
        "the per-user cap"
    );
    assert!(other.by_sid("okta", "sid-A", 10).await.unwrap().is_empty());

    tables.forget("s1").await.unwrap();
    assert_eq!(tables.by_sid("okta", "sid-A", 10).await.unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_recorded_assertion_id_is_still_refused_after_the_host_is_rebuilt() {
    let Some((env, db)) = pool().await else {
        return;
    };
    let first = spot_sso_pg::PgReplayStore::new(db.pool(), env.site.clone());
    first.prepare().await.unwrap();
    let now = 1_790_000_000;
    assert_eq!(
        first.claim("assertion-1", now, now + 600).await.unwrap(),
        Claim::Fresh
    );
    assert_eq!(
        first
            .claim("assertion-1", now + 1, now + 600)
            .await
            .unwrap(),
        Claim::Replayed
    );

    // The host is rebuilt: a new store value over the same database.
    drop(first);
    let rebuilt = spot_sso_pg::PgReplayStore::new(db.pool(), env.site.clone());
    assert_eq!(
        rebuilt
            .claim("assertion-1", now + 2, now + 600)
            .await
            .unwrap(),
        Claim::Replayed
    );
    // Another tenant's namespace is its own.
    let other = spot_sso_pg::PgReplayStore::new(db.pool(), "another-tenant");
    assert_eq!(
        other
            .claim("assertion-1", now + 2, now + 600)
            .await
            .unwrap(),
        Claim::Fresh
    );
    // After the record expires the id is fresh again.
    assert_eq!(
        rebuilt
            .claim("assertion-1", now + 601, now + 1200)
            .await
            .unwrap(),
        Claim::Fresh
    );
}
