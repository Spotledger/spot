//! The ADR-083 conformance suite (Appendix A of the ADR, WP-IAM-11.T6): B1 audience, B2 listener,
//! B3 outside users (portal), B5 audience does not move, B7 defaults, and visible `#[ignore]`d
//! markers for B4, B6, B8 and B9. See the README for what each drives.
//!
//! The real router of each audience runs over the fake application services of `spot-http`'s own
//! test support (included by path) and the real sign-in service over in-memory ports, with real
//! sessions. A failure names the barrier and the task that owns the missing piece.
#![allow(
    dead_code,
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_arguments
)]

#[path = "../../../../crates/framework/spot-http/tests/support/mod.rs"]
mod support;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, header};
use spot_auth::{HashParams, Password, PasswordHash, StoredDigest, TotpSecret};
use spot_core::{Audience, ManualClock, SiteId};
use spot_http::api::idempotency::MemoryIdempotencyStore;
use spot_http::api::{
    ApiServices, AuthService as _, RouteAuth, RouteEntry, has_hand_wired_handler, route_table,
    router_for,
};
use spot_http::{ServerBuilder, ServerConfig, StaticSites};
use spot_iam_directory::UserType;
use spot_identity_conformance::{
    AUDIENCE_NOT_REFUSED, Failure, OPEN_DEFAULT, consumer_surface_failures, mark_failures,
    parse_snapshot, route_set_failures, sample_path, surface_of,
};
use spot_login::{
    BoxFuture, IdentityEvent, IdentityEvents, KeyRecord, KeyStore, LoginAttempt, LoginConfig,
    LoginError, LoginService, UserDirectory, UserRecord, UserState,
};
use spot_sessions::throttle::InMemoryThrottleStore;
use spot_sessions::{
    ClientInfo, InMemorySessionStore, SaltKey, SessionManager, SessionPolicy, SessionStore,
    ThrottleStore, UserKey,
};
use spot_storage_contract::RecordId;
use support::fake::FakeApp;
use time::OffsetDateTime;
use tower::ServiceExt as _;

const HOST: &str = "erp.acme.com";
const PASSWORD: &str = "right-password";
const SNAPSHOT: &str = include_str!("../consumer-surface.snapshot");
/// The routes that need no credential, by operation: the login, the consumer cookie renewal, the
/// invitation acceptance, the single sign-on flow (its own protocol checks) and the two signed
/// inbound webhooks (the link exchange and the process trigger).
const OPEN: &[&str] = &[
    "auth_login",
    "auth_consumer_renew",
    "auth_invitation_accept",
    "sso_complete",
    "sso_provider_list",
    "sso_acs",
    "sso_backchannel_logout",
    "sso_begin",
    "sso_callback",
    "sso_saml_metadata",
    "sso_slo_get",
    "sso_slo_post",
    "link_inbound",
    "process_webhook",
];

// ---- the in-memory ports of the real sign-in service ------------------------------------------

#[derive(Default)]
struct Users(Mutex<BTreeMap<RecordId, UserRecord>>);

impl Users {
    fn add(&self, name: &str, user_type: UserType) -> RecordId {
        let id = RecordId::new_v7();
        let hash: PasswordHash =
            spot_auth::hash_password(&Password::new(PASSWORD), &HashParams::for_tests());
        self.0.lock().unwrap().insert(
            id,
            UserRecord {
                id,
                user_name: name.to_owned(),
                email: None,
                full_name: name.to_owned(),
                state: UserState::Active,
                user_type,
                password_hash: Some(hash),
                totp_secret: None,
                recovery_digests: Vec::new(),
                totp_last_step: None,
            },
        );
        id
    }
}

fn clone_user(user: &UserRecord) -> UserRecord {
    UserRecord {
        id: user.id,
        user_name: user.user_name.clone(),
        email: user.email.clone(),
        full_name: user.full_name.clone(),
        state: user.state,
        user_type: user.user_type,
        password_hash: user.password_hash.clone(),
        totp_secret: None,
        recovery_digests: Vec::new(),
        totp_last_step: None,
    }
}

impl UserDirectory for Users {
    fn find_by_identifier<'a>(
        &'a self,
        identifier: &'a str,
    ) -> BoxFuture<'a, Result<Option<UserRecord>, LoginError>> {
        Box::pin(async move {
            Ok(self
                .0
                .lock()
                .unwrap()
                .values()
                .find(|u| u.user_name.eq_ignore_ascii_case(identifier))
                .map(clone_user))
        })
    }

    fn find_by_id<'a>(
        &'a self,
        id: RecordId,
    ) -> BoxFuture<'a, Result<Option<UserRecord>, LoginError>> {
        Box::pin(async move { Ok(self.0.lock().unwrap().get(&id).map(clone_user)) })
    }

    fn set_password_hash<'a>(
        &'a self,
        _id: RecordId,
        _hash: PasswordHash,
    ) -> BoxFuture<'a, Result<(), LoginError>> {
        Box::pin(async { Ok(()) })
    }

    fn record_totp_step<'a>(
        &'a self,
        _id: RecordId,
        _step: u64,
    ) -> BoxFuture<'a, Result<(), LoginError>> {
        Box::pin(async { Ok(()) })
    }

    fn consume_recovery_digest<'a>(
        &'a self,
        _id: RecordId,
        _index: usize,
    ) -> BoxFuture<'a, Result<(), LoginError>> {
        Box::pin(async { Ok(()) })
    }

    fn set_second_factor<'a>(
        &'a self,
        _id: RecordId,
        _totp: Option<TotpSecret>,
        _recovery: Vec<StoredDigest>,
    ) -> BoxFuture<'a, Result<(), LoginError>> {
        Box::pin(async { Ok(()) })
    }
}

#[derive(Default)]
struct NoKeys;

impl KeyStore for NoKeys {
    fn find_by_key_id<'a>(
        &'a self,
        _key_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<KeyRecord>, LoginError>> {
        Box::pin(async { Ok(None) })
    }
}

#[derive(Default)]
struct Events;

impl IdentityEvents for Events {
    fn emit<'a>(&'a self, _event: IdentityEvent) -> BoxFuture<'a, Result<(), LoginError>> {
        Box::pin(async { Ok(()) })
    }

    fn record_login_attempt<'a>(
        &'a self,
        _attempt: LoginAttempt,
    ) -> BoxFuture<'a, Result<(), LoginError>> {
        Box::pin(async { Ok(()) })
    }
}

struct Rig {
    users: Arc<Users>,
    store: Arc<InMemorySessionStore>,
    service: Arc<LoginService>,
    fake: Arc<FakeApp>,
    clock: Arc<ManualClock>,
}

fn rig() -> Rig {
    let clock = Arc::new(ManualClock::new(
        OffsetDateTime::from_unix_timestamp(1_000_000).unwrap(),
    ));
    let users = Arc::new(Users::default());
    let store = Arc::new(InMemorySessionStore::new());
    let throttle: Arc<dyn ThrottleStore> = Arc::new(InMemoryThrottleStore::new());
    let service = Arc::new(
        LoginService::new(
            LoginConfig::for_tests(),
            store.clone() as Arc<dyn SessionStore>,
            throttle,
            users.clone(),
            Arc::new(NoKeys),
            Arc::new(Events),
            clock.clone(),
            Arc::new(spot_login::Argon2Hasher::new(HashParams::for_tests())),
            Arc::new(spot_login::AllowAll),
        )
        .expect("the sign-in service builds"),
    );
    let fake = FakeApp::new(clock.clone());
    Rig {
        users,
        store,
        service,
        fake,
        clock,
    }
}

impl Rig {
    fn services(&self) -> ApiServices {
        let idem = Arc::new(MemoryIdempotencyStore::new(self.clock.clone()));
        ApiServices::new(
            self.fake.clone(),
            self.fake.clone(),
            self.fake.clone(),
            self.service.clone(),
            idem,
        )
        // The permission summary and the reveal command are optional services: wired, their
        // routes exist for the barriers to be tried against.
        .with_permissions(Arc::new(support::permissions::FakePermissions::new()))
        .with_reveal(Arc::new(support::permissions::FakeReveal::new()))
    }

    /// The router of `audiences` behind the host-to-site resolution, as a listener serves it.
    fn listener(&self, audiences: &[Audience]) -> Router {
        let site = SiteId::new("acme").unwrap();
        ServerBuilder::new(
            ServerConfig::default(),
            StaticSites::new()
                .add(HOST, site.clone())
                .add_site_name(site),
        )
        .site_routes(router_for(audiences, self.services()))
        .build()
    }

    /// A session started straight in the store, whatever the user's type: the way a consumer
    /// session can exist today (no consumer sign-in does).
    async fn session_with(&self, user: RecordId, audience: Audience) -> String {
        let manager = SessionManager::new(
            self.store.clone() as Arc<dyn SessionStore>,
            SessionPolicy::DEFAULT,
            Arc::new(SaltKey::generate()),
        );
        manager
            .start(
                UserKey::new(user.to_string()).unwrap(),
                audience,
                "10.0.0.1",
                ClientInfo::default(),
                1_000_000,
            )
            .await
            .expect("a session starts")
            .token
            .as_str()
            .to_owned()
    }
}

async fn send(router: &Router, method: &str, path: &str, token: Option<&str>) -> (u16, String) {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, HOST);
    if let Some(token) = token {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let request = builder.body(Body::empty()).unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

fn ctx(token: Option<&str>) -> spot_http::api::RequestCtx {
    spot_http::api::RequestCtx {
        site: SiteId::new("acme").unwrap(),
        credential: token.map(spot_http::api::Credential::new),
        idempotency_key: None,
        language: None,
        request_id: spot_http::request::RequestId::generate(),
        peer: None,
    }
}

fn table() -> Vec<RouteEntry> {
    route_table().to_vec()
}

fn staff_routes() -> Vec<RouteEntry> {
    table()
        .into_iter()
        .filter(|e| !e.audiences.contains(&Audience::Consumer))
        .collect()
}

fn assert_no_failures(failures: &[Failure]) {
    assert!(failures.is_empty(), "{failures:#?}");
}

// ---- B1: audience -----------------------------------------------------------------------------

/// A consumer-audience token is refused on every staff route before any handler runs: the
/// answer is 401 or 403 and the fake application services saw no request at all.
#[tokio::test(flavor = "multi_thread")]
async fn b1_every_staff_route_refuses_a_consumer_token_before_any_handler() {
    let rig = rig();
    let staff = rig.users.add("ada", UserType::Internal);
    let portal = rig.users.add("pat", UserType::Portal);
    let router = rig.listener(&[Audience::Staff, Audience::Partner]);
    let mut failures = Vec::new();
    for owner in [staff, portal] {
        let token = rig.session_with(owner, Audience::Consumer).await;
        for entry in staff_routes()
            .into_iter()
            .filter(|e| e.auth == RouteAuth::Required && has_hand_wired_handler(e.operation))
        {
            let before = rig.fake.seen_peers().len();
            let (status, body) = send(
                &router,
                entry.method,
                &sample_path(entry.path),
                Some(&token),
            )
            .await;
            let reached = rig.fake.seen_peers().len() != before;
            if !matches!(status, 401 | 403) || reached {
                failures.push(Failure {
                    code: AUDIENCE_NOT_REFUSED,
                    detail: format!(
                        "{} {} answered {status} (handler reached: {reached}) to a consumer token: {body}",
                        entry.method, entry.path
                    ),
                });
            }
        }
    }
    assert_no_failures(&failures);
}

/// A consumer API-key-shaped string is refused (never a handler); no token is 401 except on the
/// unauthenticated routes.
#[tokio::test(flavor = "multi_thread")]
async fn b1_an_api_key_shaped_string_and_no_token_are_refused() {
    let rig = rig();
    let router = rig.listener(&[Audience::Staff, Audience::Partner]);
    let mut failures = Vec::new();
    for entry in staff_routes()
        .into_iter()
        .filter(|e| has_hand_wired_handler(e.operation))
    {
        let path = sample_path(entry.path);
        let required = entry.auth == RouteAuth::Required;
        let (keyed, _) = send(
            &router,
            entry.method,
            &path,
            Some("spk_abcdefghijkl_notasecret"),
        )
        .await;
        if required && !matches!(keyed, 401 | 403) {
            failures.push(Failure {
                code: AUDIENCE_NOT_REFUSED,
                detail: format!(
                    "{} {} answered {keyed} to an unknown key",
                    entry.method, entry.path
                ),
            });
        }
        let (bare, _) = send(&router, entry.method, &path, None).await;
        if required && bare != 401 {
            failures.push(Failure {
                code: AUDIENCE_NOT_REFUSED,
                detail: format!(
                    "{} {} answered {bare} with no token, not 401",
                    entry.method, entry.path
                ),
            });
        }
    }
    assert_eq!(
        rig.fake.seen_peers().len(),
        0,
        "no handler ran for any of them"
    );
    assert_no_failures(&failures);
}

/// The table half of B1: every route has a mark and the unauthenticated ones are the listed two.
#[test]
fn b1_every_route_is_marked_and_only_the_listed_routes_need_no_credential() {
    assert_no_failures(&mark_failures(&table(), OPEN));
}

// ---- B2: listener -----------------------------------------------------------------------------

/// On the consumer listener every staff route is 404, not 403, with or without a token; the set
/// the consumer router answers equals the set marked consumer, and the surface is pinned.
#[tokio::test(flavor = "multi_thread")]
async fn b2_the_consumer_listener_answers_404_for_every_staff_route() {
    let rig = rig();
    let staff = rig.users.add("ada", UserType::Internal);
    let token = rig.session_with(staff, Audience::Consumer).await;
    let router = rig.listener(&[Audience::Consumer]);
    let mut failures = Vec::new();
    for entry in staff_routes() {
        for bearer in [None, Some(token.as_str())] {
            let (status, _) = send(&router, entry.method, &sample_path(entry.path), bearer).await;
            if status != 404 {
                failures.push(Failure {
                    code: spot_identity_conformance::SURFACE_MISMATCH,
                    detail: format!(
                        "the consumer listener answered {status}, not 404, for {} {}",
                        entry.method, entry.path
                    ),
                });
            }
        }
    }
    assert_no_failures(&failures);
}

#[test]
fn b2_the_consumer_surface_equals_the_snapshot_and_the_routes_marked_consumer() {
    let snapshot = parse_snapshot(SNAPSHOT);
    assert_no_failures(&consumer_surface_failures(&table(), &snapshot));
    // The staff and partner router mounts exactly the table's staff and partner entries that
    // have a handler (operations no hand-wired handler serves are the registry's groups).
    let unrouted: Vec<&str> = table()
        .iter()
        .map(|e| e.operation)
        .filter(|op| !has_hand_wired_handler(op))
        .collect();
    let hand_wired: Vec<String> = table()
        .iter()
        .filter(|e| has_hand_wired_handler(e.operation))
        .filter(|e| {
            e.audiences
                .iter()
                .any(|a| matches!(a, Audience::Staff | Audience::Partner))
        })
        .map(|e| format!("{} {}", e.method, e.path))
        .collect();
    assert_no_failures(&route_set_failures(
        &table(),
        &[Audience::Staff, Audience::Partner],
        &hand_wired,
        &unrouted,
    ));
    assert_eq!(surface_of(&table(), Audience::Consumer), snapshot);
}

/// The suite's own comparison notices a tampered table: one staff route flipped to consumer, and
/// one route dropped.
#[test]
fn meta_the_comparisons_notice_a_tampered_table() {
    let snapshot = parse_snapshot(SNAPSHOT);
    static CONSUMER: &[Audience] = &[Audience::Consumer];

    let mut flipped = table();
    let victim = flipped
        .iter()
        .position(|e| e.operation == "resource_get")
        .unwrap();
    flipped[victim].audiences = CONSUMER;
    assert!(
        !consumer_surface_failures(&flipped, &snapshot).is_empty(),
        "a staff route marked consumer must fail the snapshot comparison"
    );

    let mut dropped = table();
    let gone = dropped.remove(victim);
    let mounted: Vec<String> = table()
        .iter()
        .filter(|e| has_hand_wired_handler(e.operation))
        .map(|e| format!("{} {}", e.method, e.path))
        .collect();
    let failures = route_set_failures(
        &dropped,
        &[Audience::Staff, Audience::Partner, Audience::Consumer],
        &mounted,
        &[],
    );
    assert!(
        failures.iter().any(|f| f.detail.contains(gone.path)),
        "a route the table no longer lists must fail the route-set comparison: {failures:?}"
    );

    let mut unmarked = table();
    unmarked[0].audiences = &[];
    assert!(!mark_failures(&unmarked, OPEN).is_empty());
}

// ---- B3: outside users (portal) ---------------------------------------------------------------

const ROLES: &str =
    include_str!("../../../../crates/framework/spot-iam-roles/tests/fixtures/outside.toml");

/// A role with a staff duty is refused to a portal user on every entry the guard serves: the
/// allow-list (kind and exposed duties), including through an inherited role and a direct right.
#[test]
fn b3_a_portal_user_is_refused_every_role_with_a_staff_duty() {
    let registry = spot_iam_roles::Registry::from_toml(ROLES).expect("the roles fixture");
    let guard = spot_iam_roles::OutsideGuard::new(&registry, ["pat"]);
    for refused in [
        "Desk Clerk",
        "Portal Leaky",
        "Portal Inherits Leaky",
        "Portal Old Duty",
        "Portal Over Desk",
        "Portal Direct",
    ] {
        let role = spot_iam_roles::RoleName::from(refused);
        assert!(
            guard.check("pat", &role).is_err(),
            "{refused} must be refused"
        );
    }
    for allowed in ["Portal Viewer", "Portal Empty"] {
        let role = spot_iam_roles::RoleName::from(allowed);
        assert!(
            guard.check("pat", &role).is_ok(),
            "{allowed} is a plain portal role"
        );
    }
    // A role the registry does not know is refused (a forged name), and a staff user is not
    // limited by the portal rule.
    let forged = spot_iam_roles::RoleName::from("Forged");
    assert!(guard.check("pat", &forged).is_err());
    assert!(
        guard
            .check("ada", &spot_iam_roles::RoleName::from("Desk Clerk"))
            .is_ok()
    );
}

/// Structural half for consumers: no consumer-facing crate exists yet, so none can depend on a
/// crate that writes a role assignment or a `User` row. When the first one appears this list
/// must name it and the dependency scan below must pass over it.
#[test]
fn b3_the_consumer_facing_crates_are_none_yet() {
    const CONSUMER_FACING_CRATES: &[&str] = &[];
    const WRITERS: &[&str] = &[
        "spot-iam-assign",
        "spot-iam-directory",
        "spot-iam-governance",
        "spot-iam-scim",
    ];
    assert!(
        CONSUMER_FACING_CRATES.is_empty(),
        "a consumer-facing crate exists: scan its `cargo metadata` dependencies for {WRITERS:?} and remove this assertion"
    );
}

// ---- B5: audience does not move ---------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn b5_a_session_keeps_its_audience_and_no_endpoint_changes_it() {
    let rig = rig();
    rig.users.add("ada", UserType::Internal);
    rig.users.add("pat", UserType::Portal);
    for (name, expected) in [("ada", Audience::Staff), ("pat", Audience::Partner)] {
        let session = rig
            .service
            .login_with_source(
                &ctx(None),
                &spot_http::api::LoginBody {
                    user: name.to_owned(),
                    password: PASSWORD.to_owned(),
                    totp: None,
                    recovery_code: None,
                },
                "10.0.0.1",
            )
            .await
            .expect("a first factor signs in");
        for _ in 0..3 {
            let ctx = ctx(Some(&session.token));
            assert_eq!(rig.service.audience_of(&ctx).await.unwrap(), expected);
        }
    }
    // No route takes an audience, and none is named for changing one.
    for entry in table() {
        let lower = entry.path.to_ascii_lowercase();
        assert!(
            !lower.contains("/audience") && !lower.ends_with("{audience}"),
            "{} {} looks like an endpoint that could change an audience",
            entry.method,
            entry.path
        );
    }
    // A consumer token presented where a staff API key is expected is refused: it is not a key.
    let rig2 = rig_with_consumer_token().await;
    let (status, _) = send(
        &rig2.0.listener(&[Audience::Staff, Audience::Partner]),
        "GET",
        "/api/v1/auth/session",
        Some(&rig2.1),
    )
    .await;
    assert!(matches!(status, 401 | 403));
}

async fn rig_with_consumer_token() -> (Rig, String) {
    let rig = rig();
    let user = rig.users.add("ada", UserType::Internal);
    let token = rig.session_with(user, Audience::Consumer).await;
    (rig, token)
}

// ---- B7: defaults -----------------------------------------------------------------------------

/// A route that was never marked is absent: unmarked means absent, an empty audience list mounts
/// nothing, and a group that serves an operation the table does not list is never mounted.
#[tokio::test(flavor = "multi_thread")]
async fn b7_unmarked_means_absent_on_every_listener() {
    let rig = rig();
    let staff = rig.users.add("ada", UserType::Internal);
    let token = rig.session_with(staff, Audience::Staff).await;
    // An unmarked path (nothing in the table): absent on the staff listener too.
    let staff_router = rig.listener(&[Audience::Staff, Audience::Partner]);
    let (status, _) = send(
        &staff_router,
        "GET",
        "/api/v1/not-in-the-table",
        Some(&token),
    )
    .await;
    assert_eq!(status, 404);
    // No audiences: nothing is mounted.
    let nothing = rig.listener(&[]);
    for entry in table() {
        let (status, _) = send(
            &nothing,
            entry.method,
            &sample_path(entry.path),
            Some(&token),
        )
        .await;
        assert_eq!(
            status, 404,
            "{} {} must be absent",
            entry.method, entry.path
        );
    }
    // And no table entry is open by default: the only entries a consumer may use are the
    // snapshot's.
    let consumer_open: Vec<String> = surface_of(&table(), Audience::Consumer);
    assert_eq!(consumer_open, parse_snapshot(SNAPSHOT));
    assert!(
        table().iter().all(|e| !e.audiences.is_empty()),
        "{OPEN_DEFAULT}: a route with an empty mark"
    );
}

// ---- outstanding barriers (each names the task that owns it) ----------------------------------

#[test]
#[ignore = "gap: ADR-083 task 5 not merged: user_type immutable on every channel (B4)"]
fn b4_user_type_is_immutable_on_every_channel() {
    // Intended: PATCH of the user record, SCIM PATCH/PUT, import, bulk update, workflow action
    // and a server script each fail to change `user_type` after creation, with a stable error and
    // an audit record. No channel exists to assert against until task 5 lands.
}

#[test]
#[ignore = "gap: ADR-083 task 7 not merged for the consumer door: no oracle on unauthenticated routes (B6)"]
fn b6_unauthenticated_routes_are_no_oracle() {
    // Intended: sign-in, one-time-code request, recovery and registration answer the same status,
    // body shape and cost class for a known and an unknown name; the coordinator throttle
    // (WP-IAM-11.T7, adopted by spot-login in T11) counts both alike. The consumer doors adopt it
    // in WP-IAM-11.T14.
}

#[test]
#[ignore = "gap: ADR-083 tasks 16 to 18 not merged: surfaces, bundles and the command interface (B8, B9)"]
fn b8_b9_surfaces_bundles_and_commands_are_closed_by_default() {
    // Intended: a consumer surface, an app bundle and a command each need an explicit consumer
    // mark; unmarked ones are absent from the consumer listener and refused on the staff one.
}
