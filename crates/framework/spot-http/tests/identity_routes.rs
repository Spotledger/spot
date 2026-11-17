//! The `/api/v1/identity/` administration routes (contract 0.18.0, WP-IAM-12.T1) over fake ports:
//! every operation mounted and answering its contract status, `501` for an unset area, the group
//! never half wired, bounded inputs, closed bodies (no actor, approver or tenant), the step-up
//! checked before the port, a permission check that fails closed, `no-store` on every answer, the
//! secret on its one response type only, and every refusal and failure audited.
//!
//! The fakes decide nothing the real adapters decide; they answer what a test scripts and keep
//! what the layer passed them.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, header};
use serde_json::{Value as Json, json};
use spot_core::{ManualClock, SiteId};
use spot_http::api::idempotency::MemoryIdempotencyStore;
use spot_http::api::ports::identity::*;
use spot_http::api::registry::Code;
use spot_http::api::router::routes::{self, Registration, RouteState, SkipReason, identity};
use spot_http::api::router::{ApiServices, router_with};
use spot_http::api::service::{ApiError, BoxFuture, RequestCtx};
use spot_http::api::{PortKey, Ports};
use spot_http::{ServerBuilder, ServerConfig, StaticSites};
use tower::ServiceExt as _;

#[allow(dead_code)]
mod support;

const HOST: &str = "erp.acme.com";
const TOKEN: &str = "tok-admin";
const U1: &str = "0190a1b2-c3d4-7e5f-8a6b-1c2d3e4f5a6b";
const U2: &str = "0190a1b2-c3d4-7e5f-8a6b-1c2d3e4f5a6c";
const SECRET: &str = "sk_live_THE_ONE_SECRET_VALUE";

#[derive(Clone, Copy)]
struct Areas {
    users: bool,
    roles: bool,
    assignments: bool,
    keys: bool,
    sessions: bool,
}

const ALL: Areas = Areas {
    users: true,
    roles: true,
    assignments: true,
    keys: true,
    sessions: true,
};

#[rustfmt::skip]
struct Fake {
    areas: Areas,
    /// `Some(error)`: the permission check answers it; `None`: the permission is held.
    perm_error: Mutex<Option<ApiError>>,
    audience: Mutex<CallerAudience>,
    step_up: AtomicBool,
    /// The next port call fails with this.
    fail: Mutex<Option<ApiError>>,
    calls: Mutex<Vec<String>>,
    tokens: Mutex<Vec<String>>,
    keys: Mutex<Vec<Option<String>>>,
    audit: Mutex<Vec<AuditEntry>>,
    grant_needs_approval: AtomicBool,
    creates: AtomicUsize,
}

#[rustfmt::skip]
impl Fake {
    fn new(areas: Areas) -> Arc<Fake> {
        Arc::new(Fake {
            areas, perm_error: Mutex::new(None), audience: Mutex::new(CallerAudience::Staff), step_up: AtomicBool::new(true),
            fail: Mutex::new(None), calls: Mutex::new(Vec::new()), tokens: Mutex::new(Vec::new()), keys: Mutex::new(Vec::new()),
            audit: Mutex::new(Vec::new()), grant_needs_approval: AtomicBool::new(false), creates: AtomicUsize::new(0),
        })
    }
    fn rec(&self, ctx: &RequestCtx, op: &str) -> IdResult<()> {
        self.calls.lock().unwrap().push(op.to_owned());
        self.tokens.lock().unwrap().push(ctx.credential.as_ref().map(|c| c.token.clone()).unwrap_or_default());
        self.keys.lock().unwrap().push(ctx.idempotency_key.clone());
        self.fail.lock().unwrap().take().map_or(Ok(()), Err)
    }
    fn calls(&self) -> Vec<String> { self.calls.lock().unwrap().clone() }
}

#[rustfmt::skip]
fn user() -> UserOut {
    UserOut { id: U1.into(), email: "a@acme.com".into(), full_name: "Ayesha".into(), audience: "staff".into(), status: "active".into(), roles: vec!["Manager".into()], second_factor: true, last_sign_in_at: None, created_at: "2026-11-01T08:00:00Z".into(), version: 7 }
}
#[rustfmt::skip]
fn role() -> RoleOut {
    RoleOut { id: U1.into(), name: "Manager".into(), description: None, builtin: true, customised_from: None, permissions: vec!["a.b".into()], version: 2 }
}
#[rustfmt::skip]
fn assignment() -> AssignmentOut {
    AssignmentOut { id: U1.into(), user_id: U1.into(), role_id: U2.into(), scope: None, granted_by: U2.into(), granted_at: "2026-11-01T08:00:00Z".into(), expires_at: None, source: Some("direct".into()) }
}
#[rustfmt::skip]
fn api_key() -> ApiKeyOut {
    ApiKeyOut { id: U1.into(), name: "ci".into(), owner_id: U2.into(), scopes: vec!["read".into()], created_at: "2026-11-01T08:00:00Z".into(), last_used_at: None, expires_at: None, prefix: Some("sk_ab12".into()) }
}
fn page<T>(items: Vec<T>) -> Page<T> {
    Page {
        items,
        next_cursor: None,
    }
}

macro_rules! ok {
    ($self:ident, $ctx:ident, $op:expr, $val:expr) => {
        Box::pin(async move {
            $self.rec($ctx, $op)?;
            Ok($val)
        })
    };
}

#[rustfmt::skip]
impl UserAdmin for Fake {
    fn user_list<'a>(&'a self, ctx: &'a RequestCtx, _q: UserListIn) -> BoxFuture<'a, IdResult<Page<UserOut>>> {
        ok!(self, ctx, "user_list", page(vec![user()]))
    }
    fn user_create<'a>(&'a self, ctx: &'a RequestCtx, _i: UserCreateIn) -> BoxFuture<'a, IdResult<UserOut>> {
        ok!(self, ctx, "user_create", user())
    }
    fn user_get<'a>(&'a self, ctx: &'a RequestCtx, _id: &'a str) -> BoxFuture<'a, IdResult<UserOut>> {
        ok!(self, ctx, "user_get", user())
    }
    fn user_patch<'a>(&'a self, ctx: &'a RequestCtx, _id: &'a str, _v: u64, _p: UserPatchIn) -> BoxFuture<'a, IdResult<UserOut>> {
        ok!(self, ctx, "user_patch", user())
    }
    fn user_disable<'a>(&'a self, ctx: &'a RequestCtx, _id: &'a str) -> BoxFuture<'a, IdResult<UserChangeOut>> {
        ok!(
            self,
            ctx,
            "user_disable",
            UserChangeOut {
                user: user(),
                leaver_actions: vec![("sessions_ended".into(), 2)]
            }
        )
    }
    fn user_enable<'a>(&'a self, ctx: &'a RequestCtx, _id: &'a str) -> BoxFuture<'a, IdResult<UserChangeOut>> {
        ok!(
            self,
            ctx,
            "user_enable",
            UserChangeOut {
                user: user(),
                leaver_actions: vec![]
            }
        )
    }
    fn user_anonymise<'a>(&'a self, ctx: &'a RequestCtx, _id: &'a str, _c: String) -> BoxFuture<'a, IdResult<UserOut>> {
        ok!(self, ctx, "user_anonymise", user())
    }
    fn user_recover<'a>(&'a self, ctx: &'a RequestCtx, _id: &'a str, _k: String) -> BoxFuture<'a, IdResult<RecoverOut>> {
        ok!(self, ctx, "user_recover", RecoverOut::NeedsApproval)
    }
    fn invitation_list<'a>(&'a self, ctx: &'a RequestCtx, _p: PageIn) -> BoxFuture<'a, IdResult<Page<InvitationOut>>> {
        ok!(
            self,
            ctx,
            "invitation_list",
            page(vec![InvitationOut {
                id: U1.into(),
                user_id: U2.into(),
                email: "a@acme.com".into(),
                status: "pending".into(),
                expires_at: "2026-11-09T08:00:00Z".into(),
                sent_at: "2026-11-02T08:00:00Z".into(),
            }])
        )
    }
    fn invitation_revoke<'a>(&'a self, ctx: &'a RequestCtx, _id: &'a str) -> BoxFuture<'a, IdResult<()>> {
        ok!(self, ctx, "invitation_revoke", ())
    }
    fn invitation_resend<'a>(&'a self, ctx: &'a RequestCtx, _id: &'a str) -> BoxFuture<'a, IdResult<()>> {
        ok!(self, ctx, "invitation_resend", ())
    }
}

#[rustfmt::skip]
impl RoleAdmin for Fake {
    fn role_list<'a>(&'a self, ctx: &'a RequestCtx, _p: PageIn) -> BoxFuture<'a, IdResult<Page<RoleOut>>> {
        ok!(self, ctx, "role_list", page(vec![role()]))
    }
    fn role_get<'a>(&'a self, ctx: &'a RequestCtx, _id: &'a str) -> BoxFuture<'a, IdResult<RoleOut>> {
        ok!(self, ctx, "role_get", role())
    }
    fn role_customise<'a>(&'a self, ctx: &'a RequestCtx, _id: &'a str, _n: String) -> BoxFuture<'a, IdResult<RoleOut>> {
        ok!(self, ctx, "role_customise", role())
    }
    fn role_upgrade_diff<'a>(&'a self, ctx: &'a RequestCtx, _id: &'a str) -> BoxFuture<'a, IdResult<UpgradeDiffOut>> {
        ok!(
            self,
            ctx,
            "role_upgrade_diff",
            UpgradeDiffOut {
                added: vec!["x".into()],
                removed: vec![],
                from_version: 1,
                to_version: 2
            }
        )
    }
}

#[rustfmt::skip]
impl AssignmentAdmin for Fake {
    fn assignment_list<'a>(&'a self, ctx: &'a RequestCtx, _q: AssignmentListIn) -> BoxFuture<'a, IdResult<Page<AssignmentOut>>> {
        ok!(self, ctx, "assignment_list", page(vec![assignment()]))
    }
    fn assignment_create<'a>(&'a self, ctx: &'a RequestCtx, _i: AssignmentCreateIn) -> BoxFuture<'a, IdResult<GrantOut>> {
        let approval = self.grant_needs_approval.load(Ordering::SeqCst);
        ok!(
            self,
            ctx,
            "assignment_create",
            if approval {
                GrantOut::Requested(U2.into())
            } else {
                GrantOut::Granted(assignment())
            }
        )
    }
    fn assignment_delete<'a>(&'a self, ctx: &'a RequestCtx, _id: &'a str) -> BoxFuture<'a, IdResult<()>> {
        ok!(self, ctx, "assignment_delete", ())
    }
    fn rules_get<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, IdResult<RulesOut>> {
        ok!(
            self,
            ctx,
            "rules_get",
            RulesOut {
                rules: vec![],
                version: 3
            }
        )
    }
    fn rules_put<'a>(&'a self, ctx: &'a RequestCtx, v: u64, rules: Vec<RuleIn>) -> BoxFuture<'a, IdResult<RulesOut>> {
        ok!(
            self,
            ctx,
            "rules_put",
            RulesOut {
                rules,
                version: v + 1
            }
        )
    }
    fn rules_preview<'a>(&'a self, ctx: &'a RequestCtx, _r: Option<Vec<RuleIn>>) -> BoxFuture<'a, IdResult<PlanOut>> {
        ok!(
            self,
            ctx,
            "rules_preview",
            PlanOut {
                grants: vec![(U1.into(), U2.into(), None)],
                revokes: vec![],
                truncated: false,
                plan_digest: "abc123".into()
            }
        )
    }
    fn rules_run<'a>(&'a self, ctx: &'a RequestCtx, _d: String) -> BoxFuture<'a, IdResult<RunOut>> {
        ok!(
            self,
            ctx,
            "rules_run",
            RunOut {
                granted: 1,
                revoked: 0,
                refused: 0
            }
        )
    }
}

#[rustfmt::skip]
impl KeyAdmin for Fake {
    fn api_key_list<'a>(&'a self, ctx: &'a RequestCtx, _p: PageIn) -> BoxFuture<'a, IdResult<Page<ApiKeyOut>>> {
        ok!(self, ctx, "api_key_list", page(vec![api_key()]))
    }
    fn api_key_create<'a>(&'a self, ctx: &'a RequestCtx, _i: ApiKeyCreateIn) -> BoxFuture<'a, IdResult<NewApiKey>> {
        self.creates.fetch_add(1, Ordering::SeqCst);
        ok!(
            self,
            ctx,
            "api_key_create",
            NewApiKey {
                key: api_key(),
                secret: SECRET.into()
            }
        )
    }
    fn api_key_delete<'a>(&'a self, ctx: &'a RequestCtx, _id: &'a str) -> BoxFuture<'a, IdResult<()>> {
        ok!(self, ctx, "api_key_delete", ())
    }
    fn scim_list<'a>(&'a self, ctx: &'a RequestCtx, _p: PageIn) -> BoxFuture<'a, IdResult<Page<ScimOut>>> {
        ok!(self, ctx, "scim_list", page(vec![scim()]))
    }
    fn scim_create<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, IdResult<NewScimCredential>> {
        ok!(
            self,
            ctx,
            "scim_create",
            NewScimCredential {
                credential: scim(),
                secret: SECRET.into()
            }
        )
    }
    fn scim_delete<'a>(&'a self, ctx: &'a RequestCtx, _id: &'a str) -> BoxFuture<'a, IdResult<()>> {
        ok!(self, ctx, "scim_delete", ())
    }
}

fn scim() -> ScimOut {
    ScimOut {
        id: U1.into(),
        created_at: "2026-11-01T08:00:00Z".into(),
        last_used_at: None,
        prefix: Some("scim_ab".into()),
    }
}

#[rustfmt::skip]
impl SessionAdmin for Fake {
    fn session_list<'a>(&'a self, ctx: &'a RequestCtx, _q: SessionListIn) -> BoxFuture<'a, IdResult<Page<AdminSessionOut>>> {
        ok!(
            self,
            ctx,
            "session_list",
            page(vec![AdminSessionOut {
                id: U1.into(),
                user_id: U2.into(),
                created_at: "2026-11-01T08:00:00Z".into(),
                last_seen_at: None,
                client: None,
                method: Some("password".into()),
                impersonated_by: None,
            }])
        )
    }
    fn session_end<'a>(&'a self, ctx: &'a RequestCtx, _id: &'a str) -> BoxFuture<'a, IdResult<()>> {
        ok!(self, ctx, "session_end", ())
    }
    fn sessions_end_all<'a>(&'a self, ctx: &'a RequestCtx, _u: &'a str) -> BoxFuture<'a, IdResult<u64>> {
        ok!(self, ctx, "sessions_end_all", 4)
    }
    fn impersonation_start<'a>(&'a self, ctx: &'a RequestCtx, _i: ImpersonationIn) -> BoxFuture<'a, IdResult<String>> {
        ok!(
            self,
            ctx,
            "impersonation_start",
            "2026-11-12T10:00:00Z".to_owned()
        )
    }
    fn impersonation_end<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, IdResult<()>> {
        ok!(self, ctx, "impersonation_end", ())
    }
}

#[rustfmt::skip]
impl IdentityAdminPort for Fake {
    fn users(&self) -> Option<&dyn UserAdmin> {
        self.areas.users.then_some(self as &dyn UserAdmin)
    }
    fn roles(&self) -> Option<&dyn RoleAdmin> {
        self.areas.roles.then_some(self as &dyn RoleAdmin)
    }
    fn assignments(&self) -> Option<&dyn AssignmentAdmin> {
        self.areas
            .assignments
            .then_some(self as &dyn AssignmentAdmin)
    }
    fn keys(&self) -> Option<&dyn KeyAdmin> {
        self.areas.keys.then_some(self as &dyn KeyAdmin)
    }
    fn sessions(&self) -> Option<&dyn SessionAdmin> {
        self.areas.sessions.then_some(self as &dyn SessionAdmin)
    }
}

#[rustfmt::skip]
impl IdentityPermissionPort for Fake {
    fn check<'a>(&'a self, _ctx: &'a RequestCtx, _p: IdentityPermission) -> BoxFuture<'a, IdResult<IdentityCaller>> {
        Box::pin(async move {
            if let Some(e) = self.perm_error.lock().unwrap().clone() {
                return Err(e);
            }
            Ok(IdentityCaller {
                user_id: U2.into(),
                audience: *self.audience.lock().unwrap(),
            })
        })
    }
}

#[rustfmt::skip]
impl IdentityStepUpPort for Fake {
    fn is_recent<'a>(&'a self, _ctx: &'a RequestCtx) -> BoxFuture<'a, IdResult<bool>> {
        Box::pin(async move { Ok(self.step_up.load(Ordering::SeqCst)) })
    }
}

#[rustfmt::skip]
impl IdentityAuditPort for Fake {
    fn record<'a>(&'a self, _ctx: &'a RequestCtx, entry: AuditEntry) -> BoxFuture<'a, IdResult<()>> {
        Box::pin(async move {
            self.audit.lock().unwrap().push(entry);
            Ok(())
        })
    }
}

fn ports_of(fake: &Arc<Fake>) -> Ports {
    let mut ports = Ports::new();
    ports.provide::<dyn IdentityAdminPort>(fake.clone());
    ports.provide::<dyn IdentityPermissionPort>(fake.clone());
    ports.provide::<dyn IdentityStepUpPort>(fake.clone());
    ports.provide::<dyn IdentityAuditPort>(fake.clone());
    ports
}

/// The group as the registry's mount function takes it (the build's registry holds the same).
fn registration() -> Registration {
    Registration {
        name: "identity",
        serves: identity::SERVES,
        required_ports: identity::required_ports,
        routes: identity::routes,
        public: None,
    }
}

fn router_of(fake: &Arc<Fake>) -> axum::Router {
    let state = RouteState::new(ports_of(fake).subset(identity::required_ports()).unwrap());
    let acme = SiteId::new("acme").unwrap();
    ServerBuilder::new(
        ServerConfig::default(),
        StaticSites::new()
            .add(HOST, acme.clone())
            .add_site_name(acme),
    )
    .site_routes(identity::routes().with_state(state))
    .build()
}

#[rustfmt::skip]
struct Reply { status: u16, headers: axum::http::HeaderMap, text: String }

impl Reply {
    fn json(&self) -> Json {
        serde_json::from_str(&self.text).unwrap_or(Json::Null)
    }
    fn code(&self) -> String {
        self.json()["error"]["code"]
            .as_str()
            .unwrap_or("")
            .to_owned()
    }
}

struct Call<'a> {
    method: &'a str,
    path: String,
    body: Option<Json>,
    raw: Option<String>,
    bearer: bool,
    idem: bool,
    if_match: bool,
    content_type: &'a str,
}

fn call<'a>(method: &'a str, path: &str, body: Option<Json>) -> Call<'a> {
    Call {
        method,
        path: format!("/api/v1/identity{path}"),
        body,
        raw: None,
        bearer: true,
        idem: true,
        if_match: true,
        content_type: "application/json",
    }
}

async fn send(router: &axum::Router, c: Call<'_>) -> Reply {
    let raw = c.raw.or(c.body.map(|b| b.to_string())).unwrap_or_default();
    let mut b = Request::builder()
        .method(c.method)
        .uri(c.path.as_str())
        .header(header::HOST, HOST);
    if c.bearer {
        b = b.header(header::AUTHORIZATION, format!("Bearer {TOKEN}"));
    }
    if c.idem {
        b = b.header("idempotency-key", "key-1");
    }
    if c.if_match {
        b = b.header("if-match", "\"7\"");
    }
    if !raw.is_empty() {
        b = b.header(header::CONTENT_TYPE, c.content_type);
    }
    let response = router
        .clone()
        .oneshot(b.body(Body::from(raw)).unwrap())
        .await
        .unwrap();
    let (status, headers) = (response.status().as_u16(), response.headers().clone());
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 22)
        .await
        .unwrap();
    Reply {
        status,
        headers,
        text: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

/// Every operation with a valid request and the status the contract gives it.
#[rustfmt::skip]
fn table() -> Vec<(&'static str, &'static str, String, Option<Json>, u16)> {
    let u = format!("/users/{U1}");
    vec![
        ( "GET", "user_list", "/users?status=active&q=ay&limit=10".into(), None, 200, ),
        ( "POST", "user_create", "/users".into(), Some( json!({"email": "n@acme.com", "full_name": "N", "audience": "staff", "roles": ["Viewer"], "invite": true}), ), 201, ),
        ("GET", "user_get", u.clone(), None, 200),
        ( "PATCH", "user_patch", u.clone(), Some(json!({"full_name": "New Name"})), 200, ),
        ("POST", "user_disable", format!("{u}/disable"), None, 200),
        ("POST", "user_enable", format!("{u}/enable"), None, 200),
        ( "POST", "user_anonymise", format!("{u}/anonymise"), Some(json!({"confirm_email": "a@acme.com"})), 200, ),
        ( "POST", "user_recover", format!("{u}/recover"), Some(json!({"kind": "password_reset"})), 202, ),
        ("GET", "invitation_list", "/invitations".into(), None, 200),
        ( "DELETE", "invitation_revoke", format!("/invitations/{U1}"), None, 204, ),
        ( "POST", "invitation_resend", format!("/invitations/{U1}/resend"), None, 204, ),
        ("GET", "role_list", "/roles".into(), None, 200),
        ("GET", "role_get", format!("/roles/{U1}"), None, 200),
        ( "POST", "role_customise", format!("/roles/{U1}/customise"), Some(json!({"name": "Copy"})), 201, ),
        ( "GET", "role_upgrade_diff", format!("/roles/{U1}/upgrade-diff"), None, 200, ),
        ( "GET", "assignment_list", format!("/assignments?user={U1}&role={U2}"), None, 200, ),
        ( "POST", "assignment_create", "/assignments".into(), Some( json!({"user_id": U1, "role_id": U2, "scope": "company:A", "expires_at": "2027-01-01T00:00:00Z", "reason": "cover"}), ), 201, ),
        ( "DELETE", "assignment_delete", format!("/assignments/{U1}"), None, 204, ),
        ("GET", "rules_get", "/assignment-rules".into(), None, 200),
        ( "PUT", "rules_put", "/assignment-rules".into(), Some( json!({"rules": [{"id": "r1", "when": "dept == 'x'", "role_id": U1, "scope": null, "enabled": true}]}), ), 200, ),
        ( "POST", "rules_preview", "/assignment-rules/preview".into(), Some(json!({})), 200, ),
        ( "POST", "rules_run", "/assignment-rules/run".into(), Some(json!({"plan_digest": "abc123"})), 200, ),
        ("GET", "api_key_list", "/api-keys".into(), None, 200),
        ( "POST", "api_key_create", "/api-keys".into(), Some(json!({"name": "ci", "owner_id": U1, "scopes": ["read"]})), 201, ),
        ( "DELETE", "api_key_delete", format!("/api-keys/{U1}"), None, 204, ),
        ("GET", "scim_list", "/scim-credentials".into(), None, 200),
        ("POST", "scim_create", "/scim-credentials".into(), None, 201),
        ( "DELETE", "scim_delete", format!("/scim-credentials/{U1}"), None, 204, ),
        ( "GET", "session_list", format!("/sessions?user={U1}"), None, 200, ),
        ( "DELETE", "session_end", format!("/sessions/{U1}"), None, 204, ),
        ( "POST", "sessions_end_all", format!("{u}/sessions/end-all"), None, 200, ),
        ( "POST", "impersonation_start", "/impersonation".into(), Some(json!({"user_id": U1, "reason": "ticket 1", "ticket": "T-1"})), 200, ),
        ( "DELETE", "impersonation_end", "/impersonation".into(), None, 204, ),
    ]
}

#[tokio::test]
async fn every_operation_is_mounted_and_answers_its_contract_status() {
    assert_eq!(table().len(), identity::SERVES.len());
    for (method, op, path, body, status) in table() {
        let fake = Fake::new(ALL);
        let r = send(&router_of(&fake), call(method, &path, body)).await;
        assert_eq!(r.status, status, "{op}: {}", r.text);
        assert_eq!(fake.calls(), [op], "{op}: the port is called once");
        assert_eq!(
            r.headers.get("cache-control").unwrap(),
            "private, no-store",
            "{op}"
        );
        assert_eq!(
            fake.tokens.lock().unwrap().as_slice(),
            [TOKEN],
            "{op}: the caller is the credential"
        );
        assert!(
            fake.audit.lock().unwrap().is_empty(),
            "{op}: a success is the adapter's to audit"
        );
    }
}

#[tokio::test]
async fn an_unset_area_answers_501_and_the_port_is_not_called() {
    let none = Areas {
        users: false,
        roles: false,
        assignments: false,
        keys: false,
        sessions: false,
    };
    for (method, op, path, body, _) in table() {
        let fake = Fake::new(none);
        let r = send(&router_of(&fake), call(method, &path, body)).await;
        assert_eq!(r.status, 501, "{op}");
        assert_eq!(r.code(), "service.not_configured", "{op}");
        assert!(fake.calls().is_empty());
    }
    // A filled area serves while the others stay unset.
    let fake = Fake::new(Areas {
        roles: true,
        ..none
    });
    let router = router_of(&fake);
    assert_eq!(send(&router, call("GET", "/roles", None)).await.status, 200);
    assert_eq!(send(&router, call("GET", "/users", None)).await.status, 501);
}

#[test]
fn the_group_is_mounted_only_when_every_port_is_present() {
    let fake = Fake::new(ALL);
    let full = ports_of(&fake);
    let report = routes::mount(&[registration()], &full, &[]).report;
    assert_eq!(report.mounted, vec!["identity"]);
    assert_eq!(report.served.len(), 33);
    // Each of the four ports is required: without one the group is not mounted at all.
    let keys: [PortKey; 4] = [
        PortKey::of::<dyn IdentityAdminPort>(),
        PortKey::of::<dyn IdentityPermissionPort>(),
        PortKey::of::<dyn IdentityStepUpPort>(),
        PortKey::of::<dyn IdentityAuditPort>(),
    ];
    assert_eq!(identity::required_ports(), keys);
    for missing in 0..4 {
        let mut ports = Ports::new();
        if missing != 0 {
            ports.provide::<dyn IdentityAdminPort>(fake.clone());
        }
        if missing != 1 {
            ports.provide::<dyn IdentityPermissionPort>(fake.clone());
        }
        if missing != 2 {
            ports.provide::<dyn IdentityStepUpPort>(fake.clone());
        }
        if missing != 3 {
            ports.provide::<dyn IdentityAuditPort>(fake.clone());
        }
        let report = routes::mount(&[registration()], &ports, &[]).report;
        assert!(report.mounted.is_empty(), "port {missing} is required");
        assert!(matches!(report.skipped[0].1, SkipReason::MissingPort(_)));
    }
}

#[tokio::test]
async fn a_request_without_a_bearer_is_refused_before_anything() {
    let fake = Fake::new(ALL);
    let router = router_of(&fake);
    let r = send(
        &router,
        Call {
            bearer: false,
            ..call("GET", "/users", None)
        },
    )
    .await;
    assert_eq!((r.status, r.code().as_str()), (401, "auth.required"));
    assert!(fake.calls().is_empty());
}

#[tokio::test]
#[rustfmt::skip]
async fn inputs_are_bounded_and_checked_before_the_port() {
    let big = "x".repeat(256 * 1024 + 1);
    let cases: Vec<(&str, String, Option<Json>, u16)> = vec![
        ("GET", "/users?limit=101".into(), None, 400),
        ("GET", "/users?limit=0".into(), None, 400),
        ("GET", "/users?limit=abc".into(), None, 400),
        ( "GET", format!("/users?cursor={}", "c".repeat(2049)), None, 400, ),
        ("GET", "/users?status=bogus".into(), None, 400),
        ("GET", format!("/users?q={}", "q".repeat(101)), None, 400),
        ("GET", "/users?other=1".into(), None, 400),
        ("GET", "/users?limit=1&limit=2".into(), None, 400),
        ("GET", "/sessions".into(), None, 400),
        ("GET", "/assignments?user=not-an-id".into(), None, 400),
        ("GET", "/users/not-an-id".into(), None, 400),
        ("GET", format!("/users/{}", U1.to_uppercase()), None, 400),
        ("DELETE", "/sessions/short".into(), None, 400),
        ( "POST", "/users".into(), Some(json!({"email": "n@acme.com", "full_name": "", "audience": "staff"})), 400, ),
        ( "POST", "/users".into(), Some(json!({"email": "n@acme.com", "full_name": "N", "audience": "consumer"})), 400, ),
        ( "POST", "/users".into(), Some( json!({"email": "n@acme.com", "full_name": "N", "audience": "staff", "roles": vec!["r"; 21]}), ), 400, ),
        ( "POST", "/users".into(), Some( json!({"email": "n@acme.com", "full_name": "N", "audience": "staff", "invite": "yes"}), ), 400, ),
        ("POST", "/users".into(), Some(json!([1, 2])), 400),
        ( "POST", "/users".into(), Some(json!({"email": big, "full_name": "N", "audience": "staff"})), 413, ),
        ( "POST", "/assignments".into(), Some(json!({"user_id": U1, "role_id": "nope"})), 400, ),
        ( "POST", "/assignments".into(), Some(json!({"user_id": U1, "role_id": U2, "expires_at": "tomorrow"})), 400, ),
        ( "POST", "/assignments".into(), Some(json!({"user_id": U1, "role_id": U2, "reason": "r".repeat(501)})), 400, ),
        ( "PUT", "/assignment-rules".into(), Some( json!({"rules": vec![json!({"id": "a", "when": "w", "role_id": U1, "enabled": true}); 201]}), ), 400, ),
        ( "PUT", "/assignment-rules".into(), Some(json!({"rules": [{"id": "a", "when": "w", "role_id": U1}]})), 400, ),
        ( "POST", "/assignment-rules/run".into(), Some(json!({"plan_digest": "bad digest!"})), 400, ),
        ( "POST", "/api-keys".into(), Some(json!({"name": "k", "owner_id": U1, "scopes": vec!["s"; 101]})), 400, ),
        ( "POST", "/impersonation".into(), Some(json!({"user_id": U1, "reason": ""})), 400, ),
    ];
    for (method, path, body, status) in cases {
        let fake = Fake::new(ALL);
        let r = send(&router_of(&fake), call(method, &path, body)).await;
        assert_eq!(
            r.status,
            status,
            "{method} {path:.60}: {}",
            r.text.chars().take(200).collect::<String>()
        );
        assert!(
            fake.calls().is_empty(),
            "{method} {path:.60}: refused before the port"
        );
    }
}

#[tokio::test]
#[rustfmt::skip]
async fn bodies_are_closed_so_none_can_name_an_actor_an_approver_or_a_tenant() {
    for field in ["actor", "approver", "granted_by", "approved_by", "tenant", "tenant_id", "acting_user"] {
        let fake = Fake::new(ALL);
        let mut body = json!({"user_id": U1, "role_id": U2});
        body[field] = json!(U2);
        let r = send(&router_of(&fake), call("POST", "/assignments", Some(body))).await;
        assert_eq!((r.status, r.code().as_str()), (400, "api.invalid_request"), "{field}");
        assert!(!r.text.contains(field), "the refusal does not echo the field name");
        assert!(fake.calls().is_empty());
    }
    let fake = Fake::new(ALL);
    let router = router_of(&fake);
    let patch = call("PATCH", &format!("/users/{U1}"), Some(json!({"status": "active"})));
    assert_eq!(send(&router, patch).await.status, 400);
    let r = send(&router, Call { raw: Some("not json".into()), ..call("POST", "/users", None) }).await;
    assert_eq!((r.status, r.code().as_str()), (400, "api.invalid_json"));
    let r = send(&router, Call { content_type: "text/plain", ..call("POST", "/users", Some(json!({}))) }).await;
    assert_eq!(r.status, 415);
    assert!(fake.calls().is_empty());
}

#[tokio::test]
async fn idempotency_key_and_if_match_are_required_where_the_contract_says() {
    let fake = Fake::new(ALL);
    let router = router_of(&fake);
    let user_body = json!({"email": "n@acme.com", "full_name": "N", "audience": "staff"});
    let r = send(
        &router,
        Call {
            idem: false,
            ..call("POST", "/users", Some(user_body.clone()))
        },
    )
    .await;
    assert_eq!((r.status, r.code().as_str()), (400, "api.invalid_request"));
    let r = send(
        &router,
        Call {
            if_match: false,
            ..call("PATCH", &format!("/users/{U1}"), Some(json!({})))
        },
    )
    .await;
    assert_eq!(
        (r.status, r.code().as_str()),
        (428, "api.precondition_required")
    );
    let r = send(
        &router,
        Call {
            if_match: false,
            ..call("PUT", "/assignment-rules", Some(json!({"rules": []})))
        },
    )
    .await;
    assert_eq!(r.status, 428);
    assert!(fake.calls().is_empty());
    // The key reaches the port as the command id.
    send(&router, call("POST", "/users", Some(user_body))).await;
    assert_eq!(
        fake.keys.lock().unwrap().as_slice(),
        [Some("key-1".to_owned())]
    );
}

#[tokio::test]
async fn every_write_needs_the_step_up_before_the_port_except_preview_and_ending_an_impersonation()
{
    for (method, op, path, body, status) in table() {
        let fake = Fake::new(ALL);
        fake.step_up.store(false, Ordering::SeqCst);
        let r = send(&router_of(&fake), call(method, &path, body)).await;
        let exempt = matches!(op, "rules_preview" | "impersonation_end");
        let read = method == "GET";
        if read || exempt {
            assert_eq!(r.status, status, "{op} needs no step-up");
        } else {
            assert_eq!(
                (r.status, r.code().as_str()),
                (403, "auth.step_up_required"),
                "{op}"
            );
            assert!(fake.calls().is_empty(), "{op}: refused before the port");
            let audit = fake.audit.lock().unwrap();
            assert_eq!(audit.len(), 1, "{op}: the refusal is audited");
            assert_eq!(audit[0].outcome, AuditOutcome::Refused);
            assert_eq!(audit[0].code, "auth.step_up_required");
        }
    }
}

#[tokio::test]
async fn the_permission_check_fails_closed() {
    let held = [
        Some(ApiError::from_code(Code::ServerUnavailable)),
        Some(ApiError::app("engine.timeout", "the engine did not answer")),
        Some(ApiError::from_code(Code::HttpInternal)),
        Some(ApiError::from_code(Code::PermissionDenied)),
    ];
    for error in held {
        let fake = Fake::new(ALL);
        *fake.perm_error.lock().unwrap() = error.clone();
        let r = send(&router_of(&fake), call("GET", "/users", None)).await;
        let expected = match error.unwrap().code.as_str() {
            "server.unavailable" => 503,
            _ => 403,
        };
        assert_eq!(r.status, expected, "{}", r.text);
        assert!(
            fake.calls().is_empty(),
            "an error from the engine is never an allow"
        );
        assert!(
            !r.text.contains("engine.timeout"),
            "the engine's reason is not returned"
        );
    }
    // A consumer is refused everywhere; a partner may write but not read.
    for (audience, read_ok, write_ok) in [
        (CallerAudience::Consumer, false, false),
        (CallerAudience::Partner, false, true),
        (CallerAudience::Staff, true, true),
    ] {
        let fake = Fake::new(ALL);
        *fake.audience.lock().unwrap() = audience;
        let router = router_of(&fake);
        assert_eq!(
            send(&router, call("GET", "/users", None)).await.status == 200,
            read_ok
        );
        let r = send(&router, call("POST", &format!("/users/{U1}/enable"), None)).await;
        assert_eq!(r.status == 200, write_ok);
    }
}

#[tokio::test]
async fn secrets_ride_only_the_one_response_and_are_never_stored_for_replay() {
    let fake = Fake::new(ALL);
    let router = router_of(&fake);
    let created = send(
        &router,
        call(
            "POST",
            "/api-keys",
            Some(json!({"name": "ci", "owner_id": U1, "scopes": ["read"]})),
        ),
    )
    .await;
    assert_eq!(created.status, 201);
    assert_eq!(
        created.headers.get("cache-control").unwrap(),
        "private, no-store"
    );
    assert_eq!(created.json()["data"]["secret"], SECRET);
    let scim = send(&router, call("POST", "/scim-credentials", None)).await;
    assert_eq!(scim.json()["data"]["secret"], SECRET);
    assert_eq!(
        scim.headers.get("cache-control").unwrap(),
        "private, no-store"
    );
    for path in ["/api-keys", "/scim-credentials"] {
        let list = send(&router, call("GET", path, None)).await;
        assert!(
            !list.text.contains(SECRET) && !list.text.contains("secret"),
            "{path}"
        );
    }
    // The wire type has no secret anywhere else, and its Debug redacts it.
    assert!(
        !format!(
            "{:?}",
            NewApiKey {
                key: api_key(),
                secret: SECRET.into()
            }
        )
        .contains(SECRET)
    );
    assert!(!api_key().json().to_string().contains("secret"));
}

#[tokio::test]
async fn the_idempotency_store_never_holds_a_secret_answer() {
    let fake = Fake::new(ALL);
    let clock = Arc::new(ManualClock::new(time::OffsetDateTime::now_utc()));
    let store = Arc::new(MemoryIdempotencyStore::new(clock.clone()));
    let app = support::fake::FakeApp::new(clock);
    let mut services = ApiServices::new(app.clone(), app.clone(), app.clone(), app, store);
    services.ports = ports_of(&fake);
    let (api, report) = router_with(services, &[registration()]);
    assert_eq!(report.mounted, vec!["identity"]);
    let acme = SiteId::new("acme").unwrap();
    let sites = StaticSites::new()
        .add(HOST, acme.clone())
        .add_site_name(acme);
    let router = ServerBuilder::new(ServerConfig::default(), sites)
        .site_routes(api)
        .build();
    let body = json!({"name": "ci", "owner_id": U1, "scopes": ["read"]});
    for _ in 0..2 {
        let r = send(&router, call("POST", "/api-keys", Some(body.clone()))).await;
        assert_eq!(r.status, 201, "{}", r.text);
        assert!(
            r.headers.get("idempotency-replayed").is_none(),
            "no replay of a secret"
        );
    }
    assert_eq!(
        fake.creates.load(Ordering::SeqCst),
        2,
        "both calls reached the adapter"
    );
}

#[tokio::test]
#[rustfmt::skip]
async fn failures_from_the_port_are_rendered_as_they_are_and_audited_without_content() {
    let fake = Fake::new(ALL);
    let router = router_of(&fake);
    let grant = json!({"user_id": U1, "role_id": U2});
    let key = json!({"name": "k", "owner_id": U1, "scopes": ["a"]});
    let user = format!("/users/{U1}");
    let rows = [
        (Code::IdentityNotFound, 404, user.clone(), "GET", None),
        (Code::IdentitySodConflict, 409, "/assignments".to_owned(), "POST", Some(grant)),
        (Code::IdentityLastAdministrator, 409, format!("{user}/disable"), "POST", None),
        (Code::IdentityScopeExceedsOwner, 422, "/api-keys".to_owned(), "POST", Some(key)),
    ];
    for (code, status, path, method, body) in rows {
        *fake.fail.lock().unwrap() = Some(ApiError::from_code(code));
        let r = send(&router, call(method, &path, body)).await;
        assert_eq!((r.status, r.code()), (status, code.as_str().to_owned()));
    }
    let audit = fake.audit.lock().unwrap();
    assert_eq!(audit.len(), 4);
    assert!(audit.iter().all(|e| e.outcome == AuditOutcome::Failed && e.actor.as_deref() == Some(U2)));
    assert_eq!(audit[0].operation, "identity_user_get");
    assert_eq!(audit[0].target.as_deref(), Some(U1));
    assert_eq!(audit[1].code, "identity.sod_conflict");
    let dump = format!("{audit:?}");
    assert!(!dump.contains("a@acme.com") && !dump.contains(SECRET), "no content in an audit record");
}

#[tokio::test]
async fn a_grant_that_needs_approval_answers_202_with_the_request_id_only() {
    let fake = Fake::new(ALL);
    fake.grant_needs_approval.store(true, Ordering::SeqCst);
    let r = send(
        &router_of(&fake),
        call(
            "POST",
            "/assignments",
            Some(json!({"user_id": U1, "role_id": U2})),
        ),
    )
    .await;
    assert_eq!(r.status, 202);
    assert_eq!(r.json(), json!({"data": {"grant_request_id": U2}}));
}

#[tokio::test]
async fn versions_become_etags_and_the_json_has_the_contract_shape() {
    let fake = Fake::new(ALL);
    let router = router_of(&fake);
    let r = send(&router, call("GET", &format!("/users/{U1}"), None)).await;
    assert_eq!(r.headers.get("etag").unwrap(), "\"7\"");
    assert_eq!(r.json()["data"]["last_sign_in_at"], Json::Null);
    assert_eq!(r.json()["data"]["version"], 7);
    let r = send(&router, call("GET", "/assignment-rules", None)).await;
    assert_eq!(r.headers.get("etag").unwrap(), "\"3\"");
    let r = send(&router, call("GET", "/users", None)).await;
    assert_eq!(r.json()["next_cursor"], Json::Null);
    let r = send(
        &router,
        call("POST", &format!("/users/{U1}/sessions/end-all"), None),
    )
    .await;
    assert_eq!(r.json(), json!({"data": {"ended": 4}}));
    let r = send(&router, call("POST", &format!("/users/{U1}/disable"), None)).await;
    assert_eq!(
        r.json()["data"]["leaver_actions"],
        json!([{"action": "sessions_ended", "count": 2}])
    );
}
