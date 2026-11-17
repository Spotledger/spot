//! The route table (ADR-083 section 2.3, Barriers 1 and 2): every route the native API router
//! can mount, with the audiences that may use it.
//!
//! **Unmarked means absent.** There is no default audience and no "unmarked" state: every
//! [`RouteEntry`] spells its audiences, and a route that is not in [`route_table`] is not
//! mounted by [`router_for`](super::router::router_for), whatever handler exists for it. The
//! consumer surface starts empty (no entry lists [`Audience::Consumer`]) and grows only by a
//! reviewed change to this file.
//!
//! The table is a flat static slice sorted by path, then method, one entry per method. Read it
//! in one place; there are no macros. `tests/route_table.rs` proves the table and the router
//! agree and that the table agrees with `contracts/native-api/openapi.yaml`.
//!
//! **At request time** the router stamps every matched request (and its response) with a
//! [`RouteMark`], looked up from the matched path pattern and the method. It is the only way
//! later code learns a route's audiences. A request that matches no entry gets no mark.
//!
//! **Not in this table.** Routes other crates mount through `ServerBuilder::site_routes`
//! (SCIM in `spot-iam-scim`, realtime in `spot-realtime`) and the health routes
//! (`/health/live`, `/health/ready`, served by `server.rs`) are staff-listener-only; the
//! per-audience listeners (WP-IAM-11.T3) never mount them on a consumer listener.

use axum::extract::{MatchedPath, Request};
use axum::middleware::Next;
use axum::response::Response;
use spot_core::Audience;

/// Whether a route needs a bearer credential.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteAuth {
    /// A bearer credential must be present (the bearer-presence middleware runs).
    Required,
    /// No credential (a login, or a cookie-only renewal).
    None,
}

/// One route of the native API.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RouteEntry {
    /// The contract `operationId`.
    pub operation: &'static str,
    /// One HTTP method (an operation with two methods on one path is two entries).
    pub method: &'static str,
    /// The axum path pattern, e.g. `/api/v1/resource/{doctype}/{id}`.
    pub path: &'static str,
    /// The audiences that may use the route; never empty.
    pub audiences: &'static [Audience],
    /// Whether a bearer credential is required.
    pub auth: RouteAuth,
}

/// A request extension the host sets on a function call that carries no credential, on a site
/// whose function service serves guest functions: the bearer layer then lets the call through to
/// the service, which decides it as an anonymous caller. Without it a missing bearer is `401`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GuestCall;

/// What the router stamps on a matched request (a request extension) and on its response.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RouteMark {
    /// The contract `operationId` of the matched route.
    pub operation: &'static str,
    /// The audiences the route is marked for.
    pub audiences: &'static [Audience],
    /// Whether the route requires a bearer credential.
    pub auth: RouteAuth,
}

/// Staff and partner share the ERP listener. Today a portal token reaches the same routes as a
/// staff one and the permission engine's party scoping limits it; marking narrower is a later
/// decision (ADR-083).
const STAFF_PARTNER: &[Audience] = &[Audience::Staff, Audience::Partner];

/// Every route, sorted by path then method.
static TABLE: [RouteEntry; 133] = [
    RouteEntry {
        operation: "app_list",
        method: "GET",
        path: "/api/v1/apps",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "auth_consumer_renew",
        method: "POST",
        path: "/api/v1/auth/consumer/renew",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::None,
    },
    RouteEntry {
        operation: "auth_invitation_accept",
        method: "POST",
        path: "/api/v1/auth/invitations/accept",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::None,
    },
    RouteEntry {
        operation: "auth_login",
        method: "POST",
        path: "/api/v1/auth/login",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::None,
    },
    RouteEntry {
        operation: "auth_logout",
        method: "POST",
        path: "/api/v1/auth/logout",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "auth_permissions",
        method: "GET",
        path: "/api/v1/auth/permissions",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "auth_session",
        method: "GET",
        path: "/api/v1/auth/session",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "auth_session_list",
        method: "GET",
        path: "/api/v1/auth/sessions",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "auth_session_end",
        method: "DELETE",
        path: "/api/v1/auth/sessions/{id}",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "sso_complete",
        method: "POST",
        path: "/api/v1/auth/sso/complete",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::None,
    },
    RouteEntry {
        operation: "sso_provider_list",
        method: "GET",
        path: "/api/v1/auth/sso/providers",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::None,
    },
    RouteEntry {
        operation: "sso_acs",
        method: "POST",
        path: "/api/v1/auth/sso/{provider}/acs",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::None,
    },
    RouteEntry {
        operation: "sso_backchannel_logout",
        method: "POST",
        path: "/api/v1/auth/sso/{provider}/backchannel-logout",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::None,
    },
    RouteEntry {
        operation: "sso_begin",
        method: "POST",
        path: "/api/v1/auth/sso/{provider}/begin",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::None,
    },
    RouteEntry {
        operation: "sso_callback",
        method: "GET",
        path: "/api/v1/auth/sso/{provider}/callback",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::None,
    },
    RouteEntry {
        operation: "sso_saml_metadata",
        method: "GET",
        path: "/api/v1/auth/sso/{provider}/saml-metadata",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::None,
    },
    RouteEntry {
        operation: "sso_slo_get",
        method: "GET",
        path: "/api/v1/auth/sso/{provider}/slo",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::None,
    },
    RouteEntry {
        operation: "sso_slo_post",
        method: "POST",
        path: "/api/v1/auth/sso/{provider}/slo",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::None,
    },
    RouteEntry {
        operation: "connector_list",
        method: "GET",
        path: "/api/v1/connectors",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "connector_get",
        method: "GET",
        path: "/api/v1/connectors/{name}",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "connector_invoke",
        method: "POST",
        path: "/api/v1/connectors/{name}/operations/{op}",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "connector_runs",
        method: "GET",
        path: "/api/v1/connectors/{name}/runs",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "event_consumer_list",
        method: "GET",
        path: "/api/v1/events/consumers",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "event_consumer_delete",
        method: "DELETE",
        path: "/api/v1/events/consumers/{name}",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "event_consumer_get",
        method: "GET",
        path: "/api/v1/events/consumers/{name}",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "event_consumer_put",
        method: "PUT",
        path: "/api/v1/events/consumers/{name}",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "event_consumer_ack",
        method: "POST",
        path: "/api/v1/events/consumers/{name}/ack",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "event_consumer_pull",
        method: "POST",
        path: "/api/v1/events/consumers/{name}/pull",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "event_consumer_restart",
        method: "POST",
        path: "/api/v1/events/consumers/{name}/restart",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "event_stream",
        method: "GET",
        path: "/api/v1/events/stream",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "file_upload",
        method: "POST",
        path: "/api/v1/files",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "file_get",
        method: "GET",
        path: "/api/v1/files/{id}",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "graph_query",
        method: "POST",
        path: "/api/v1/graph/query",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "identity_api_key_list",
        method: "GET",
        path: "/api/v1/identity/api-keys",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "identity_api_key_create",
        method: "POST",
        path: "/api/v1/identity/api-keys",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "identity_api_key_delete",
        method: "DELETE",
        path: "/api/v1/identity/api-keys/{id}",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "identity_assignment_rules_get",
        method: "GET",
        path: "/api/v1/identity/assignment-rules",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "identity_assignment_rules_put",
        method: "PUT",
        path: "/api/v1/identity/assignment-rules",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "identity_assignment_rules_preview",
        method: "POST",
        path: "/api/v1/identity/assignment-rules/preview",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "identity_assignment_rules_run",
        method: "POST",
        path: "/api/v1/identity/assignment-rules/run",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "identity_assignment_list",
        method: "GET",
        path: "/api/v1/identity/assignments",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "identity_assignment_create",
        method: "POST",
        path: "/api/v1/identity/assignments",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "identity_assignment_delete",
        method: "DELETE",
        path: "/api/v1/identity/assignments/{id}",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "identity_impersonation_end",
        method: "DELETE",
        path: "/api/v1/identity/impersonation",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "identity_impersonation_start",
        method: "POST",
        path: "/api/v1/identity/impersonation",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "identity_invitation_list",
        method: "GET",
        path: "/api/v1/identity/invitations",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "identity_invitation_revoke",
        method: "DELETE",
        path: "/api/v1/identity/invitations/{id}",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "identity_invitation_resend",
        method: "POST",
        path: "/api/v1/identity/invitations/{id}/resend",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "identity_role_list",
        method: "GET",
        path: "/api/v1/identity/roles",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "identity_role_get",
        method: "GET",
        path: "/api/v1/identity/roles/{id}",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "identity_role_customise",
        method: "POST",
        path: "/api/v1/identity/roles/{id}/customise",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "identity_role_upgrade_diff",
        method: "GET",
        path: "/api/v1/identity/roles/{id}/upgrade-diff",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "identity_scim_credential_list",
        method: "GET",
        path: "/api/v1/identity/scim-credentials",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "identity_scim_credential_create",
        method: "POST",
        path: "/api/v1/identity/scim-credentials",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "identity_scim_credential_delete",
        method: "DELETE",
        path: "/api/v1/identity/scim-credentials/{id}",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "identity_session_list",
        method: "GET",
        path: "/api/v1/identity/sessions",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "identity_session_end",
        method: "DELETE",
        path: "/api/v1/identity/sessions/{id}",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "sso_admin_policy_get",
        method: "GET",
        path: "/api/v1/identity/sso/policy",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "sso_admin_policy_put",
        method: "PUT",
        path: "/api/v1/identity/sso/policy",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "sso_admin_provider_list",
        method: "GET",
        path: "/api/v1/identity/sso/providers",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "sso_admin_provider_create",
        method: "POST",
        path: "/api/v1/identity/sso/providers",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "sso_admin_provider_delete",
        method: "DELETE",
        path: "/api/v1/identity/sso/providers/{id}",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "sso_admin_provider_get",
        method: "GET",
        path: "/api/v1/identity/sso/providers/{id}",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "sso_admin_provider_put",
        method: "PUT",
        path: "/api/v1/identity/sso/providers/{id}",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "sso_admin_provider_test",
        method: "POST",
        path: "/api/v1/identity/sso/providers/{id}/test",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "identity_user_list",
        method: "GET",
        path: "/api/v1/identity/users",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "identity_user_create",
        method: "POST",
        path: "/api/v1/identity/users",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "identity_user_get",
        method: "GET",
        path: "/api/v1/identity/users/{id}",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "identity_user_patch",
        method: "PATCH",
        path: "/api/v1/identity/users/{id}",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "identity_user_anonymise",
        method: "POST",
        path: "/api/v1/identity/users/{id}/anonymise",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "identity_user_disable",
        method: "POST",
        path: "/api/v1/identity/users/{id}/disable",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "identity_user_enable",
        method: "POST",
        path: "/api/v1/identity/users/{id}/enable",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "identity_user_recover",
        method: "POST",
        path: "/api/v1/identity/users/{id}/recover",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "identity_user_sessions_end_all",
        method: "POST",
        path: "/api/v1/identity/users/{id}/sessions/end-all",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "import_create",
        method: "POST",
        path: "/api/v1/import",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "import_limits",
        method: "GET",
        path: "/api/v1/import/limits",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "import_get",
        method: "GET",
        path: "/api/v1/import/{id}",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "import_cancel",
        method: "POST",
        path: "/api/v1/import/{id}/cancel",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "import_errors",
        method: "GET",
        path: "/api/v1/import/{id}/errors",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "import_run",
        method: "POST",
        path: "/api/v1/import/{id}/run",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "journal_info_get",
        method: "GET",
        path: "/api/v1/journal",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "journal_manifest_get",
        method: "GET",
        path: "/api/v1/journal/manifest/{generation}",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "journal_segment_get",
        method: "GET",
        path: "/api/v1/journal/segments/{generation}/{first_seq}-{last_seq}",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "lineage_trace",
        method: "POST",
        path: "/api/v1/lineage",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "link_inbound",
        method: "POST",
        path: "/api/v1/link/inbound/{peer}",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::None,
    },
    RouteEntry {
        operation: "lookup_search",
        method: "POST",
        path: "/api/v1/lookup/{doctype}/{field}",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "meta_get",
        method: "GET",
        path: "/api/v1/meta/{doctype}",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "function_call_get",
        method: "GET",
        path: "/api/v1/method/{app}/{module}/{function}",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "function_call",
        method: "POST",
        path: "/api/v1/method/{app}/{module}/{function}",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "notification_list",
        method: "GET",
        path: "/api/v1/notifications",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "occupancy_availability",
        method: "GET",
        path: "/api/v1/occupancy/{entity}/availability",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "occupancy_claim",
        method: "POST",
        path: "/api/v1/occupancy/{entity}/holds",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "occupancy_release",
        method: "DELETE",
        path: "/api/v1/occupancy/{entity}/holds/{id}",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "occupancy_confirm",
        method: "POST",
        path: "/api/v1/occupancy/{entity}/holds/{id}/confirm",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "print_get",
        method: "GET",
        path: "/api/v1/print/{doctype}/{id}",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "process_list",
        method: "GET",
        path: "/api/v1/process",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "process_webhook",
        method: "POST",
        path: "/api/v1/process/hook/{trigger}",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::None,
    },
    RouteEntry {
        operation: "run_list",
        method: "GET",
        path: "/api/v1/process/runs",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "run_get",
        method: "GET",
        path: "/api/v1/process/runs/{id}",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "run_cancel",
        method: "POST",
        path: "/api/v1/process/runs/{id}/cancel",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "run_events",
        method: "GET",
        path: "/api/v1/process/runs/{id}/events",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "process_migration_get",
        method: "GET",
        path: "/api/v1/process/{process}/migrations/{id}",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "run_start",
        method: "POST",
        path: "/api/v1/process/{process}/runs",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "process_version_list",
        method: "GET",
        path: "/api/v1/process/{process}/versions",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "process_version_publish",
        method: "POST",
        path: "/api/v1/process/{process}/versions",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "process_migration_start",
        method: "POST",
        path: "/api/v1/process/{process}/versions/{from}/migrations",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "process_migration_plan",
        method: "POST",
        path: "/api/v1/process/{process}/versions/{from}/migrations/plan",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "process_version_get",
        method: "GET",
        path: "/api/v1/process/{process}/versions/{n}",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "process_version_retire",
        method: "POST",
        path: "/api/v1/process/{process}/versions/{n}/retire",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "report_list",
        method: "GET",
        path: "/api/v1/report",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "report_describe",
        method: "GET",
        path: "/api/v1/report/{report}",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "report_run",
        method: "POST",
        path: "/api/v1/report/{report}/run",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "resource_list",
        method: "GET",
        path: "/api/v1/resource/{doctype}",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "resource_create",
        method: "POST",
        path: "/api/v1/resource/{doctype}",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "resource_query",
        method: "POST",
        path: "/api/v1/resource/{doctype}/query",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "resource_delete",
        method: "DELETE",
        path: "/api/v1/resource/{doctype}/{id}",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "resource_get",
        method: "GET",
        path: "/api/v1/resource/{doctype}/{id}",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "resource_update",
        method: "PATCH",
        path: "/api/v1/resource/{doctype}/{id}",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "lifecycle_cancel",
        method: "POST",
        path: "/api/v1/resource/{doctype}/{id}/cancel",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "lifecycle_correct",
        method: "POST",
        path: "/api/v1/resource/{doctype}/{id}/correct",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "document_method_call",
        method: "POST",
        path: "/api/v1/resource/{doctype}/{id}/methods/{method}",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "lifecycle_post",
        method: "POST",
        path: "/api/v1/resource/{doctype}/{id}/post",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "lifecycle_rename",
        method: "POST",
        path: "/api/v1/resource/{doctype}/{id}/rename",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "field_reveal",
        method: "POST",
        path: "/api/v1/resource/{doctype}/{id}/reveal",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "task_list",
        method: "GET",
        path: "/api/v1/task",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "task_get",
        method: "GET",
        path: "/api/v1/task/{id}",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "task_claim",
        method: "POST",
        path: "/api/v1/task/{id}/claim",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "task_complete",
        method: "POST",
        path: "/api/v1/task/{id}/complete",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "task_delegate",
        method: "POST",
        path: "/api/v1/task/{id}/delegate",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "task_reassign",
        method: "POST",
        path: "/api/v1/task/{id}/reassign",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "task_release",
        method: "POST",
        path: "/api/v1/task/{id}/release",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "webhook_signing_secret_get",
        method: "GET",
        path: "/api/v1/webhooks/{name}/signing-secret",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
    RouteEntry {
        operation: "webhook_signing_secret_put",
        method: "PUT",
        path: "/api/v1/webhooks/{name}/signing-secret",
        audiences: STAFF_PARTNER,
        auth: RouteAuth::Required,
    },
];

/// Every route the native API router can mount, sorted by path then method.
pub fn route_table() -> &'static [RouteEntry] {
    &TABLE
}

/// The entry for `method` on the axum pattern `path`, if there is one.
pub fn entry_for(method: &str, path: &str) -> Option<&'static RouteEntry> {
    TABLE
        .iter()
        .find(|entry| entry.method == method && same_template(entry.path, path))
}

/// Whether two path templates are the same route: equal segment by segment, a `{name}` segment
/// matching any other `{name}` segment. One axum router allows one parameter name per segment,
/// while the contract names the same segment differently in different operations
/// (`{n}` and `{from}` for a version number).
fn same_template(a: &str, b: &str) -> bool {
    let param = |segment: &str| segment.starts_with('{') && segment.ends_with('}');
    let mut left = a.split('/');
    let mut right = b.split('/');
    loop {
        match (left.next(), right.next()) {
            (None, None) => return true,
            (Some(x), Some(y)) if x == y || (param(x) && param(y)) => {}
            _ => return false,
        }
    }
}

/// The mark of `method` on the matched pattern `path`; `None` for a route not in the table.
pub fn mark_for(method: &str, path: &str) -> Option<RouteMark> {
    entry_for(method, path).map(|entry| RouteMark {
        operation: entry.operation,
        audiences: entry.audiences,
        auth: entry.auth,
    })
}

/// Whether any of `wanted` is among the entry's audiences. An empty `wanted` matches nothing
/// (never "all").
pub fn entry_allows(entry: &RouteEntry, wanted: &[Audience]) -> bool {
    entry.audiences.iter().any(|a| wanted.contains(a))
}

/// Whether a registered route group may be mounted for `wanted`: every operation it serves has
/// at least one table entry, and every such entry allows one of `wanted`. A group serving an
/// operation the table does not list is never mounted (fail closed).
pub fn group_allowed(serves: &[&str], wanted: &[Audience]) -> bool {
    !serves.is_empty()
        && serves.iter().all(|operation| {
            let mut entries = TABLE
                .iter()
                .filter(|entry| entry.operation == *operation)
                .peekable();
            entries.peek().is_some() && entries.all(|entry| entry_allows(entry, wanted))
        })
}

/// The layer the router puts on every built router: stamps [`RouteMark`] on the request and on
/// the response of a matched route. Never logs, and reads no header.
pub(super) async fn stamp_route_mark(mut request: Request, next: Next) -> Response {
    let mark = request
        .extensions()
        .get::<MatchedPath>()
        .and_then(|path| mark_for(request.method().as_str(), path.as_str()));
    if let Some(mark) = mark {
        request.extensions_mut().insert(mark);
    }
    let mut response = next.run(request).await;
    if let Some(mark) = mark {
        response.extensions_mut().insert(mark);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_entry_has_audiences_and_no_consumer_mark() {
        for entry in route_table() {
            assert!(!entry.audiences.is_empty(), "{}", entry.operation);
            assert!(
                !entry.audiences.contains(&Audience::Consumer),
                "{} is marked consumer; the consumer surface starts empty",
                entry.operation
            );
        }
    }

    #[test]
    fn the_table_is_sorted_and_has_no_duplicate_method_and_path() {
        let keys: Vec<(&str, &str)> = route_table().iter().map(|e| (e.path, e.method)).collect();
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(keys, sorted);
    }

    #[test]
    fn operations_are_unique_per_method_and_methods_are_plain() {
        let mut seen = std::collections::BTreeSet::new();
        for entry in route_table() {
            assert!(seen.insert((entry.operation, entry.method)));
            assert!(matches!(
                entry.method,
                "GET" | "POST" | "PUT" | "PATCH" | "DELETE"
            ));
            assert!(entry.path.starts_with("/api/v1/"));
            assert!(!entry.path.ends_with('/'));
        }
    }

    #[test]
    fn only_the_login_and_the_cookie_renewal_are_unauthenticated() {
        let open: Vec<&str> = route_table()
            .iter()
            .filter(|e| e.auth == RouteAuth::None)
            .map(|e| e.operation)
            .collect();
        // The sign-in family (invitation accept, SSO) and the inbound routes (the trigger webhook,
        // the external link) are authenticated by their own token or signature, not a bearer.
        assert_eq!(
            open,
            vec![
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
            ]
        );
    }

    #[test]
    fn lookup_finds_a_known_route_and_nothing_else() {
        let mark = mark_for("GET", "/api/v1/auth/session");
        assert_eq!(mark.map(|m| m.operation), Some("auth_session"));
        assert!(mark_for("POST", "/api/v1/auth/session").is_none());
        assert!(mark_for("GET", "/api/v1/auth/session/").is_none());
        assert!(mark_for("GET", "/API/v1/auth/session").is_none());
    }

    #[test]
    fn group_allowed_is_fail_closed() {
        let staff = [Audience::Staff];
        assert!(group_allowed(&["report_list", "report_describe"], &staff));
        assert!(!group_allowed(&["report_list"], &[Audience::Consumer]));
        assert!(!group_allowed(&["report_list"], &[]));
        assert!(!group_allowed(&["no_such_operation"], &staff));
        assert!(!group_allowed(&[], &staff));
    }
}
