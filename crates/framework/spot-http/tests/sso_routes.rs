//! The single sign-on routes (WP-IAM-05.T10, contract 0.17.0): the public sign-in group over a
//! fake `SsoPort` and the administration group over a fake `SsoAdminPort`, driven through the
//! real site layer without a socket.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, header};
use serde_json::{Value as Json, json};
use spot_core::SiteId;
use spot_http::api::ports::sso::{
    AcsParams, Begin, CallbackParams, Handover, ProviderList, ProviderPublic, SignedIn, SloOutcome,
    SloParams, SsoAdminPort, SsoFailure, SsoPort,
};
use spot_http::api::router::routes::{Registration, RouteState, mount, sso, sso_admin};
use spot_http::api::service::{BoxFuture, RequestCtx, Session, UserInfo};
use spot_http::api::{Code, Ports};
use spot_http::{ServerBuilder, ServerConfig, StaticSites};
use tower::ServiceExt as _;

use admin::FakeAdmin;

const HOST: &str = "erp.acme.com";
const BINDING: &str = "bind-1";
const HANDOVER: &str = "hand-1";

// --- the fake public port ---------------------------------------------------------------------

#[derive(Default)]
struct FakeSso {
    calls: Mutex<Vec<String>>,
    fail_with: Mutex<Option<SsoFailure>>,
    location: Mutex<Option<String>>,
    redirect: Mutex<Option<String>>,
}

impl FakeSso {
    fn note(&self, what: &str) {
        self.calls.lock().unwrap().push(what.to_owned());
    }
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
    fn outcome<T>(&self, ok: T) -> Result<T, SsoFailure> {
        match *self.fail_with.lock().unwrap() {
            Some(kind) => Err(kind),
            None => Ok(ok),
        }
    }
}

impl SsoPort for FakeSso {
    fn providers<'a>(
        &'a self,
        _ctx: &'a RequestCtx,
    ) -> BoxFuture<'a, Result<ProviderList, SsoFailure>> {
        Box::pin(async move {
            self.note("providers");
            self.outcome(ProviderList {
                providers: vec![ProviderPublic {
                    id: "okta".into(),
                    name: "Okta".into(),
                    kind: "oidc".into(),
                }],
                password_login: true,
            })
        })
    }
    fn begin<'a>(
        &'a self,
        _ctx: &'a RequestCtx,
        provider: &'a str,
        return_to: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Begin, SsoFailure>> {
        Box::pin(async move {
            self.note(&format!("begin {provider} {return_to:?}"));
            let url = self
                .redirect
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| "https://idp.example.com/authorize?x=1".into());
            self.outcome(Begin {
                redirect_url: url,
                binding: BINDING.into(),
            })
        })
    }
    fn callback<'a>(
        &'a self,
        _ctx: &'a RequestCtx,
        provider: &'a str,
        params: &'a CallbackParams,
    ) -> BoxFuture<'a, Result<Handover, SsoFailure>> {
        Box::pin(async move {
            self.note(&format!("callback {provider} binding={}", params.binding));
            let location = self
                .location
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| "/desk".into());
            self.outcome(Handover {
                location,
                cookie: HANDOVER.into(),
            })
        })
    }
    fn acs<'a>(
        &'a self,
        _ctx: &'a RequestCtx,
        provider: &'a str,
        params: &'a AcsParams,
    ) -> BoxFuture<'a, Result<Handover, SsoFailure>> {
        Box::pin(async move {
            self.note(&format!(
                "acs {provider} len={} relay={:?}",
                params.saml_response.len(),
                params.relay_state
            ));
            self.outcome(Handover {
                location: "/".into(),
                cookie: HANDOVER.into(),
            })
        })
    }
    fn complete<'a>(
        &'a self,
        _ctx: &'a RequestCtx,
        handover: &'a str,
    ) -> BoxFuture<'a, Result<SignedIn, SsoFailure>> {
        Box::pin(async move {
            self.note(&format!("complete {handover}"));
            self.outcome(SignedIn {
                session: Session {
                    token: "tok-secret".into(),
                    expires_at: "2026-11-12T20:00:00Z".into(),
                    user: UserInfo {
                        id: spot_storage_contract::RecordId::new_v7(),
                        email: "a@acme.example".into(),
                        full_name: "A".into(),
                    },
                },
            })
        })
    }
    fn backchannel_logout<'a>(
        &'a self,
        _ctx: &'a RequestCtx,
        provider: &'a str,
        _logout_token: &'a str,
    ) -> BoxFuture<'a, Result<(), SsoFailure>> {
        Box::pin(async move {
            self.note(&format!("backchannel {provider}"));
            self.outcome(())
        })
    }
    fn single_logout<'a>(
        &'a self,
        _ctx: &'a RequestCtx,
        provider: &'a str,
        params: &'a SloParams,
    ) -> BoxFuture<'a, Result<SloOutcome, SsoFailure>> {
        Box::pin(async move {
            self.note(&format!("slo {provider} {:?}", params.binding));
            self.outcome(SloOutcome::Redirect("https://idp.example.com/slo".into()))
        })
    }
    fn saml_metadata<'a>(
        &'a self,
        _ctx: &'a RequestCtx,
        provider: &'a str,
    ) -> BoxFuture<'a, Result<String, SsoFailure>> {
        Box::pin(async move {
            self.note(&format!("metadata {provider}"));
            self.outcome("<EntityDescriptor/>".to_owned())
        })
    }
}

/// The public group as the registry holds it (`build.rs` fills the same row from the file).
fn sso_registration() -> Registration {
    Registration {
        name: "sso",
        serves: sso::SERVES,
        required_ports: sso::required_ports,
        routes: sso::routes,
        public: Some(sso::PUBLIC),
    }
}

/// The administration group as the registry holds it (authenticated).
fn admin_registration() -> Registration {
    Registration {
        name: "sso_admin",
        serves: sso_admin::SERVES,
        required_ports: sso_admin::required_ports,
        routes: sso_admin::routes,
        public: None,
    }
}

// --- harness ----------------------------------------------------------------------------------

struct Fixture {
    router: Router,
    sso: Arc<FakeSso>,
    admin: Arc<FakeAdmin>,
}

fn site_router(routes: Router) -> Router {
    let acme = SiteId::new("acme").unwrap();
    ServerBuilder::new(
        ServerConfig::default(),
        StaticSites::new()
            .add(HOST, acme.clone())
            .add_site_name(acme),
    )
    .site_routes(routes)
    .build()
}

fn fixture() -> Fixture {
    let sso = Arc::new(FakeSso::default());
    let admin = Arc::new(FakeAdmin::default());
    *admin.step_up.lock().unwrap() = true;
    let mut ports = Ports::new();
    ports.provide::<dyn SsoPort>(sso.clone());
    ports.provide::<dyn SsoAdminPort>(admin.clone());
    let public = sso::routes().with_state(RouteState::new(
        ports.subset(sso::required_ports()).unwrap(),
    ));
    let admin_routes = sso_admin::routes().with_state(RouteState::new(
        ports.subset(sso_admin::required_ports()).unwrap(),
    ));
    Fixture {
        router: site_router(public.merge(admin_routes)),
        sso,
        admin,
    }
}

struct Reply {
    status: u16,
    headers: HeaderMap,
    raw: String,
}

impl Reply {
    fn json(&self) -> Json {
        serde_json::from_str(&self.raw).unwrap_or(Json::Null)
    }
    fn cookies(&self) -> Vec<String> {
        self.headers
            .get_all(header::SET_COOKIE)
            .iter()
            .map(|v| v.to_str().unwrap().to_owned())
            .collect()
    }
    fn code(&self) -> String {
        self.json()["error"]["code"]
            .as_str()
            .unwrap_or("")
            .to_owned()
    }
}

async fn send_to(
    router: &Router,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Vec<u8>,
) -> Reply {
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, HOST);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let response = router
        .clone()
        .oneshot(builder.body(Body::from(body)).unwrap())
        .await
        .unwrap();
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 22)
        .await
        .unwrap();
    Reply {
        status,
        headers,
        raw: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

async fn send(
    f: &Fixture,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> Reply {
    send_to(&f.router, method, path, headers, body.as_bytes().to_vec()).await
}

const FORM: (&str, &str) = ("content-type", "application/x-www-form-urlencoded");
const JSON_CT: (&str, &str) = ("content-type", "application/json");
const BEARER: (&str, &str) = ("authorization", "Bearer admin-token");
const COOKIE: (&str, &str) = ("cookie", "__Host-spot_sso_binding=bind-1");

fn assert_sso_headers(r: &Reply) {
    assert_eq!(r.headers.get(header::CACHE_CONTROL).unwrap(), "no-store");
    assert_eq!(
        r.headers.get(header::REFERRER_POLICY).unwrap(),
        "no-referrer"
    );
}

// --- public group -----------------------------------------------------------------------------

#[tokio::test]
async fn every_public_operation_is_mounted_and_carries_the_fixed_headers() {
    let f = fixture();
    let r = send(&f, "GET", "/api/v1/auth/sso/providers", &[], "").await;
    assert_eq!(r.status, 200, "{}", r.raw);
    assert_eq!(r.json()["data"]["providers"][0]["id"], "okta");
    assert_eq!(r.json()["data"]["password_login"], true);
    assert_sso_headers(&r);

    let r = send(
        &f,
        "POST",
        "/api/v1/auth/sso/okta/begin",
        &[JSON_CT],
        r#"{"return_to":"/desk"}"#,
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.raw);
    assert_sso_headers(&r);

    let r = send(
        &f,
        "GET",
        "/api/v1/auth/sso/okta/callback?code=c&state=s",
        &[COOKIE],
        "",
    )
    .await;
    assert_eq!(r.status, 303, "{}", r.raw);
    assert_sso_headers(&r);

    let r = send(
        &f,
        "POST",
        "/api/v1/auth/sso/saml/acs",
        &[FORM, COOKIE],
        "SAMLResponse=abc&RelayState=r",
    )
    .await;
    assert_eq!(r.status, 303, "{}", r.raw);
    assert_sso_headers(&r);

    let r = send(
        &f,
        "POST",
        "/api/v1/auth/sso/complete",
        &[("cookie", "__Host-spot_sso_handover=hand-1")],
        "",
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.raw);
    assert_sso_headers(&r);

    let r = send(
        &f,
        "POST",
        "/api/v1/auth/sso/okta/backchannel-logout",
        &[FORM],
        "logout_token=t",
    )
    .await;
    assert_eq!(r.status, 200);
    assert!(r.raw.is_empty());
    assert_sso_headers(&r);

    let r = send(
        &f,
        "GET",
        "/api/v1/auth/sso/saml/slo?SAMLRequest=abc",
        &[],
        "",
    )
    .await;
    assert_eq!(r.status, 303);
    assert_eq!(
        r.headers.get(header::LOCATION).unwrap(),
        "https://idp.example.com/slo"
    );
    assert_sso_headers(&r);

    let r = send(
        &f,
        "POST",
        "/api/v1/auth/sso/saml/slo",
        &[FORM],
        "SAMLResponse=abc",
    )
    .await;
    assert_eq!(r.status, 303);

    let r = send(&f, "GET", "/api/v1/auth/sso/saml/saml-metadata", &[], "").await;
    assert_eq!(r.status, 200);
    assert_eq!(
        r.headers.get(header::CONTENT_TYPE).unwrap(),
        "application/samlmetadata+xml"
    );
    assert_sso_headers(&r);
}

#[tokio::test]
async fn begin_sets_the_binding_cookie_byte_for_byte() {
    let f = fixture();
    let r = send(&f, "POST", "/api/v1/auth/sso/okta/begin", &[JSON_CT], "{}").await;
    assert_eq!(
        r.cookies(),
        ["__Host-spot_sso_binding=bind-1; Max-Age=600; Path=/; Secure; HttpOnly; SameSite=Lax"]
    );
    assert_eq!(
        r.json()["data"]["redirect_url"],
        "https://idp.example.com/authorize?x=1"
    );
}

#[tokio::test]
async fn a_return_to_that_is_not_a_relative_path_never_reaches_the_port_or_a_location() {
    let f = fixture();
    for bad in [
        "https://evil.example/x",
        "//evil.example",
        "/\\\\evil.example",
        "evil",
        "/",
        "/a b",
    ] {
        let body = json!({"return_to": bad}).to_string();
        let r = send(&f, "POST", "/api/v1/auth/sso/okta/begin", &[JSON_CT], &body).await;
        assert_eq!(r.status, 400, "{bad}: {}", r.raw);
        assert_eq!(r.code(), "sso.request_invalid");
        assert!(r.headers.get(header::LOCATION).is_none());
    }
    assert!(
        f.sso.calls().is_empty(),
        "the port was called: {:?}",
        f.sso.calls()
    );
}

#[tokio::test]
async fn a_port_redirect_that_is_not_https_is_not_followed() {
    let f = fixture();
    *f.sso.redirect.lock().unwrap() = Some("javascript:alert(1)".into());
    let r = send(&f, "POST", "/api/v1/auth/sso/okta/begin", &[JSON_CT], "{}").await;
    assert_eq!(r.status, 500);
    assert!(r.cookies().is_empty());
    *f.sso.location.lock().unwrap() = Some("https://evil.example/".into());
    let r = send(
        &f,
        "GET",
        "/api/v1/auth/sso/okta/callback?code=c&state=s",
        &[COOKIE],
        "",
    )
    .await;
    assert_eq!(r.status, 500);
    assert!(r.headers.get(header::LOCATION).is_none());
}

#[tokio::test]
async fn begin_refuses_unknown_fields_a_wrong_content_type_and_a_large_body() {
    let f = fixture();
    let r = send(
        &f,
        "POST",
        "/api/v1/auth/sso/okta/begin",
        &[JSON_CT],
        r#"{"other":1}"#,
    )
    .await;
    assert_eq!(r.code(), "sso.request_invalid");
    let r = send(
        &f,
        "POST",
        "/api/v1/auth/sso/okta/begin",
        &[FORM],
        "return_to=%2Fa",
    )
    .await;
    assert_eq!(r.status, 415);
    let big = format!("{{\"return_to\":\"/{}\"}}", "a".repeat(70_000));
    let r = send(&f, "POST", "/api/v1/auth/sso/okta/begin", &[JSON_CT], &big).await;
    assert_eq!(r.status, 413);
    assert!(f.sso.calls().is_empty());
}

#[tokio::test]
async fn the_callback_without_exactly_one_binding_cookie_is_refused_before_the_port() {
    let f = fixture();
    let r = send(
        &f,
        "GET",
        "/api/v1/auth/sso/okta/callback?code=c&state=s",
        &[],
        "",
    )
    .await;
    assert_eq!(r.status, 400);
    assert_eq!(r.code(), "sso.state_invalid");
    let two = [(
        "cookie",
        "__Host-spot_sso_binding=a; __Host-spot_sso_binding=b",
    )];
    let r = send(
        &f,
        "GET",
        "/api/v1/auth/sso/okta/callback?code=c&state=s",
        &two,
        "",
    )
    .await;
    assert_eq!(r.code(), "sso.state_invalid");
    assert!(f.sso.calls().is_empty());
}

#[tokio::test]
async fn the_callback_sets_the_handover_cookie_and_clears_the_binding_one() {
    let f = fixture();
    let r = send(
        &f,
        "GET",
        "/api/v1/auth/sso/okta/callback?code=c&state=s",
        &[COOKIE],
        "",
    )
    .await;
    assert_eq!(r.status, 303);
    assert_eq!(r.headers.get(header::LOCATION).unwrap(), "/desk");
    assert_eq!(
        r.cookies(),
        [
            "__Host-spot_sso_handover=hand-1; Max-Age=60; Path=/; Secure; HttpOnly; SameSite=Lax",
            "__Host-spot_sso_binding=; Max-Age=0; Path=/; Secure; HttpOnly; SameSite=Lax",
        ]
    );
    assert_eq!(f.sso.calls(), ["callback okta binding=bind-1"]);
}

#[tokio::test]
async fn duplicate_and_unknown_parameters_are_refused() {
    let f = fixture();
    for path in [
        "/api/v1/auth/sso/okta/callback?code=a&code=b",
        "/api/v1/auth/sso/okta/callback?code=a&other=b",
        "/api/v1/auth/sso/okta/callback?state=%zz",
    ] {
        let r = send(&f, "GET", path, &[COOKIE], "").await;
        assert_eq!(r.status, 400, "{path}");
        assert_eq!(r.code(), "sso.request_invalid");
    }
    let r = send(
        &f,
        "POST",
        "/api/v1/auth/sso/saml/acs",
        &[FORM, COOKIE],
        "SAMLResponse=a&SAMLResponse=b",
    )
    .await;
    assert_eq!(r.code(), "sso.request_invalid");
    let r = send(
        &f,
        "POST",
        "/api/v1/auth/sso/saml/acs",
        &[FORM, COOKIE],
        "RelayState=r",
    )
    .await;
    assert_eq!(r.code(), "sso.request_invalid");
    let r = send(
        &f,
        "POST",
        "/api/v1/auth/sso/saml/acs",
        &[COOKIE],
        "SAMLResponse=a",
    )
    .await;
    assert_eq!(r.status, 415);
    assert!(f.sso.calls().is_empty());
}

#[tokio::test]
async fn a_form_body_is_capped_at_64_kib() {
    let f = fixture();
    let body = format!("SAMLResponse={}", "a".repeat(70_000));
    let r = send(
        &f,
        "POST",
        "/api/v1/auth/sso/saml/acs",
        &[FORM, COOKIE],
        &body,
    )
    .await;
    assert_eq!(r.status, 413);
    assert!(f.sso.calls().is_empty());
}

#[tokio::test]
async fn complete_answers_the_session_once_with_no_cookie_and_needs_the_handover_cookie() {
    let f = fixture();
    let r = send(&f, "POST", "/api/v1/auth/sso/complete", &[], "").await;
    assert_eq!(r.status, 400);
    assert_eq!(r.code(), "sso.state_invalid");
    let r = send(
        &f,
        "POST",
        "/api/v1/auth/sso/complete",
        &[("cookie", "__Host-spot_sso_handover=hand-1")],
        "x",
    )
    .await;
    assert_eq!(r.code(), "sso.request_invalid");
    assert!(f.sso.calls().is_empty());
    let r = send(
        &f,
        "POST",
        "/api/v1/auth/sso/complete",
        &[("cookie", "__Host-spot_sso_handover=hand-1")],
        "",
    )
    .await;
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["data"]["token"], "tok-secret");
    assert!(r.cookies().is_empty(), "the session is not a cookie");
}

#[tokio::test]
async fn a_refusal_is_a_stable_code_with_the_generic_message() {
    let f = fixture();
    let cases = [
        (SsoFailure::SignInFailed, 401, "sso.sign_in_failed"),
        (SsoFailure::NotAllowed, 403, "sso.not_allowed"),
        (SsoFailure::StateInvalid, 400, "sso.state_invalid"),
        (SsoFailure::ProviderNotFound, 404, "sso.provider_not_found"),
        (
            SsoFailure::ProviderUnavailable,
            503,
            "sso.provider_unavailable",
        ),
    ];
    for (kind, status, code) in cases {
        *f.sso.fail_with.lock().unwrap() = Some(kind);
        let r = send(
            &f,
            "GET",
            "/api/v1/auth/sso/okta/callback?code=c&state=s",
            &[COOKIE],
            "",
        )
        .await;
        assert_eq!(r.status, status, "{code}");
        assert_eq!(r.code(), code);
        assert_eq!(
            r.json()["error"]["message"],
            Code::from_code_str(code).unwrap().default_message()
        );
        assert_sso_headers(&r);
        assert!(
            r.cookies()
                .iter()
                .any(|c| c.starts_with("__Host-spot_sso_binding=;")),
            "the binding cookie is single use"
        );
    }
    *f.sso.fail_with.lock().unwrap() = Some(SsoFailure::LogoutRejected);
    let r = send(
        &f,
        "POST",
        "/api/v1/auth/sso/okta/backchannel-logout",
        &[FORM],
        "logout_token=t",
    )
    .await;
    assert_eq!((r.status, r.code().as_str()), (400, "sso.logout_rejected"));
    let r = send(
        &f,
        "POST",
        "/api/v1/auth/sso/okta/backchannel-logout",
        &[FORM],
        "",
    )
    .await;
    assert_eq!(r.code(), "sso.logout_rejected");
}

#[tokio::test]
async fn a_provider_id_that_is_not_visible_ascii_is_a_not_found() {
    let f = fixture();
    let long = "a".repeat(201);
    let r = send(
        &f,
        "POST",
        &format!("/api/v1/auth/sso/{long}/begin"),
        &[JSON_CT],
        "{}",
    )
    .await;
    assert_eq!(r.code(), "sso.provider_not_found");
    assert!(f.sso.calls().is_empty());
}

#[tokio::test]
async fn single_logout_needs_exactly_one_of_request_and_response() {
    let f = fixture();
    let r = send(&f, "GET", "/api/v1/auth/sso/saml/slo", &[], "").await;
    assert_eq!(r.code(), "sso.logout_rejected");
    let r = send(
        &f,
        "GET",
        "/api/v1/auth/sso/saml/slo?SAMLRequest=a&SAMLResponse=b",
        &[],
        "",
    )
    .await;
    assert_eq!(r.code(), "sso.logout_rejected");
    assert!(f.sso.calls().is_empty());
}

#[tokio::test]
async fn the_group_is_not_mounted_without_its_port() {
    let ports = Ports::new();
    let mounted = mount(&[sso_registration()], &ports, &[]);
    assert!(mounted.report.mounted.is_empty());
    assert_eq!(mounted.report.skipped.len(), 1);
    let mounted = mount(&[admin_registration()], &ports, &[]);
    assert!(mounted.report.mounted.is_empty());
    // Mounted with the port, public and admin as the registry sees them.
    let mut ports = Ports::new();
    ports.provide::<dyn SsoPort>(Arc::new(FakeSso::default()));
    ports.provide::<dyn SsoAdminPort>(Arc::new(FakeAdmin::default()));
    let mounted = mount(&[sso_registration(), admin_registration()], &ports, &[]);
    assert_eq!(mounted.report.public, ["sso"]);
    assert_eq!(mounted.report.mounted, ["sso", "sso_admin"]);
    assert_eq!(mounted.report.served.len(), 17);
}

#[path = "sso_routes/admin.rs"]
mod admin;
