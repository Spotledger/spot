//! Unit tests of the SSO host with small in-process fixtures: a scripted fetcher for the
//! identity provider, a scripted verifier for the checks `spot-sso` already tests with real keys,
//! and a fake directory. The end-to-end cases against a stub IdP are WP-IAM-05.T14.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use spot_core::{Audience, ManualClock, SiteId};
use spot_egress::{
    Authorized, EgressError, EgressPolicy, Guard, HttpRequest, InMemorySink, Response, Sender,
    TestResolver,
};
use spot_http::api::ports::sso::{CallbackParams, Handover, SignedIn, SsoFailure, SsoPort};
use spot_http::api::service::{ApiError, BoxFuture, RequestCtx, Session, UserInfo};
use spot_http::request::RequestId;
use spot_sso::contract_stores::{FailingPendingStore, sample_provider};
use spot_sso::{
    CompleteError, DirectoryUser, Discovery, IdpConfig, JitPolicy, KeyCache, MemoryPendingStore,
    MemoryProviderStore, MemoryReplayStore, PendingAuth, PendingStore, ProviderConfig, ProviderId,
    ProviderStore, ReplayStore, SamlSignIn, SignInMode, SignInPolicy, SpConfig, SsoError,
    SsoIdentity, TokenResponse, codes,
};
use spot_storage_contract::RecordId;
use time::OffsetDateTime;

use super::fetch::{CacheLimits, EgressFetcher, FetchError, FetchLimits, Fetcher};
use super::ports::{
    AuditUnavailable, Collaborators, DirectorySnapshot, IdentityFailure, NoSamlProviders,
    SignInRequest, SsoAudit, SsoAuditEvent, SsoIdentityPort, SsoSecrets, TracingAudit,
};
use super::stores::Stores;
use super::{LibraryVerifier, SsoAdmission, SsoHost, Verifier};
use spot_secrets::SecretString;

const NOW: i64 = 1_800_000_000;
const ISSUER: &str = "https://idp.example.com";

// ---------------------------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------------------------

fn ctx() -> RequestCtx {
    RequestCtx {
        site: SiteId::new("acme").unwrap(),
        credential: None,
        idempotency_key: None,
        language: None,
        request_id: RequestId::generate(),
        peer: Some("203.0.113.9".parse().unwrap()),
    }
}

fn b64url(bytes: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0_u32, |n, (i, b)| n | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..=chunk.len() {
            out.push(char::from(A[((n >> (18 - 6 * i)) & 63) as usize]));
        }
    }
    out
}

fn discovery_body() -> Vec<u8> {
    serde_json::json!({
        "issuer": ISSUER,
        "authorization_endpoint": "https://idp.example.com/auth",
        "token_endpoint": "https://idp.example.com/token",
        "jwks_uri": "https://idp.example.com/jwks",
        "code_challenge_methods_supported": ["S256"],
        "response_types_supported": ["code"]
    })
    .to_string()
    .into_bytes()
}

fn jwks_body() -> Vec<u8> {
    let mut modulus = vec![0xC1_u8; 256];
    modulus[255] = 0x01;
    serde_json::json!({"keys": [{"kty": "RSA", "kid": "k1", "n": b64url(&modulus), "e": "AQAB"}]})
        .to_string()
        .into_bytes()
}

#[derive(Default)]
struct Script {
    #[allow(clippy::type_complexity)]
    gets: Mutex<BTreeMap<String, Result<Vec<u8>, FetchError>>>,
    #[allow(clippy::type_complexity)]
    posts: Mutex<Vec<(String, Vec<(String, String)>)>>,
    token: Mutex<Option<Result<Vec<u8>, FetchError>>>,
}

struct FakeFetcher(Arc<Script>);

impl Fetcher for FakeFetcher {
    fn get<'a>(&'a self, _t: &'a str, url: &'a str) -> BoxFuture<'a, Result<Vec<u8>, FetchError>> {
        let answer = self
            .0
            .gets
            .lock()
            .unwrap()
            .get(url)
            .cloned()
            .unwrap_or(Err(FetchError::Refused));
        Box::pin(async move { answer })
    }
    fn post_form<'a>(
        &'a self,
        _t: &'a str,
        url: &'a str,
        form: &'a [(String, String)],
    ) -> BoxFuture<'a, Result<Vec<u8>, FetchError>> {
        self.0
            .posts
            .lock()
            .unwrap()
            .push((url.to_owned(), form.to_vec()));
        let answer = self
            .0
            .token
            .lock()
            .unwrap()
            .clone()
            .unwrap_or(Err(FetchError::Refused));
        Box::pin(async move { answer })
    }
}

struct Admit(Mutex<Option<u16>>);

impl SsoAdmission for Admit {
    fn admit<'a>(
        &'a self,
        _t: &'a str,
        _a: &'a str,
        _p: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), ApiError>> {
        let status = *self.0.lock().unwrap();
        Box::pin(async move {
            match status {
                None => Ok(()),
                Some(429) => Err(ApiError::from_code(
                    spot_http::api::registry::Code::ApiRateLimited,
                )),
                Some(_) => Err(ApiError::from_code(
                    spot_http::api::registry::Code::ServerUnavailable,
                )),
            }
        })
    }
}

#[derive(Clone)]
struct Verdict(Arc<Mutex<Option<SsoIdentity>>>);

impl Verifier for Verdict {
    fn oidc(
        &self,
        _c: &ProviderConfig,
        _d: &Discovery,
        _k: &KeyCache,
        _p: &PendingAuth,
        _t: &TokenResponse,
        _n: i64,
    ) -> Result<SsoIdentity, CompleteError> {
        self.0.lock().unwrap().clone().ok_or_else(|| {
            CompleteError::Refused(SsoError::new(codes::TOKEN_MALFORMED, "refused", "refused"))
        })
    }
    fn saml<'a>(
        &'a self,
        _s: &'a SpConfig,
        _i: &'a IdpConfig,
        _p: &'a spot_sso::saml::SamlPendingAuth,
        _r: &'a str,
        _rs: Option<&'a str>,
        _n: i64,
        _replay: &'a Arc<dyn ReplayStore>,
    ) -> BoxFuture<'a, Result<SamlSignIn, SsoError>> {
        let identity = self.0.lock().unwrap().clone();
        Box::pin(async move {
            identity
                .map(|identity| SamlSignIn {
                    identity,
                    session_not_on_or_after: None,
                })
                .ok_or_else(|| SsoError::new(codes::SAML_REPLAY, "refused", "refused"))
        })
    }
}

struct Directory {
    snapshot: Mutex<Option<DirectorySnapshot>>,
    signed_in: Mutex<u32>,
}

impl SsoIdentityPort for Directory {
    fn policy<'a>(
        &'a self,
        _c: &'a RequestCtx,
    ) -> BoxFuture<'a, Result<SignInPolicy, IdentityFailure>> {
        let policy = self
            .snapshot
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|s| s.policy.clone());
        Box::pin(async move { policy.ok_or(IdentityFailure::Unavailable) })
    }
    fn snapshot<'a>(
        &'a self,
        _c: &'a RequestCtx,
        _i: &'a SsoIdentity,
    ) -> BoxFuture<'a, Result<DirectorySnapshot, IdentityFailure>> {
        let snapshot = self.snapshot.lock().unwrap().clone();
        Box::pin(async move { snapshot.ok_or(IdentityFailure::Unavailable) })
    }
    fn sign_in<'a>(
        &'a self,
        _r: SignInRequest<'a>,
    ) -> BoxFuture<'a, Result<Handover, IdentityFailure>> {
        *self.signed_in.lock().unwrap() += 1;
        Box::pin(async {
            Ok(Handover {
                location: "/app".to_owned(),
                cookie: "one-time".to_owned(),
            })
        })
    }
    fn redeem<'a>(
        &'a self,
        _c: &'a RequestCtx,
        handover: &'a str,
    ) -> BoxFuture<'a, Result<SignedIn, IdentityFailure>> {
        let known = handover == "one-time";
        Box::pin(async move {
            if known {
                Ok(SignedIn {
                    session: Session {
                        token: "t".into(),
                        expires_at: "2027-01-01T00:00:00Z".into(),
                        user: UserInfo {
                            id: RecordId::new_v7(),
                            email: "ada@example.com".into(),
                            full_name: "Ada".into(),
                        },
                    },
                })
            } else {
                Err(IdentityFailure::Refused("sso.state_replayed"))
            }
        })
    }
}

struct Secret;

impl SsoSecrets for Secret {
    fn client_secret<'a>(
        &'a self,
        _c: &'a RequestCtx,
        _n: &'a str,
    ) -> BoxFuture<'a, Option<SecretString>> {
        Box::pin(async { Some(SecretString::new("the-client-secret")) })
    }
}

struct DownAudit;

impl SsoAudit for DownAudit {
    fn record<'a>(
        &'a self,
        _c: &'a RequestCtx,
        _e: SsoAuditEvent,
    ) -> BoxFuture<'a, Result<(), AuditUnavailable>> {
        Box::pin(async { Err(AuditUnavailable) })
    }
}

fn identity(provider: &str) -> SsoIdentity {
    SsoIdentity {
        provider: ProviderId::new(provider),
        issuer: ISSUER.to_owned(),
        subject: "sub-1".to_owned(),
        email: Some("ada@example.com".to_owned()),
        email_verified: true,
        display_name: None,
        groups: Vec::new(),
        acr: None,
        amr: Vec::new(),
        auth_time: None,
        raw_claims_hash: "hash".to_owned(),
    }
}

fn policy(provider: &str, mode: SignInMode) -> SignInPolicy {
    SignInPolicy {
        mode,
        providers: vec![ProviderId::new(provider)],
        jit: JitPolicy::default(),
        break_glass: Vec::new(),
        require_second_factor_for_break_glass: true,
    }
}

fn user(user_type: spot_iam_directory::UserType) -> DirectoryUser {
    DirectoryUser {
        id: RecordId::new_v7(),
        enabled: true,
        user_type,
        sign_in_name: "ada@example.com".to_owned(),
        is_tenant_admin: false,
        has_local_password: false,
        second_factors: Vec::new(),
        allows_sso: true,
    }
}

/// A snapshot in which `sub-1` of `ISSUER` is linked to `user`.
fn snapshot_with(provider: &str, user: DirectoryUser) -> DirectorySnapshot {
    DirectorySnapshot {
        policy: Some(policy(provider, SignInMode::SsoOptional)),
        linked: vec![((ISSUER.to_owned(), "sub-1".to_owned()), user.id)],
        users: vec![user],
        ..DirectorySnapshot::default()
    }
}

struct Rig {
    host: SsoHost,
    script: Arc<Script>,
    directory: Arc<Directory>,
    verdict: Verdict,
    admit: Arc<Admit>,
    providers: MemoryProviderStore,
}

fn rig_with(audit: Arc<dyn SsoAudit>, pending: Arc<dyn PendingStore>) -> Rig {
    let backend = MemoryProviderStore::new();
    let providers = backend.for_tenant("acme");
    let script = Arc::new(Script::default());
    {
        let mut gets = script.gets.lock().unwrap();
        gets.insert(
            format!("{ISSUER}/.well-known/openid-configuration"),
            Ok(discovery_body()),
        );
        gets.insert("https://idp.example.com/jwks".to_owned(), Ok(jwks_body()));
        *script.token.lock().unwrap() = Some(Ok(br#"{"id_token":"x"}"#.to_vec()));
    }
    let directory = Arc::new(Directory {
        snapshot: Mutex::new(None),
        signed_in: Mutex::new(0),
    });
    let verdict = Verdict(Arc::new(Mutex::new(None)));
    let admit = Arc::new(Admit(Mutex::new(None)));
    let stores = Stores {
        providers: Arc::new(providers.clone()),
        pending,
        replay: Arc::new(MemoryReplayStore::new()),
        durable: false,
    };
    let collab = Collaborators {
        identity: Some(directory.clone()),
        secrets: Some(Arc::new(Secret)),
        audit,
        saml: Arc::new(NoSamlProviders),
    };
    let host = SsoHost::new(
        "acme",
        Arc::new(ManualClock::new(
            OffsetDateTime::from_unix_timestamp(NOW).unwrap(),
        )),
        stores,
        Arc::new(FakeFetcher(script.clone())),
        CacheLimits::default(),
        collab,
        admit.clone(),
        Arc::new(verdict.clone()),
    );
    Rig {
        host,
        script,
        directory,
        verdict,
        admit,
        providers,
    }
}

fn rig() -> Rig {
    rig_with(
        Arc::new(TracingAudit),
        Arc::new(MemoryPendingStore::new().for_tenant("acme")),
    )
}

async fn install(rig: &Rig, id: &str, audience: Audience, user_type: spot_iam_directory::UserType) {
    let config = sample_provider(id, audience);
    rig.providers.put(&config).await.unwrap();
    rig.script
        .gets
        .lock()
        .unwrap()
        .insert(config.discovery_url.clone(), Ok(discovery_body()));
    *rig.directory.snapshot.lock().unwrap() = Some(snapshot_with(id, user(user_type)));
    *rig.verdict.0.lock().unwrap() = Some(identity(id));
}

fn params(binding: &str, state: &str) -> CallbackParams {
    CallbackParams {
        code: Some("the-code".into()),
        state: Some(state.into()),
        error: None,
        binding: binding.into(),
    }
}

/// Begins a sign-in and returns the binding and the `state` the provider URL carries.
async fn begin(rig: &Rig, id: &str) -> (String, String) {
    let begun = rig.host.begin(&ctx(), id, Some("/app")).await.unwrap();
    let url = url::Url::parse(&begun.redirect_url).unwrap();
    let state = url
        .query_pairs()
        .find(|(k, _)| k == "state")
        .unwrap()
        .1
        .into_owned();
    (begun.binding, state)
}

// ---------------------------------------------------------------------------------------------
// The port
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_oidc_happy_path_signs_in_and_hands_over() {
    let rig = rig();
    install(
        &rig,
        "acme-idp",
        Audience::Staff,
        spot_iam_directory::UserType::Internal,
    )
    .await;
    let (binding, state) = begin(&rig, "acme-idp").await;
    let handover = rig
        .host
        .callback(&ctx(), "acme-idp", &params(&binding, &state))
        .await
        .unwrap();
    assert_eq!(handover.location, "/app");
    assert_eq!(*rig.directory.signed_in.lock().unwrap(), 1);
    // The client secret went in the token request, and only there.
    {
        let posts = rig.script.posts.lock().unwrap();
        assert_eq!(posts.len(), 1);
        assert!(
            posts[0]
                .1
                .iter()
                .any(|(k, v)| k == "client_secret" && v == "the-client-secret")
        );
    }
    // The hand-over is redeemed for the session; an unknown one is refused.
    assert!(rig.host.complete(&ctx(), "one-time").await.is_ok());
    assert_eq!(
        rig.host.complete(&ctx(), "nope").await.unwrap_err(),
        SsoFailure::SignInFailed
    );
}

#[tokio::test]
async fn a_callback_without_the_binding_is_refused() {
    let rig = rig();
    install(
        &rig,
        "acme-idp",
        Audience::Staff,
        spot_iam_directory::UserType::Internal,
    )
    .await;
    let (_binding, state) = begin(&rig, "acme-idp").await;
    let refused = rig
        .host
        .callback(&ctx(), "acme-idp", &params("", &state))
        .await;
    assert_eq!(refused.unwrap_err(), SsoFailure::StateInvalid);
    let foreign = rig
        .host
        .callback(
            &ctx(),
            "acme-idp",
            &params("another-browsers-cookie", &state),
        )
        .await;
    assert_eq!(foreign.unwrap_err(), SsoFailure::StateInvalid);
    assert_eq!(*rig.directory.signed_in.lock().unwrap(), 0);
}

#[tokio::test]
async fn a_state_cannot_be_used_twice_or_tampered() {
    let rig = rig();
    install(
        &rig,
        "acme-idp",
        Audience::Staff,
        spot_iam_directory::UserType::Internal,
    )
    .await;
    let (binding, state) = begin(&rig, "acme-idp").await;
    assert!(
        rig.host
            .callback(&ctx(), "acme-idp", &params(&binding, &state))
            .await
            .is_ok()
    );
    let again = rig
        .host
        .callback(&ctx(), "acme-idp", &params(&binding, &state))
        .await;
    assert_eq!(again.unwrap_err(), SsoFailure::StateInvalid);

    let (binding, _state) = begin(&rig, "acme-idp").await;
    let tampered = rig
        .host
        .callback(&ctx(), "acme-idp", &params(&binding, "forged"))
        .await;
    assert_eq!(tampered.unwrap_err(), SsoFailure::StateInvalid);
    // The tampered attempt spent the entry: the genuine state cannot follow it.
    assert_eq!(*rig.directory.signed_in.lock().unwrap(), 1);
}

#[tokio::test]
async fn another_tenants_provider_is_not_found() {
    let rig = rig();
    install(
        &rig,
        "acme-idp",
        Audience::Staff,
        spot_iam_directory::UserType::Internal,
    )
    .await;
    // A provider that exists only in tenant `other` (the same backend, another view).
    let theirs = MemoryProviderStore::new().for_tenant("other");
    theirs
        .put(&sample_provider("their-idp", Audience::Staff))
        .await
        .unwrap();
    let begun = rig.host.begin(&ctx(), "their-idp", None).await;
    assert_eq!(begun.unwrap_err(), SsoFailure::ProviderNotFound);
    let unknown = rig.host.begin(&ctx(), "no-such", None).await;
    assert_eq!(unknown.unwrap_err(), SsoFailure::ProviderNotFound);
}

#[tokio::test]
async fn a_partner_provider_never_signs_in_a_staff_user() {
    let rig = rig();
    install(
        &rig,
        "partner-idp",
        Audience::Partner,
        spot_iam_directory::UserType::Internal,
    )
    .await;
    let (binding, state) = begin(&rig, "partner-idp").await;
    let refused = rig
        .host
        .callback(&ctx(), "partner-idp", &params(&binding, &state))
        .await;
    assert_eq!(refused.unwrap_err(), SsoFailure::NotAllowed);
    assert_eq!(*rig.directory.signed_in.lock().unwrap(), 0);
}

#[tokio::test]
async fn a_policy_that_is_local_or_does_not_trust_the_provider_refuses_begin() {
    let rig = rig();
    install(
        &rig,
        "acme-idp",
        Audience::Staff,
        spot_iam_directory::UserType::Internal,
    )
    .await;
    rig.directory
        .snapshot
        .lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .policy = Some(policy("acme-idp", SignInMode::Local));
    assert_eq!(
        rig.host.begin(&ctx(), "acme-idp", None).await.unwrap_err(),
        SsoFailure::NotAllowed
    );
    rig.directory
        .snapshot
        .lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .policy = Some(policy("another", SignInMode::SsoOptional));
    assert_eq!(
        rig.host.begin(&ctx(), "acme-idp", None).await.unwrap_err(),
        SsoFailure::NotAllowed
    );
    // A policy that cannot be read is a refusal, never `local`.
    *rig.directory.snapshot.lock().unwrap() = None;
    assert_eq!(
        rig.host.begin(&ctx(), "acme-idp", None).await.unwrap_err(),
        SsoFailure::Unavailable
    );
}

#[tokio::test]
async fn the_throttle_comes_first_and_fails_closed() {
    let rig = rig();
    install(
        &rig,
        "acme-idp",
        Audience::Staff,
        spot_iam_directory::UserType::Internal,
    )
    .await;
    *rig.admit.0.lock().unwrap() = Some(429);
    assert_eq!(
        rig.host.begin(&ctx(), "acme-idp", None).await.unwrap_err(),
        SsoFailure::SignInFailed
    );
    *rig.admit.0.lock().unwrap() = Some(503);
    assert_eq!(
        rig.host.begin(&ctx(), "acme-idp", None).await.unwrap_err(),
        SsoFailure::Unavailable
    );
    // Nothing was fetched and nothing stored for a refused attempt.
    assert!(rig.script.posts.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_pending_store_error_refuses_the_sign_in() {
    let rig = rig_with(Arc::new(TracingAudit), Arc::new(FailingPendingStore));
    install(
        &rig,
        "acme-idp",
        Audience::Staff,
        spot_iam_directory::UserType::Internal,
    )
    .await;
    // Begin cannot record the flow, so it does not start.
    assert_eq!(
        rig.host.begin(&ctx(), "acme-idp", None).await.unwrap_err(),
        SsoFailure::Unavailable
    );
    // A callback cannot take it, so it is refused, and nothing signs in.
    let refused = rig
        .host
        .callback(&ctx(), "acme-idp", &params("b", "s"))
        .await;
    assert_eq!(refused.unwrap_err(), SsoFailure::Unavailable);
    assert_eq!(*rig.directory.signed_in.lock().unwrap(), 0);
}

#[tokio::test]
async fn a_success_that_cannot_be_audited_is_refused() {
    let rig = rig_with(
        Arc::new(DownAudit),
        Arc::new(MemoryPendingStore::new().for_tenant("acme")),
    );
    install(
        &rig,
        "acme-idp",
        Audience::Staff,
        spot_iam_directory::UserType::Internal,
    )
    .await;
    assert_eq!(
        rig.host.begin(&ctx(), "acme-idp", None).await.unwrap_err(),
        SsoFailure::Unavailable
    );
}

#[tokio::test]
async fn an_identity_provider_that_cannot_be_reached_is_unavailable_for_every_egress_refusal() {
    for failure in [
        FetchError::Refused,
        FetchError::NotHttps,
        FetchError::Timeout,
        FetchError::TooLarge,
        FetchError::BadStatus,
        FetchError::Unavailable,
    ] {
        let rig = rig();
        install(
            &rig,
            "acme-idp",
            Audience::Staff,
            spot_iam_directory::UserType::Internal,
        )
        .await;
        let config = sample_provider("acme-idp", Audience::Staff);
        rig.script
            .gets
            .lock()
            .unwrap()
            .insert(config.discovery_url, Err(failure));
        let refused = rig.host.begin(&ctx(), "acme-idp", None).await;
        assert_eq!(
            refused.unwrap_err(),
            SsoFailure::ProviderUnavailable,
            "{failure:?}"
        );
    }
}

#[tokio::test]
async fn an_unknown_kid_refetches_once_per_interval() {
    let rig = rig();
    install(
        &rig,
        "acme-idp",
        Audience::Staff,
        spot_iam_directory::UserType::Internal,
    )
    .await;
    let config: ProviderConfig = sample_provider("acme-idp", Audience::Staff);
    let (discovery, keys) = rig.host.material(&config, false).await.unwrap();
    let pending = sample_pending();
    let _ = (discovery, keys, pending);
    assert!(rig.host.cache.allow_refetch("acme-idp", NOW));
    assert!(!rig.host.cache.allow_refetch("acme-idp", NOW + 1));
}

fn sample_pending() -> PendingAuth {
    spot_sso::contract_stores::sample_pending("t")
}

#[tokio::test]
async fn saml_has_no_provider_until_a_module_supplies_them() {
    let rig = rig();
    install(
        &rig,
        "acme-idp",
        Audience::Staff,
        spot_iam_directory::UserType::Internal,
    )
    .await;
    let acs = spot_http::api::ports::sso::AcsParams {
        saml_response: "x".into(),
        relay_state: None,
        binding: "b".into(),
    };
    assert_eq!(
        rig.host.acs(&ctx(), "saml-idp", &acs).await.unwrap_err(),
        SsoFailure::ProviderNotFound
    );
    assert_eq!(
        rig.host
            .saml_metadata(&ctx(), "saml-idp")
            .await
            .unwrap_err(),
        SsoFailure::ProviderNotFound
    );
    // Logout is rejected until `sso_logout` installs its handler.
    assert_eq!(
        rig.host
            .backchannel_logout(&ctx(), "acme-idp", "tok")
            .await
            .unwrap_err(),
        SsoFailure::LogoutRejected
    );
}

#[tokio::test]
async fn the_provider_list_follows_the_policy() {
    let rig = rig();
    install(
        &rig,
        "acme-idp",
        Audience::Staff,
        spot_iam_directory::UserType::Internal,
    )
    .await;
    let list = rig.host.providers(&ctx()).await.unwrap();
    assert_eq!(list.providers.len(), 1);
    assert!(list.password_login);
    rig.directory
        .snapshot
        .lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .policy = Some(policy("acme-idp", SignInMode::SsoRequired));
    assert!(!rig.host.providers(&ctx()).await.unwrap().password_login);
    rig.directory
        .snapshot
        .lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .policy = Some(policy("acme-idp", SignInMode::Local));
    let list = rig.host.providers(&ctx()).await.unwrap();
    assert!(list.providers.is_empty() && list.password_login);
}

#[test]
fn a_code_is_answered_without_naming_the_failed_check() {
    use super::flow::failure_for;
    assert_eq!(failure_for(codes::STATE_REPLAYED), SsoFailure::StateInvalid);
    assert_eq!(failure_for(codes::WRONG_AUDIENCE), SsoFailure::NotAllowed);
    assert_eq!(
        failure_for(codes::PENDING_STORE_UNAVAILABLE),
        SsoFailure::Unavailable
    );
    // Every signature, nonce, issuer and clock failure is the same answer.
    assert_eq!(
        failure_for(codes::TOKEN_MALFORMED),
        SsoFailure::SignInFailed
    );
    assert_eq!(failure_for("sso.saml.replay"), SsoFailure::SignInFailed);
}

// ---------------------------------------------------------------------------------------------
// Egress
// ---------------------------------------------------------------------------------------------

struct StubSender {
    status: u16,
    delay: Duration,
    too_large: bool,
}

#[async_trait::async_trait]
impl Sender for StubSender {
    async fn send_request(
        &self,
        _request: &HttpRequest,
        _authorized: &Authorized,
        _max_body: u64,
    ) -> Result<Response, EgressError> {
        tokio::time::sleep(self.delay).await;
        if self.too_large {
            return Err(EgressError::ResponseTooLarge);
        }
        Ok(Response {
            status: http::StatusCode::from_u16(self.status).unwrap(),
            headers: http::HeaderMap::new(),
            body: b"{}".to_vec(),
        })
    }
}

fn fetcher(status: u16, delay: Duration, too_large: bool) -> EgressFetcher {
    let resolver = TestResolver::new();
    let public: IpAddr = "93.184.216.34".parse().unwrap();
    let private: IpAddr = "10.0.0.5".parse().unwrap();
    let loopback: IpAddr = "127.0.0.1".parse().unwrap();
    resolver.add_response("idp.example.com", vec![vec![public]; 8]);
    resolver.add_response("intranet.example.com", vec![vec![private]; 8]);
    resolver.add_response("loopback.example.com", vec![vec![loopback]; 8]);
    // The URL path denies a tenant with no entry outright: the tenant allows its provider only.
    let mut policy = EgressPolicy::default();
    policy.tenants.insert(
        "acme".to_owned(),
        spot_egress::TenantPolicy {
            allow: vec![spot_egress::AllowEntry {
                scheme: Some("https".to_owned()),
                host: "idp.example.com".to_owned(),
                ports: None,
                purpose: None,
                allow_private: None,
            }],
            ..Default::default()
        },
    );
    let guard = Arc::new(Guard::new(
        policy,
        Arc::new(resolver),
        Arc::new(InMemorySink::new()),
        Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH)),
    ));
    EgressFetcher::new(
        guard,
        Arc::new(StubSender {
            status,
            delay,
            too_large,
        }),
        FetchLimits {
            timeout: Duration::from_millis(100),
            max_bytes: 1024,
        },
    )
}

#[tokio::test]
async fn the_egress_fetcher_refuses_what_the_policy_and_the_limits_refuse() {
    let ok = fetcher(200, Duration::ZERO, false);
    let first = ok.get("acme", "https://idp.example.com/x").await;
    assert!(first.is_ok(), "{first:?}");
    // Plain http, before any lookup.
    assert_eq!(
        ok.get("acme", "http://idp.example.com/x")
            .await
            .unwrap_err(),
        FetchError::NotHttps
    );
    assert_eq!(
        ok.get("acme", "not a url").await.unwrap_err(),
        FetchError::NotHttps
    );
    // Private and loopback addresses.
    assert_eq!(
        ok.get("acme", "https://intranet.example.com/x")
            .await
            .unwrap_err(),
        FetchError::Refused
    );
    assert_eq!(
        ok.get("acme", "https://loopback.example.com/x")
            .await
            .unwrap_err(),
        FetchError::Refused
    );
    // A redirect is a response, not followed: a refusal.
    let redirect = fetcher(302, Duration::ZERO, false);
    assert_eq!(
        redirect
            .get("acme", "https://idp.example.com/x")
            .await
            .unwrap_err(),
        FetchError::BadStatus
    );
    // A body over the cap, and a slow server.
    let big = fetcher(200, Duration::ZERO, true);
    assert_eq!(
        big.get("acme", "https://idp.example.com/x")
            .await
            .unwrap_err(),
        FetchError::TooLarge
    );
    let slow = fetcher(200, Duration::from_millis(400), false);
    assert_eq!(
        slow.get("acme", "https://idp.example.com/x")
            .await
            .unwrap_err(),
        FetchError::Timeout
    );
    // A form post goes the same way.
    let form = [("a".to_owned(), "b".to_owned())];
    assert!(
        ok.post_form("acme", "https://idp.example.com/token", &form)
            .await
            .is_ok()
    );
    assert_eq!(
        ok.post_form("acme", "http://idp.example.com/token", &form)
            .await
            .unwrap_err(),
        FetchError::NotHttps
    );
}

#[test]
fn the_library_verifier_is_the_default_and_settings_refuse_unknown_keys() {
    let _ = LibraryVerifier;
    let bad: Result<super::Settings, _> = toml::from_str("enabld = true");
    assert!(bad.is_err());
    let ok: super::Settings = toml::from_str("enabled = true\nstore = \"memory\"").unwrap();
    assert!(ok.validate().is_ok());
    let mut wide = ok.clone();
    wide.fetch_timeout_secs = 0;
    assert!(
        wide.validate()
            .unwrap_err()
            .to_string()
            .contains("sso_host.settings_invalid")
    );
    let mut nothing = ok;
    nothing.store = "redis".into();
    assert!(nothing.validate().is_err());
}
