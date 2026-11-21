//! The connector end-to-end suite (WP-INT-05.T5) on a running server and PostgreSQL.
//!
//! The six groups below are the acceptance of WP-INT-05. They are **blocked**, each with the
//! observation that proves the surface is missing, because the shared fixture host does not serve
//! them yet: the connector service (WP-INT-05.T3) is not registered by the host, no `ConnectorsPort`
//! adapter is provided to the route registry (WP-INT-05.T4 ships the routes and an in-memory port),
//! the public ingress is not mounted, and the harness has no local stub server with an allow-list,
//! stub resolver or second node. When those exist each entry becomes a `CaseDef::run`; the
//! assertions the brief names are recorded in `pins` so the case is written against a fixed
//! description, not a guess. Blocked is not a pass: it shows in every run.

use super::CaseDef;

/// After the other groups.
pub const ORDER: u32 = 42;

const SECTION: &str = "Connectors";
const OWNER: &str =
    "WP-INT-05.T5 (host registration of the connector service and the ingress mount)";
const NO_ROUTES: &str =
    "`/api/v1/connectors*` is not mounted by the host: no `ConnectorsPort` adapter is provided";
const NO_INGRESS: &str = "`POST /api/v1/connectors/{name}/inbound` is not mounted by the host";
const NO_STUB: &str =
    "the harness has no local stub server, stub resolver, redirecting stub or second server node";

pub fn cases() -> Vec<CaseDef> {
    vec![
        CaseDef::blocked(
            "connectors: an_invoke_injects_the_credential_and_no_secret_reaches_the_response_run_log_document_or_server_log",
            SECTION,
            "with an allow-listed local stub and a secret stored through the write-only action, a permitted user invokes an operation; the stub sees the injected credential; the response, the run log, the connector document read and the server log never contain the secret",
            OWNER,
            NO_ROUTES,
        ),
        CaseDef::blocked(
            "connectors: a_user_without_the_right_and_a_user_of_another_tenant_cannot_list_get_or_invoke",
            SECTION,
            "a user without the right cannot invoke (403); tenant B cannot list, get or invoke tenant A's connector (404), and an inbound request for A's connector sent to B's host is the uniform 401",
            OWNER,
            NO_ROUTES,
        ),
        CaseDef::blocked(
            "connectors: the_inbound_ingress_accepts_a_signed_request_and_refuses_replay_tamper_and_unknown_names_alike",
            SECTION,
            "a signed request is accepted and processed by the job, a replayed delivery id is 409, a tampered body is 401, and an unknown name equals a bad signature in status and body",
            OWNER,
            NO_INGRESS,
        ),
        CaseDef::blocked(
            "connectors: ssrf_targets_are_refused_and_audited",
            SECTION,
            "127.0.0.1, 10.x, 169.254.169.254, localhost, a name resolving to loopback, an IPv6 loopback, an open redirect to a private address and a port outside the allow-list are all refused and audited",
            OWNER,
            NO_STUB,
        ),
        CaseDef::blocked(
            "connectors: a_scheduled_run_fires_once_with_two_nodes_and_a_killed_server_resumes_without_a_duplicate_effect",
            SECTION,
            "two server nodes fire a scheduled run once; a server killed mid-run resumes without a duplicate side effect (the idempotency key reaches the stub)",
            OWNER,
            NO_STUB,
        ),
        CaseDef::blocked(
            "connectors: secret_rotation_takes_effect_on_the_next_run_and_deleting_the_connector_deletes_its_secret",
            SECTION,
            "a rotated secret is used by the next run; deleting the connector deletes its secret",
            OWNER,
            NO_ROUTES,
        ),
    ]
}
