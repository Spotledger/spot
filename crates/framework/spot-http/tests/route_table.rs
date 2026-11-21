//! The route table and the router agree (ADR-083 section 2.3, Barriers 1 and 2): every table
//! entry is mounted exactly as marked, a route not in the table does not exist, and the table
//! agrees with `contracts/native-api/openapi.yaml`.
#![allow(clippy::unwrap_used, clippy::expect_used, dead_code)]

#[allow(dead_code)]
mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use spot_core::{Audience, ManualClock};
use spot_http::api::idempotency::MemoryIdempotencyStore;
use spot_http::api::router::routes::{REGISTRY, Registration};
use spot_http::api::{
    ApiServices, HAND_WIRED_SERVES, RouteAuth, RouteMark, has_hand_wired_handler, route_table,
    router, router_for, served_operations,
};
use support::fake::FakeApp;
use tower::ServiceExt as _;

const NO_HANDLER: &str = "contract operation no handler on the router serves yet";
const UNREGISTERED: &str =
    "route group file exists but is not in the registry yet; the host mounts it itself";
const HEALTH: &str = "served by server.rs (health routes)";
const REALTIME: &str = "served by spot-realtime through site_routes";

/// Every contract operation the table does not list, with the reason. Adding a contract
/// operation without deciding (a table entry or a row here) fails
/// `the_table_agrees_with_the_contract`.
const NOT_YET_ROUTED: &[(&str, &str)] = &[
    ("health_live", HEALTH),
    ("health_ready", HEALTH),
    ("form_event", NO_HANDLER),
    ("auth_password_change", NO_HANDLER),
    ("export_start", UNREGISTERED),
    ("export_next", UNREGISTERED),
    ("export_extract", UNREGISTERED),
    ("realtime_connect", REALTIME),
    ("connector_inbound", NO_HANDLER),
    ("auth_totp_enrol", NO_HANDLER),
    ("auth_totp_confirm", NO_HANDLER),
    ("auth_totp_disable", NO_HANDLER),
    ("auth_recovery_codes_regenerate", NO_HANDLER),
    ("governance_grant_request_list", NO_HANDLER),
    ("governance_grant_request_create", NO_HANDLER),
    ("governance_grant_request_decide", NO_HANDLER),
    ("governance_sod_rules_get", NO_HANDLER),
    ("governance_sod_rules_put", NO_HANDLER),
    ("governance_sod_analysis", NO_HANDLER),
    ("governance_sod_override_list", NO_HANDLER),
    ("governance_sod_override_create", NO_HANDLER),
    ("governance_review_list", NO_HANDLER),
    ("governance_review_create", NO_HANDLER),
    ("governance_review_get", NO_HANDLER),
    ("governance_review_items", NO_HANDLER),
    ("governance_review_item_decide", NO_HANDLER),
    ("governance_review_close", NO_HANDLER),
    ("governance_review_report", NO_HANDLER),
    ("governance_question_user_permissions", NO_HANDLER),
    ("governance_question_who_can", NO_HANDLER),
    ("governance_question_who_can_see", NO_HANDLER),
    ("governance_question_changes", NO_HANDLER),
    ("governance_emergency_create", NO_HANDLER),
    ("governance_emergency_list", NO_HANDLER),
    ("governance_emergency_review", NO_HANDLER),
    ("security_data_policy_get", NO_HANDLER),
    ("security_data_policy_put", NO_HANDLER),
    ("security_retention_run_list", NO_HANDLER),
    ("security_retention_run_create", NO_HANDLER),
    ("security_retention_hold_list", NO_HANDLER),
    ("security_retention_hold_create", NO_HANDLER),
    ("security_retention_hold_delete", NO_HANDLER),
    ("security_retention_proof_list", NO_HANDLER),
    ("security_residency_check", NO_HANDLER),
    ("security_protected_group_list", NO_HANDLER),
    ("security_protected_group_sync_class", NO_HANDLER),
    ("security_node_key_list", NO_HANDLER),
    ("security_node_key_revoke", NO_HANDLER),
    ("security_support_request_list", NO_HANDLER),
    ("security_support_request_consent", NO_HANDLER),
    ("security_support_request_revoke", NO_HANDLER),
    ("security_support_session_list", NO_HANDLER),
    ("security_support_session_recording", NO_HANDLER),
    ("security_audit_chain_verify", NO_HANDLER),
    ("security_audit_entries", NO_HANDLER),
    ("security_audit_export", NO_HANDLER),
    ("security_compliance_evidence_pack", NO_HANDLER),
    ("security_compliance_controls", NO_HANDLER),
];

fn services() -> ApiServices {
    let clock = Arc::new(ManualClock::new(time::OffsetDateTime::now_utc()));
    let store = Arc::new(MemoryIdempotencyStore::new(clock.clone()));
    let fake = FakeApp::new(clock);
    ApiServices::new(fake.clone(), fake.clone(), fake.clone(), fake, store)
}

/// The path with each `{segment}` replaced by a sample value.
fn sample(path: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    for c in path.chars() {
        match c {
            '{' => {
                inside = true;
                out.push_str("sample");
            }
            '}' => inside = false,
            _ if inside => {}
            _ => out.push(c),
        }
    }
    out
}

/// The response's status and whether the body is non-empty. The router's own 404 and 405 have
/// an empty body; a handler or a middleware answers with an envelope.
async fn call(router: &Router, method: &str, path: &str) -> (StatusCode, bool) {
    let request = Request::builder()
        .method(method)
        .uri(sample(path))
        .header("authorization", "Bearer test-token")
        .body(Body::empty())
        .unwrap();
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, !bytes.is_empty())
}

fn reached(answer: (StatusCode, bool)) -> bool {
    answer.1
}

/// Operations of the registered groups (their ports are absent in this harness, so they are
/// not mounted here; their mount is the registry's own test).
fn registry_operations() -> BTreeSet<&'static str> {
    REGISTRY
        .iter()
        .flat_map(|r: &Registration| r.serves.iter().copied())
        // SCIM is served at /scim/v2 by its own server, not an operation of the native API.
        .filter(|op| *op != "scim_v2")
        .collect()
}

fn hand_wired_entries() -> impl Iterator<Item = &'static spot_http::api::RouteEntry> {
    route_table()
        .iter()
        .filter(|e| has_hand_wired_handler(e.operation))
}

#[test]
fn every_table_operation_has_a_handler_and_every_handler_a_table_entry() {
    let table_ops: BTreeSet<&str> = route_table().iter().map(|e| e.operation).collect();
    let registry = registry_operations();
    for op in &table_ops {
        assert!(
            has_hand_wired_handler(op) || registry.contains(op),
            "{op} is in the table but nothing serves it"
        );
        assert!(
            !(has_hand_wired_handler(op) && registry.contains(op)),
            "{op} is served twice"
        );
    }
    for op in HAND_WIRED_SERVES {
        assert!(
            table_ops.contains(op),
            "{op} has a handler and no table entry"
        );
        assert!(has_hand_wired_handler(op));
    }
    for op in &registry {
        assert!(
            table_ops.contains(op),
            "{op} is registered and not in the table"
        );
    }
    let mut served = served_operations();
    // SCIM is served at /scim/v2 by its own server, not an operation of the native API.
    served.remove("scim_v2");
    assert_eq!(
        served, table_ops,
        "served_operations() and the table must list the same operations"
    );
}

#[tokio::test]
async fn the_router_serves_exactly_the_hand_wired_entries() {
    let app = router(services());
    let table: BTreeMap<&str, BTreeSet<&str>> =
        route_table().iter().fold(BTreeMap::new(), |mut acc, e| {
            acc.entry(e.path).or_default().insert(e.method);
            acc
        });
    for entry in hand_wired_entries() {
        // `auth_permissions` and `field_reveal` need their optional service, which the harness
        // does not wire: they are absent here by design.
        if matches!(entry.operation, "auth_permissions" | "field_reveal") {
            continue;
        }
        let answer = call(&app, entry.method, entry.path).await;
        assert!(
            reached(answer),
            "{} {} did not reach a handler or middleware: {answer:?}",
            entry.method,
            entry.path
        );
        // Every method the table does not list for this path is not served.
        for other in ["GET", "POST", "PUT", "PATCH", "DELETE"] {
            if table[entry.path].contains(other) {
                continue;
            }
            let (status, _) = call(&app, other, entry.path).await;
            assert!(
                status == StatusCode::METHOD_NOT_ALLOWED || status.is_client_error(),
                "{other} {} answered {status}",
                entry.path
            );
            assert!(
                !status.is_success() && !status.is_server_error(),
                "{other} {}",
                entry.path
            );
        }
    }
    // A path in no entry is absent.
    for path in [
        "/api/v1/nothing",
        "/api/v1/auth/sessions/x/y",
        "/api//v1/auth/session",
        "/api/v1/auth/session/",
        "/API/v1/auth/session",
    ] {
        let (status, body) = call(&app, "GET", path).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        assert!(!body, "{path}");
    }
}

#[tokio::test]
async fn a_consumer_router_has_no_table_route() {
    let app = router_for(&[Audience::Consumer], services());
    for entry in route_table() {
        let (status, body) = call(&app, entry.method, entry.path).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "{} {}",
            entry.method,
            entry.path
        );
        assert!(!body);
    }
}

#[tokio::test]
async fn no_audience_mounts_nothing() {
    let app = router_for(&[], services());
    for entry in route_table() {
        let (status, _) = call(&app, entry.method, entry.path).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}

#[tokio::test]
async fn staff_and_partner_routers_mount_what_is_marked_and_the_union_is_the_router() {
    let staff = router_for(&[Audience::Staff], services());
    let partner = router_for(&[Audience::Partner], services());
    let both = router(services());
    for entry in hand_wired_entries() {
        if matches!(entry.operation, "auth_permissions" | "field_reveal") {
            continue;
        }
        for (audience, app) in [(Audience::Staff, &staff), (Audience::Partner, &partner)] {
            let marked = entry.audiences.contains(&audience);
            let answer = call(app, entry.method, entry.path).await;
            assert_eq!(
                reached(answer),
                marked,
                "{} {} for {audience}",
                entry.method,
                entry.path
            );
        }
        assert!(reached(call(&both, entry.method, entry.path).await));
    }
}

#[tokio::test]
async fn registered_groups_are_filtered_by_audience() {
    let fake = |serves: &'static [&'static str]| Registration {
        name: "fake",
        serves,
        required_ports: || &[],
        routes: Router::new,
        public: None,
    };
    // Only the table decides: a group serving an unlisted operation is never mounted, a
    // listed one only for an audience its entries allow.
    let (_, report) = spot_http::api::router::router_with(services(), &[fake(&["report_list"])]);
    assert_eq!(report.mounted, vec!["fake"]);
    assert!(spot_http::api::routes::group_allowed(
        &["report_list"],
        &[Audience::Staff]
    ));
    assert!(!spot_http::api::routes::group_allowed(
        &["report_list"],
        &[Audience::Consumer]
    ));
    assert!(!spot_http::api::routes::group_allowed(
        &["fake_unlisted"],
        &Audience::ALL
    ));
}

#[tokio::test]
async fn the_mark_is_stamped_on_every_table_route_and_on_nothing_else() {
    let app = router(services());
    for entry in hand_wired_entries() {
        if matches!(entry.operation, "auth_permissions" | "field_reveal") {
            continue;
        }
        let request = Request::builder()
            .method(entry.method)
            .uri(sample(entry.path))
            .header("authorization", "Bearer test-token")
            .body(Body::empty())
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        let mark = response.extensions().get::<RouteMark>().copied();
        assert_eq!(
            mark.map(|m| (m.operation, m.audiences, m.auth)),
            Some((entry.operation, entry.audiences, entry.auth)),
            "{} {}",
            entry.method,
            entry.path
        );
    }
    let request = Request::builder()
        .uri("/api/v1/unknown")
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert!(response.extensions().get::<RouteMark>().is_none());
}

#[test]
fn only_the_login_and_the_renewal_have_no_auth() {
    // The operations the contract marks `security: []`: the login and the renewal, the process
    // webhook (its own signature), the invitation accept, the external-link inbound and the
    // public single-sign-on sign-in group.
    const OPEN: &[&str] = &[
        "auth_consumer_renew",
        "auth_invitation_accept",
        "auth_login",
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
    for entry in route_table() {
        let open = OPEN.contains(&entry.operation);
        assert_eq!(entry.auth == RouteAuth::None, open, "{}", entry.operation);
    }
}
/// `(operationId, method, path)` of the contract, by line scanning (the file's `paths:` shape is
/// regular: a path at two spaces, a method at four, `operationId` at six).
fn contract_operations() -> Vec<(String, String, String)> {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../contracts/native-api/openapi.yaml"
    );
    let text = std::fs::read_to_string(path).unwrap();
    let mut out = Vec::new();
    let mut in_paths = false;
    let mut current = String::new();
    let mut method = String::new();
    for line in text.lines() {
        if line == "paths:" {
            in_paths = true;
            continue;
        }
        if !in_paths {
            continue;
        }
        if !line.is_empty() && !line.starts_with(' ') && !line.starts_with('#') {
            break;
        }
        if let Some(rest) = line.strip_prefix("  /")
            && let Some(path) = rest.strip_suffix(':')
        {
            current = format!("/{path}");
            continue;
        }
        if let Some(rest) = line.strip_prefix("    ")
            && let Some(name) = rest.strip_suffix(':')
            && matches!(name, "get" | "put" | "post" | "patch" | "delete")
        {
            method = name.to_ascii_uppercase();
            continue;
        }
        if let Some(id) = line.strip_prefix("      operationId: ") {
            out.push((id.trim().to_owned(), method.clone(), current.clone()));
        }
    }
    out
}

#[test]
fn the_table_agrees_with_the_contract() {
    let contract = contract_operations();
    assert!(
        contract.len() > 100,
        "the scan found {} operations",
        contract.len()
    );
    let by_id: BTreeMap<&str, (&str, &str)> = contract
        .iter()
        .map(|(id, m, p)| (id.as_str(), (m.as_str(), p.as_str())))
        .collect();
    // Every table entry is a contract operation with the same method and path (the contract has
    // two methods under one `operationId`-per-method, so the id is unique).
    for entry in route_table() {
        let (method, path) = by_id
            .get(entry.operation)
            .unwrap_or_else(|| panic!("{} is not a contract operation", entry.operation));
        assert_eq!(*method, entry.method, "{}", entry.operation);
        assert_eq!(*path, entry.path, "{}", entry.operation);
    }
    // Every contract operation is in the table or listed, with a reason, and not both.
    let table: BTreeSet<&str> = route_table().iter().map(|e| e.operation).collect();
    let pending: BTreeMap<&str, &str> = NOT_YET_ROUTED.iter().copied().collect();
    assert_eq!(
        pending.len(),
        NOT_YET_ROUTED.len(),
        "a duplicate row in NOT_YET_ROUTED"
    );
    for (id, _, _) in &contract {
        let in_table = table.contains(id.as_str());
        let listed = pending.contains_key(id.as_str());
        assert!(
            in_table != listed,
            "{id}: decide it: a table entry or a NOT_YET_ROUTED row (in table: {in_table}, listed: {listed})"
        );
    }
    for id in pending.keys() {
        assert!(
            by_id.contains_key(id),
            "{id} is listed but not in the contract"
        );
    }
    for reason in pending.values() {
        assert!(!reason.is_empty());
    }
}

/// `auth_step_up` (WP-IAM-12.T7) is served by the registered `step_up` group: the table lists it
/// for staff and partner behind the bearer layer, and the served set agrees.
#[test]
fn auth_step_up_is_a_table_entry_served_by_the_step_up_group() {
    let entry = route_table()
        .iter()
        .find(|e| e.operation == "auth_step_up")
        .expect("auth_step_up is in the table");
    assert_eq!(entry.method, "POST");
    assert_eq!(entry.path, "/api/v1/auth/step-up");
    assert_eq!(entry.auth, RouteAuth::Required);
    assert!(entry.audiences.contains(&Audience::Staff));
    assert!(entry.audiences.contains(&Audience::Partner));
    assert!(!entry.audiences.contains(&Audience::Consumer));
    assert!(served_operations().contains("auth_step_up"));
    assert!(registry_operations().contains("auth_step_up"));
    assert!(!has_hand_wired_handler("auth_step_up"));
}

/// `resource_ai_timeline` (WP-AI-11.T11) is served by the registered `ai_timeline` group: the
/// table lists it for staff and partner behind the bearer layer (never a consumer), and the
/// served set agrees.
#[test]
fn resource_ai_timeline_is_a_table_entry_served_by_the_ai_timeline_group() {
    let entry = route_table()
        .iter()
        .find(|e| e.operation == "resource_ai_timeline")
        .expect("resource_ai_timeline is in the table");
    assert_eq!(entry.method, "GET");
    assert_eq!(entry.path, "/api/v1/resource/{doctype}/{id}/ai-timeline");
    assert_eq!(entry.auth, RouteAuth::Required);
    assert!(entry.audiences.contains(&Audience::Staff));
    assert!(entry.audiences.contains(&Audience::Partner));
    assert!(!entry.audiences.contains(&Audience::Consumer));
    assert!(served_operations().contains("resource_ai_timeline"));
    assert!(registry_operations().contains("resource_ai_timeline"));
    assert!(!has_hand_wired_handler("resource_ai_timeline"));
}
