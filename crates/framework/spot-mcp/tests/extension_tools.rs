//! Extension tools (contract `mcp.md`, section 5): a hosting service adds a tool to `tools/list`
//! and `tools/call` through the registry the server reads on every request. The rule under test:
//! listed only for a principal that may call it, run as the caller, the same limits, deadline and
//! audit as any tool, refused names, a tool error that carries the service's own code, a panic or
//! a hang that never takes the server down, and absence after an uninstall.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value as Json, json};
use spot_coordinator::RateLimit;
use spot_mcp::principal::Credential;
use spot_mcp::{
    BoxFuture, ExtensionError, ExtensionRegistry, ExtensionSpec, ExtensionTool, InstallError,
    Limits, McpPrincipal, MemoryAudit, ToolKind,
};
use time::OffsetDateTime;

use support::{DOCTYPE, error_of, request, result_of, server, server_with, server_with_audit};

const NEEDLE: &str = "needle-argument-value";

fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
}

/// What the probe does when it is called.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Answers who called and what the argument said.
    Echo,
    /// Refuses with a service code.
    Refuse,
    /// Fails with a service code.
    Fail,
    /// Panics while running.
    PanicInCall,
    /// Panics while building its spec.
    PanicInSpec,
    /// Never answers.
    Hang,
}

/// A fake tool. It is listed for every principal except the users in `hidden_from`.
struct Probe {
    name: String,
    mode: Mode,
    writes: bool,
    hidden_from: Vec<&'static str>,
    calls: AtomicUsize,
    callers: Mutex<Vec<String>>,
}

impl Probe {
    fn new(name: &str, mode: Mode) -> Self {
        Self {
            name: name.to_owned(),
            mode,
            writes: false,
            hidden_from: Vec::new(),
            calls: AtomicUsize::new(0),
            callers: Mutex::new(Vec::new()),
        }
    }

    fn writing(mut self) -> Self {
        self.writes = true;
        self
    }

    fn hidden_from(mut self, users: &[&'static str]) -> Self {
        self.hidden_from = users.to_vec();
        self
    }
}

impl ExtensionTool for Probe {
    fn name(&self) -> &str {
        &self.name
    }

    fn spec(&self, principal: &McpPrincipal) -> Option<ExtensionSpec> {
        assert!(self.mode != Mode::PanicInSpec, "the spec panics");
        if self.hidden_from.contains(&principal.user()) {
            return None;
        }
        let spec = ExtensionSpec::new(
            "A probe tool.",
            json!({
                "type": "object",
                "properties": { "text": { "type": "string", "maxLength": 64 } },
                "additionalProperties": false,
            }),
        );
        Some(if self.writes { spec.writing() } else { spec })
    }

    fn call<'a>(
        &'a self,
        principal: &'a McpPrincipal,
        arguments: &'a Json,
    ) -> BoxFuture<'a, Result<Json, ExtensionError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.callers
                .lock()
                .unwrap()
                .push(principal.user().to_owned());
            match self.mode {
                Mode::Echo | Mode::PanicInSpec => Ok(json!({
                    "who": principal.user(),
                    "text": arguments.get("text").cloned().unwrap_or(Json::Null),
                })),
                Mode::Refuse => Err(ExtensionError::refused(
                    "remedies.refused",
                    "the remedy needs a higher tier",
                )),
                Mode::Fail => Err(ExtensionError::failed(
                    "retrieval.store_failed",
                    "the retrieval index could not answer",
                )),
                Mode::PanicInCall => panic!("the tool panics"),
                Mode::Hang => {
                    std::future::pending::<()>().await;
                    Ok(Json::Null)
                }
            }
        })
    }
}

async fn call_as(
    server: &spot_mcp::Server,
    user: &str,
    tool: &str,
    arguments: Json,
    id: i64,
) -> Vec<Json> {
    let body = request(
        id,
        "tools/call",
        json!({ "name": tool, "arguments": arguments }),
    );
    server
        .handle_text(&body, Some(&Credential::bearer(user)), now())
        .await
}

async fn list_as(server: &spot_mcp::Server, user: &str) -> Vec<String> {
    let messages = server
        .handle_text(
            &request(1, "tools/list", json!({})),
            Some(&Credential::bearer(user)),
            now(),
        )
        .await;
    result_of(&messages)["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn a_tool_is_listed_only_for_a_principal_whose_spec_is_some() {
    let (server, _meta) = server().await;
    server
        .extensions()
        .install(Arc::new(
            Probe::new("probe", Mode::Echo).hidden_from(&["alice"]),
        ))
        .unwrap();
    assert!(
        list_as(&server, "carol")
            .await
            .contains(&"probe".to_owned())
    );
    assert!(
        !list_as(&server, "alice")
            .await
            .contains(&"probe".to_owned())
    );
    // Not listed for alice means the same refusal as any name nobody offers.
    let refused = call_as(&server, "alice", "probe", json!({}), 2).await;
    assert_eq!(error_of(&refused)["data"]["code"], "mcp.unknown_tool");
}

#[tokio::test]
async fn extension_tools_follow_the_generated_ones_in_name_order() {
    let (server, _meta) = server().await;
    server
        .extensions()
        .install(Arc::new(Probe::new("zeta_probe", Mode::Echo)))
        .unwrap();
    server
        .extensions()
        .install(Arc::new(Probe::new("alpha_probe", Mode::Echo)))
        .unwrap();
    let names = list_as(&server, "carol").await;
    let alpha = names.iter().position(|n| n == "alpha_probe").unwrap();
    let zeta = names.iter().position(|n| n == "zeta_probe").unwrap();
    assert_eq!(zeta, alpha + 1);
    assert_eq!(zeta, names.len() - 1);
    assert!(names.contains(&format!("search_{DOCTYPE}")));
    assert!(names.contains(&"job_status".to_owned()));
}

#[tokio::test]
async fn a_call_runs_as_the_caller_and_answers() {
    let (server, _meta) = server().await;
    let probe = Arc::new(Probe::new("probe", Mode::Echo));
    server.extensions().install(probe.clone()).unwrap();
    let messages = call_as(&server, "carol", "probe", json!({ "text": "hello" }), 1).await;
    let result = result_of(&messages);
    assert_eq!(result["isError"], false);
    assert_eq!(result["structuredContent"]["who"], "carol");
    assert_eq!(result["structuredContent"]["text"], "hello");
    assert_eq!(probe.callers.lock().unwrap().as_slice(), ["carol"]);
    let other = call_as(&server, "bob", "probe", json!({}), 2).await;
    assert_eq!(result_of(&other)["structuredContent"]["who"], "bob");
}

#[tokio::test]
async fn arguments_are_checked_against_the_spec_before_the_tool_runs() {
    let (server, _meta) = server().await;
    let probe = Arc::new(Probe::new("probe", Mode::Echo));
    server.extensions().install(probe.clone()).unwrap();
    let messages = call_as(&server, "carol", "probe", json!({ "text": 5 }), 1).await;
    assert_eq!(error_of(&messages)["data"]["code"], "mcp.invalid_arguments");
    assert_eq!(probe.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn the_audit_record_names_the_tool_and_carries_no_argument_value() {
    let audit = Arc::new(MemoryAudit::new());
    let (server, _meta) = server_with_audit(audit.clone()).await;
    server
        .extensions()
        .install(Arc::new(Probe::new("probe", Mode::Echo)))
        .unwrap();
    let _ = call_as(&server, "carol", "probe", json!({ "text": NEEDLE }), 1).await;
    let records = audit.records();
    assert_eq!(records.len(), 1, "a read-only tool records its completion");
    assert_eq!(
        records[0].payload.get("tool"),
        Some(&spot_audit_chain::PayloadValue::Text("probe".to_owned()))
    );
    assert!(!format!("{records:?}").contains(NEEDLE));
}

#[tokio::test]
async fn a_writing_tool_records_its_intent_first() {
    let audit = Arc::new(MemoryAudit::new());
    let (server, _meta) = server_with_audit(audit.clone()).await;
    server
        .extensions()
        .install(Arc::new(Probe::new("writer_probe", Mode::Echo).writing()))
        .unwrap();
    let _ = call_as(
        &server,
        "carol",
        "writer_probe",
        json!({ "text": NEEDLE }),
        1,
    )
    .await;
    let records = audit.records();
    assert_eq!(records.len(), 2, "intent, then completion");
    assert_eq!(
        records[0].payload.get("phase"),
        Some(&spot_audit_chain::PayloadValue::Text("intent".to_owned()))
    );
    assert!(!format!("{records:?}").contains(NEEDLE));
}

#[tokio::test]
async fn a_tool_gates_itself_so_the_generated_write_tier_rule_does_not_apply() {
    let (server, _meta) = server().await;
    server
        .extensions()
        .install(Arc::new(Probe::new("writer_probe", Mode::Echo).writing()))
        .unwrap();
    // agent0 is a T0 agent: a generated write tool would be refused for its tier.
    let messages = call_as(&server, "agent0", "writer_probe", json!({}), 1).await;
    assert_eq!(result_of(&messages)["isError"], false);
}

#[test]
fn a_name_that_collides_with_a_generated_tool_is_refused_at_install_time() {
    let registry = ExtensionRegistry::new();
    for name in [
        "query",
        "job_status",
        "search_note",
        "get_note",
        "create_note_draft",
        "update_note",
        "call_app_mod_fn",
        "Bad-Name",
        "",
    ] {
        let result = registry.install(Arc::new(Probe::new(name, Mode::Echo)));
        assert_eq!(result, Err(InstallError::BadName), "{name}");
    }
    assert!(registry.is_empty());
    registry
        .install(Arc::new(Probe::new("probe", Mode::Echo)))
        .unwrap();
    assert_eq!(
        registry.install(Arc::new(Probe::new("probe", Mode::Echo))),
        Err(InstallError::Duplicate)
    );
    assert_eq!(registry.len(), 1);
}

#[test]
fn the_catalogue_never_lists_an_extension_over_a_generated_name() {
    // The registry refuses these names; the catalogue is the second line of defence.
    let registry = support::registry();
    let engine = support::engine();
    let principal = support::principals().remove("carol").unwrap();
    let functions = support::functions();
    let clash: Arc<dyn ExtensionTool> = Arc::new(Probe::new("query", Mode::Echo));
    let catalog =
        spot_mcp::ToolCatalog::build_with(&registry, &engine, &principal, &functions, &[clash]);
    let spec = catalog.get("query").unwrap();
    assert_eq!(spec.kind, ToolKind::Query);
}

#[tokio::test]
async fn the_rate_limit_applies() {
    let limits = Limits {
        rate: RateLimit {
            capacity: 1,
            per: Duration::from_secs(3600),
        },
        ..Limits::default()
    };
    let (server, _meta) = server_with(limits, None).await;
    server
        .extensions()
        .install(Arc::new(Probe::new("probe", Mode::Echo)))
        .unwrap();
    let first = call_as(&server, "carol", "probe", json!({}), 1).await;
    assert!(error_of(&first).is_null());
    let second = call_as(&server, "carol", "probe", json!({}), 2).await;
    assert_eq!(error_of(&second)["data"]["code"], "mcp.rate_limited");
}

#[tokio::test]
async fn the_size_limit_applies() {
    let limits = Limits {
        max_input_bytes: 64,
        ..Limits::default()
    };
    let (server, _meta) = server_with(limits, None).await;
    server
        .extensions()
        .install(Arc::new(Probe::new("probe", Mode::Echo)))
        .unwrap();
    let messages = call_as(
        &server,
        "carol",
        "probe",
        json!({ "text": "x".repeat(60) }),
        1,
    )
    .await;
    assert_eq!(error_of(&messages)["data"]["code"], "mcp.limit_exceeded");
}

#[tokio::test]
async fn a_tool_error_carries_the_services_own_code() {
    let (server, _meta) = server().await;
    server
        .extensions()
        .install(Arc::new(Probe::new("refusing", Mode::Refuse)))
        .unwrap();
    server
        .extensions()
        .install(Arc::new(Probe::new("failing", Mode::Fail)))
        .unwrap();
    let refused = error_of(&call_as(&server, "carol", "refusing", json!({}), 1).await);
    assert_eq!(refused["data"]["code"], "remedies.refused");
    assert_eq!(refused["message"], "the remedy needs a higher tier");
    let failed = error_of(&call_as(&server, "carol", "failing", json!({}), 2).await);
    assert_eq!(failed["data"]["code"], "retrieval.store_failed");
}

#[tokio::test]
async fn a_tool_that_never_returns_hits_the_deadline() {
    let limits = Limits {
        call_timeout: Duration::from_millis(20),
        ..Limits::default()
    };
    let (server, _meta) = server_with(limits, None).await;
    server
        .extensions()
        .install(Arc::new(Probe::new("hanging", Mode::Hang)))
        .unwrap();
    let messages = call_as(&server, "carol", "hanging", json!({}), 1).await;
    assert_eq!(error_of(&messages)["data"]["code"], "mcp.timeout");
}

#[tokio::test]
async fn a_tool_that_panics_is_a_tool_error_and_the_server_goes_on() {
    let (server, _meta) = server().await;
    server
        .extensions()
        .install(Arc::new(Probe::new("panicking", Mode::PanicInCall)))
        .unwrap();
    server
        .extensions()
        .install(Arc::new(Probe::new("probe", Mode::Echo)))
        .unwrap();
    let messages = call_as(&server, "carol", "panicking", json!({}), 1).await;
    assert_eq!(error_of(&messages)["data"]["code"], "mcp.internal");
    // The next call, to another tool, is answered as usual.
    let next = call_as(&server, "carol", "probe", json!({}), 2).await;
    assert_eq!(result_of(&next)["isError"], false);
}

#[tokio::test]
async fn a_spec_that_panics_is_not_listed() {
    let (server, _meta) = server().await;
    server
        .extensions()
        .install(Arc::new(Probe::new("spec_panics", Mode::PanicInSpec)))
        .unwrap();
    server
        .extensions()
        .install(Arc::new(Probe::new("probe", Mode::Echo)))
        .unwrap();
    let names = list_as(&server, "carol").await;
    assert!(!names.contains(&"spec_panics".to_owned()));
    assert!(names.contains(&"probe".to_owned()));
}

#[tokio::test]
async fn after_an_uninstall_the_tool_is_absent_and_a_call_is_an_unknown_tool() {
    let (server, _meta) = server().await;
    server
        .extensions()
        .install(Arc::new(Probe::new("probe", Mode::Echo)))
        .unwrap();
    assert!(
        list_as(&server, "carol")
            .await
            .contains(&"probe".to_owned())
    );
    assert!(server.extensions().uninstall("probe"));
    assert!(!server.extensions().uninstall("probe"));
    assert!(
        !list_as(&server, "carol")
            .await
            .contains(&"probe".to_owned())
    );
    let messages = call_as(&server, "carol", "probe", json!({}), 1).await;
    assert_eq!(error_of(&messages)["data"]["code"], "mcp.unknown_tool");
}
