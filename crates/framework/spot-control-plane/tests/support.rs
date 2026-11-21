//! The `support` module: grants arrive only from the tenant host, sessions only against an active
//! grant, every step is audited, and the customer's revocation is immediate. The in-memory store
//! here implements the module's `SupportStore`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use axum::body::{Body, Bytes};
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use spot_control_plane::auth::{
    HostIdentity, OperatorCall, RequestId, ScopedTenants, TenantScope, VerifiedCommand,
};
use spot_control_plane::modules::support::{
    Grant, Policy, Session, SupportService, SupportStore, host_router, operator_router,
};
use spot_control_plane::{CpCode, CpError};
use spot_core::{Clock, ManualClock};
use spot_mgmt_api::ports::{AuditOutcome, AuditSink, BoxFuture, MemoryAuditSink};
use tower::ServiceExt as _;

const NOW: i64 = 1_800_000_000;

fn store_failed() -> CpError {
    CpError::with_reason(CpCode::Internal, "test.store_failed")
}

/// An in-memory store: the tests' backend.
#[derive(Default)]
struct MemoryStore {
    grants: Mutex<BTreeMap<(String, String), Grant>>,
    sessions: Mutex<Vec<Session>>,
}

impl SupportStore for MemoryStore {
    fn insert_grant<'a>(&'a self, grant: &'a Grant) -> BoxFuture<'a, Result<bool, CpError>> {
        Box::pin(async move {
            let mut all = self.grants.lock().map_err(|_| store_failed())?;
            let key = (grant.tenant.clone(), grant.id.clone());
            if all.contains_key(&key) {
                return Ok(false);
            }
            all.insert(key, grant.clone());
            Ok(true)
        })
    }

    fn get_grant<'a>(
        &'a self,
        tenant: &'a str,
        id: &'a str,
    ) -> BoxFuture<'a, Result<Option<Grant>, CpError>> {
        Box::pin(async move {
            let all = self.grants.lock().map_err(|_| store_failed())?;
            Ok(all.get(&(tenant.to_owned(), id.to_owned())).cloned())
        })
    }

    fn list_grants(&self) -> BoxFuture<'_, Result<Vec<Grant>, CpError>> {
        Box::pin(async move {
            Ok(self
                .grants
                .lock()
                .map_err(|_| store_failed())?
                .values()
                .take(1_000)
                .cloned()
                .collect())
        })
    }

    fn revoke_grant<'a>(
        &'a self,
        tenant: &'a str,
        id: &'a str,
        by: &'a str,
        at: i64,
    ) -> BoxFuture<'a, Result<Option<Grant>, CpError>> {
        Box::pin(async move {
            let mut all = self.grants.lock().map_err(|_| store_failed())?;
            let Some(grant) = all.get_mut(&(tenant.to_owned(), id.to_owned())) else {
                return Ok(None);
            };
            if grant.revoked_at.is_none() {
                grant.revoked_at = Some(at);
                grant.revoked_by = Some(by.to_owned());
            }
            let grant = grant.clone();
            drop(all);
            for session in self.sessions.lock().map_err(|_| store_failed())?.iter_mut() {
                if session.tenant == tenant && session.grant_id == id && session.closed_at.is_none()
                {
                    session.closed_at = Some(at);
                }
            }
            Ok(Some(grant))
        })
    }

    fn open_session<'a>(
        &'a self,
        session: &'a Session,
        max_open: usize,
        now: i64,
    ) -> BoxFuture<'a, Result<bool, CpError>> {
        Box::pin(async move {
            let mut all = self.sessions.lock().map_err(|_| store_failed())?;
            let open = all
                .iter()
                .filter(|s| {
                    s.tenant == session.tenant
                        && s.grant_id == session.grant_id
                        && s.closed_at.is_none()
                        && s.expires_at > now
                })
                .count();
            if open >= max_open {
                return Ok(false);
            }
            all.push(session.clone());
            Ok(true)
        })
    }

    fn close_session<'a>(
        &'a self,
        tenant: &'a str,
        grant_id: &'a str,
        id: &'a str,
        at: i64,
    ) -> BoxFuture<'a, Result<bool, CpError>> {
        Box::pin(async move {
            let mut all = self.sessions.lock().map_err(|_| store_failed())?;
            match all.iter_mut().find(|s| {
                s.tenant == tenant && s.grant_id == grant_id && s.id == id && s.closed_at.is_none()
            }) {
                Some(session) => {
                    session.closed_at = Some(at);
                    Ok(true)
                }
                None => Ok(false),
            }
        })
    }

    fn sessions<'a>(
        &'a self,
        tenant: &'a str,
        grant_id: &'a str,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<Session>, CpError>> {
        Box::pin(async move {
            let all = self.sessions.lock().map_err(|_| store_failed())?;
            Ok(all
                .iter()
                .rev()
                .filter(|s| s.tenant == tenant && s.grant_id == grant_id)
                .take(limit)
                .cloned()
                .collect())
        })
    }
}

struct Rig {
    service: Arc<SupportService>,
    audit: Arc<MemoryAuditSink>,
    clock: Arc<ManualClock>,
}

fn rig() -> Rig {
    let audit = Arc::new(MemoryAuditSink::new());
    let clock = Arc::new(ManualClock::new(
        time::OffsetDateTime::from_unix_timestamp(NOW).unwrap(),
    ));
    let service = Arc::new(SupportService::new(
        Arc::new(MemoryStore::default()),
        clock.clone() as Arc<dyn Clock>,
        audit.clone() as Arc<dyn AuditSink>,
        Policy::default(),
    ));
    Rig {
        service,
        audit,
        clock,
    }
}

fn host(tenant: &str) -> HostIdentity {
    HostIdentity {
        tenant: tenant.to_owned(),
        host_id: "h1".to_owned(),
        fingerprint: "fp".to_owned(),
    }
}

fn operator(tenants: &[&str], command: bool) -> OperatorCall {
    let tenants: Vec<String> = tenants.iter().map(|t| (*t).to_owned()).collect();
    OperatorCall {
        principal: "alice-ops".to_owned(),
        scope: ScopedTenants::new(TenantScope::from_config(&tenants)),
        command: command.then(|| VerifiedCommand {
            id: "cmd-1".to_owned(),
            op: "cp.support_session_open".to_owned(),
            approvals: Vec::new(),
        }),
        request_id: "req-1".to_owned(),
    }
}

fn grant_body(id: &str, write: bool, starts: i64, ends: i64) -> Bytes {
    Bytes::from(
        json!({
            "id": id,
            "doctypes": ["Invoice", "Customer"],
            "write": write,
            "starts_at": starts,
            "expires_at": ends,
            "approver": "customer-owner",
            "record": "{\"signed\":\"record\"}",
            "signature": "c2ln",
        })
        .to_string(),
    )
}

fn open_body(reason: &str, write: bool) -> Bytes {
    Bytes::from(json!({"reason": reason, "write": write}).to_string())
}

async fn receive(rig: &Rig, tenant: &str, id: &str, write: bool) {
    rig.service
        .receive_grant(
            &host(tenant),
            "req-h",
            &grant_body(id, write, NOW, NOW + 3600),
        )
        .await
        .expect("received");
}

fn code(result: Result<Value, CpError>) -> (CpCode, &'static str) {
    let error = result.expect_err("a refusal");
    (error.code, error.reason)
}

#[tokio::test]
async fn no_grant_no_session() {
    let rig = rig();
    let call = operator(&["*"], true);
    let refused = rig
        .service
        .open_session(&call, "acme", "g1", &open_body("ticket SUP-1", false))
        .await;
    assert_eq!(code(refused).0, CpCode::NotFound);
}

#[tokio::test]
async fn a_grant_from_the_host_is_listed_and_a_read_only_session_opens() {
    let rig = rig();
    receive(&rig, "acme", "g1", false).await;
    let call = operator(&["*"], true);
    let listed = rig.service.list_grants(&call).await.unwrap();
    assert_eq!(listed[0]["state"], "active");
    assert_eq!(listed[0]["scope"], json!(["Customer", "Invoice"]));
    // The operator's view never carries the signed record or the signature.
    let text = listed.to_string();
    assert!(!text.contains("signature") && !text.contains("record") && !text.contains("c2ln"));
    let session = rig
        .service
        .open_session(&call, "acme", "g1", &open_body("ticket SUP-1", false))
        .await
        .unwrap();
    assert_eq!(session["read_only"], true);
    assert_eq!(session["expires_at"], NOW + 3600);
    // The customer's host sees the session, for the audit trail it keeps.
    let seen = rig
        .service
        .host_sessions(&host("acme"), "g1")
        .await
        .unwrap();
    assert_eq!(seen[0]["reason"], "ticket SUP-1");
    // Another tenant's host sees nothing of it.
    assert_eq!(
        code(rig.service.host_sessions(&host("other"), "g1").await).0,
        CpCode::NotFound
    );
    let id = session["id"].as_str().unwrap().to_owned();
    rig.service
        .close_session(&call, "acme", "g1", &id)
        .await
        .unwrap();
    assert_eq!(
        code(rig.service.close_session(&call, "acme", "g1", &id).await).0,
        CpCode::NotFound
    );
}

#[tokio::test]
async fn an_expired_a_revoked_and_a_not_yet_started_grant_open_no_session() {
    let rig = rig();
    receive(&rig, "acme", "g1", false).await;
    rig.service
        .receive_grant(
            &host("acme"),
            "r",
            &grant_body("later", false, NOW + 600, NOW + 3600),
        )
        .await
        .unwrap();
    let call = operator(&["*"], true);
    assert_eq!(
        code(
            rig.service
                .open_session(&call, "acme", "later", &open_body("t", false))
                .await
        ),
        (CpCode::Forbidden, "support.grant_not_started")
    );
    rig.clock.advance(std::time::Duration::from_secs(3601));
    assert_eq!(
        code(
            rig.service
                .open_session(&call, "acme", "g1", &open_body("t", false))
                .await
        ),
        (CpCode::Forbidden, "support.grant_expired")
    );
    // A fresh grant, revoked by the customer.
    rig.service
        .receive_grant(
            &host("acme"),
            "r",
            &grant_body("g2", false, NOW + 3601, NOW + 7000),
        )
        .await
        .unwrap();
    rig.service
        .revoke_grant(&host("acme"), "r", "g2")
        .await
        .unwrap();
    assert_eq!(
        code(
            rig.service
                .open_session(&call, "acme", "g2", &open_body("t", false))
                .await
        ),
        (CpCode::Forbidden, "support.grant_revoked")
    );
}

#[tokio::test]
async fn customer_revocation_closes_open_sessions_at_once_and_is_final() {
    let rig = rig();
    receive(&rig, "acme", "g1", false).await;
    let call = operator(&["*"], true);
    rig.service
        .open_session(&call, "acme", "g1", &open_body("t1", false))
        .await
        .unwrap();
    rig.service
        .open_session(&call, "acme", "g1", &open_body("t2", false))
        .await
        .unwrap();
    rig.service
        .revoke_grant(&host("acme"), "r", "g1")
        .await
        .unwrap();
    let sessions = rig
        .service
        .list_sessions(&call, "acme", "g1")
        .await
        .unwrap();
    assert!(
        sessions
            .as_array()
            .unwrap()
            .iter()
            .all(|s| !s["closed_at"].is_null())
    );
    assert_eq!(
        code(
            rig.service
                .open_session(&call, "acme", "g1", &open_body("t3", false))
                .await
        )
        .0,
        CpCode::Forbidden
    );
    // Revoking again keeps the first revocation and is not an error.
    rig.service
        .revoke_grant(&host("acme"), "r", "g1")
        .await
        .unwrap();
    assert_eq!(
        rig.service.list_grants(&call).await.unwrap()[0]["state"],
        "revoked"
    );
    // Another tenant's host cannot revoke it.
    assert_eq!(
        code(rig.service.revoke_grant(&host("other"), "r", "g1").await).0,
        CpCode::NotFound
    );
}

#[tokio::test]
async fn the_operators_scope_hides_other_tenants_exactly_like_missing_ones() {
    let rig = rig();
    receive(&rig, "acme", "g1", false).await;
    receive(&rig, "beta", "g1", false).await;
    let call = operator(&["beta"], true);
    let listed = rig.service.list_grants(&call).await.unwrap();
    assert_eq!(listed.as_array().unwrap().len(), 1);
    assert_eq!(listed[0]["tenant"], "beta");
    let out = rig
        .service
        .open_session(&call, "acme", "g1", &open_body("t", false))
        .await;
    let missing = rig
        .service
        .open_session(&call, "acme", "nope", &open_body("t", false))
        .await;
    assert_eq!(code(out).0, code(missing).0);
    assert_eq!(
        code(rig.service.list_sessions(&call, "acme", "g1").await).0,
        CpCode::NotFound
    );
    assert_eq!(
        code(rig.service.close_session(&call, "acme", "g1", "x").await).0,
        CpCode::NotFound
    );
}

#[tokio::test]
async fn a_write_session_needs_a_write_grant_and_a_signed_command() {
    let rig = rig();
    receive(&rig, "acme", "ro", false).await;
    receive(&rig, "acme", "rw", true).await;
    let call = operator(&["*"], true);
    assert_eq!(
        code(
            rig.service
                .open_session(&call, "acme", "ro", &open_body("t", true))
                .await
        ),
        (CpCode::Forbidden, "support.write_not_granted")
    );
    let session = rig
        .service
        .open_session(&call, "acme", "rw", &open_body("t", true))
        .await
        .unwrap();
    assert_eq!(session["read_only"], false);
    // Read-only unless the request asks.
    let session = rig
        .service
        .open_session(&call, "acme", "rw", &open_body("t", false))
        .await
        .unwrap();
    assert_eq!(session["read_only"], true);
    // No verified command, no session.
    let unsigned = operator(&["*"], false);
    assert_eq!(
        code(
            rig.service
                .open_session(&unsigned, "acme", "rw", &open_body("t", false))
                .await
        ),
        (CpCode::Forbidden, "support.command_required")
    );
}

#[tokio::test]
async fn a_grant_is_validated_and_the_tenant_is_never_taken_from_the_body() {
    let rig = rig();
    let h = host("acme");
    let send = |body: Bytes| {
        let service = rig.service.clone();
        let h = h.clone();
        async move { service.receive_grant(&h, "r", &body).await }
    };
    // Longer than 24 hours, already ended, backdated, no scope, bad id.
    for body in [
        grant_body("g", false, NOW, NOW + 24 * 3600 + 1),
        grant_body("g", false, NOW - 7200, NOW - 3600),
        grant_body("g", false, NOW - 1000, NOW + 3600),
        grant_body("G!", false, NOW, NOW + 3600),
        Bytes::from(json!({"id":"g","doctypes":[],"starts_at":NOW,"expires_at":NOW+60,"approver":"a","record":"r","signature":"s"}).to_string()),
    ] {
        assert_eq!(code(send(body).await).0, CpCode::Invalid);
    }
    // A body that names a tenant (or any unknown field) is refused: the certificate decides.
    let mut with_tenant: Value =
        serde_json::from_slice(&grant_body("g", false, NOW, NOW + 3600)).unwrap();
    with_tenant["tenant"] = json!("beta");
    assert_eq!(
        code(send(Bytes::from(with_tenant.to_string())).await).0,
        CpCode::Invalid
    );
    assert_eq!(
        code(send(Bytes::from(vec![b' '; 40 * 1024])).await).0,
        CpCode::BodyTooLarge
    );
    // A good one is stored for the certificate's tenant, once.
    send(grant_body("g", false, NOW, NOW + 3600)).await.unwrap();
    assert_eq!(
        code(send(grant_body("g", false, NOW, NOW + 3600)).await).0,
        CpCode::Conflict
    );
    let listed = rig
        .service
        .list_grants(&operator(&["*"], true))
        .await
        .unwrap();
    assert_eq!(listed[0]["tenant"], "acme");
}

#[tokio::test]
async fn at_most_five_sessions_are_open_per_grant() {
    let rig = rig();
    receive(&rig, "acme", "g1", false).await;
    let call = operator(&["*"], true);
    for n in 0..5 {
        // A new command id per session gives a new session id.
        let mut c = call.clone();
        c.command.as_mut().unwrap().id = format!("cmd-{n}");
        rig.service
            .open_session(&c, "acme", "g1", &open_body("t", false))
            .await
            .unwrap();
    }
    let mut c = call.clone();
    c.command.as_mut().unwrap().id = "cmd-6".to_owned();
    assert_eq!(
        code(
            rig.service
                .open_session(&c, "acme", "g1", &open_body("t", false))
                .await
        ),
        (CpCode::Forbidden, "support.too_many_sessions")
    );
}

#[tokio::test]
async fn every_outcome_is_audited_and_nothing_happens_when_the_record_cannot_be_written() {
    let rig = rig();
    receive(&rig, "acme", "g1", false).await;
    let call = operator(&["*"], true);
    rig.service
        .open_session(&call, "acme", "g1", &open_body("t", false))
        .await
        .unwrap();
    let _ = rig
        .service
        .open_session(&call, "acme", "g1", &open_body("t", true))
        .await;
    let records = rig.audit.records();
    let actions: Vec<(&str, AuditOutcome)> = records
        .iter()
        .map(|r| (r.action.as_str(), r.outcome))
        .collect();
    assert!(actions.contains(&("support.grant_received", AuditOutcome::Succeeded)));
    assert!(actions.contains(&("support.session_open", AuditOutcome::Succeeded)));
    assert!(actions.contains(&("support.session_open", AuditOutcome::Refused)));
    assert!(
        records
            .iter()
            .all(|r| r.command_id.as_deref() != Some("secret"))
    );

    // A sink that always fails: the grant is not stored and no session opens.
    struct Broken;
    impl AuditSink for Broken {
        fn record(
            &self,
            _: spot_mgmt_api::ports::AuditRecord,
        ) -> BoxFuture<'_, Result<String, spot_mgmt_api::ports::AuditError>> {
            Box::pin(std::future::ready(Err(spot_mgmt_api::ports::AuditError)))
        }
    }
    let clock = Arc::new(ManualClock::new(
        time::OffsetDateTime::from_unix_timestamp(NOW).unwrap(),
    ));
    let service = SupportService::new(
        Arc::new(MemoryStore::default()),
        clock as Arc<dyn Clock>,
        Arc::new(Broken),
        Policy::default(),
    );
    let failed = service
        .receive_grant(&host("acme"), "r", &grant_body("g", false, NOW, NOW + 3600))
        .await;
    assert_eq!(code(failed).0, CpCode::Internal);
    assert!(
        service
            .list_grants(&operator(&["*"], true))
            .await
            .unwrap()
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn the_routes_exist_and_there_is_no_extend_route() {
    let rig = rig();
    let host_app = host_router(rig.service.clone())
        .layer(axum::Extension(host("acme")))
        .layer(axum::Extension(RequestId("req-h".to_owned())));
    let response = host_app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/cp/v1/host/support-grants")
                .body(Body::from(grant_body("g1", false, NOW, NOW + 3600)))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let operator_app =
        operator_router(rig.service.clone()).layer(axum::Extension(operator(&["*"], true)));
    for (method, uri) in [
        ("POST", "/cp/v1/support-grants/acme/g1/extend"),
        ("PUT", "/cp/v1/support-grants/acme/g1"),
        ("POST", "/cp/v1/support-grants"),
    ] {
        let response = operator_app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            response.status() == StatusCode::NOT_FOUND
                || response.status() == StatusCode::METHOD_NOT_ALLOWED,
            "{method} {uri} answered {}",
            response.status()
        );
    }
    let response = operator_app
        .oneshot(
            Request::builder()
                .uri("/cp/v1/support-grants")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}
