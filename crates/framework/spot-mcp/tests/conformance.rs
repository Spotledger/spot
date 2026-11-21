//! Protocol conformance transcripts: initialize, discover, list, call, cancel, pagination and
//! the error mapping.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use serde_json::{Value as Json, json};
use spot_mcp::principal::Credential;
use time::OffsetDateTime;

use support::{DOCTYPE, doc_ids, request, result_of, server, site};

fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
}

#[tokio::test]
async fn discover_lists_the_supported_versions() {
    let (server, _meta) = server().await;
    let body = request(1, "server/discover", json!({}));
    let messages = server
        .handle_text(&body, Some(&Credential::bearer("alice")), now())
        .await;
    let result = result_of(&messages);
    assert_eq!(result["resultType"], "complete");
    assert_eq!(result["supportedVersions"][0], spot_mcp::PROTOCOL_VERSION);
    assert!(result["capabilities"]["tools"].is_object());
    assert_eq!(
        result["_meta"]["io.modelcontextprotocol/serverInfo"]["name"],
        "spot"
    );
}

#[tokio::test]
async fn legacy_initialize_handshake() {
    let (server, _meta) = server().await;
    let body = json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": { "protocolVersion": spot_mcp::LEGACY_PROTOCOL_VERSION,
                    "capabilities": {}, "clientInfo": { "name": "old", "version": "1" } }
    })
    .to_string();
    let messages = server
        .handle_text(&body, Some(&Credential::bearer("alice")), now())
        .await;
    let result = result_of(&messages);
    assert_eq!(result["protocolVersion"], spot_mcp::LEGACY_PROTOCOL_VERSION);
    assert_eq!(result["serverInfo"]["name"], "spot");
}

#[tokio::test]
async fn ping_answers_empty() {
    let (server, _meta) = server().await;
    let body = request(7, "ping", json!({}));
    let messages = server
        .handle_text(&body, Some(&Credential::bearer("alice")), now())
        .await;
    assert_eq!(result_of(&messages)["resultType"], "complete");
}

#[tokio::test]
async fn unsupported_protocol_version_is_refused() {
    let (server, _meta) = server().await;
    let body = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/list",
        "params": { "_meta": {
            "io.modelcontextprotocol/protocolVersion": "1900-01-01",
            "io.modelcontextprotocol/clientCapabilities": {}
        } }
    })
    .to_string();
    let messages = server
        .handle_text(&body, Some(&Credential::bearer("alice")), now())
        .await;
    let error = support::error_of(&messages);
    assert_eq!(error["code"], -32022);
    assert_eq!(error["data"]["code"], "mcp.unsupported_protocol_version");
}

#[tokio::test]
async fn a_request_without_capabilities_is_refused() {
    let (server, _meta) = server().await;
    let body = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/list",
        "params": { "_meta": {
            "io.modelcontextprotocol/protocolVersion": spot_mcp::PROTOCOL_VERSION
        } }
    })
    .to_string();
    let messages = server
        .handle_text(&body, Some(&Credential::bearer("alice")), now())
        .await;
    let error = support::error_of(&messages);
    assert_eq!(error["code"], -32602);
}

#[tokio::test]
async fn unknown_method_is_refused() {
    let (server, _meta) = server().await;
    let body = request(1, "does/not/exist", json!({}));
    let messages = server
        .handle_text(&body, Some(&Credential::bearer("alice")), now())
        .await;
    let error = support::error_of(&messages);
    assert_eq!(error["code"], -32601);
    assert_eq!(error["data"]["code"], "mcp.method_not_found");
}

#[tokio::test]
async fn parse_error_is_reported_without_an_id() {
    let (server, _meta) = server().await;
    let messages = server
        .handle_text("not json", Some(&Credential::bearer("alice")), now())
        .await;
    let error = support::error_of(&messages);
    assert_eq!(error["code"], -32700);
    assert_eq!(error["data"]["code"], "mcp.parse");
}

#[tokio::test]
async fn tools_list_shows_metadata_tools_and_functions() {
    let (server, _meta) = server().await;
    let body = request(1, "tools/list", json!({}));
    let messages = server
        .handle_text(&body, Some(&Credential::bearer("alice")), now())
        .await;
    let result = result_of(&messages);
    let names: Vec<&str> = result["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    assert!(names.contains(&format!("search_{DOCTYPE}").as_str()));
    assert!(names.contains(&format!("get_{DOCTYPE}").as_str()));
    assert!(names.contains(&"query"));
    assert!(names.contains(&"job_status"));
    assert!(names.contains(&"call_demo_greet_hello"));
    assert!(names.contains(&"call_demo_jobs_run"));
    // A reader may not write, so no create or update tool.
    assert!(!names.contains(&format!("create_{DOCTYPE}_draft").as_str()));
    assert!(!names.contains(&format!("update_{DOCTYPE}").as_str()));
    // Stable order: sorted by name.
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted);
}

#[tokio::test]
async fn tools_list_paginates() {
    let (server, _meta) = server().await;
    let body = request(1, "tools/list", json!({}));
    let messages = server
        .handle_text(&body, Some(&Credential::bearer("alice")), now())
        .await;
    let result = result_of(&messages);
    let first_page = result["tools"].as_array().unwrap().len();
    assert!(first_page > 0);
    // The default page size is 100; our catalog is smaller, so there is no cursor.
    assert!(result.get("nextCursor").is_none());
}

#[tokio::test]
async fn get_masks_the_protected_field_for_a_reader() {
    let (server, _meta) = server().await;
    let id = doc_ids()[0].to_string();
    let body = request(
        1,
        "tools/call",
        json!({ "name": format!("get_{DOCTYPE}"), "arguments": { "id": id } }),
    );
    let messages = server
        .handle_text(&body, Some(&Credential::bearer("alice")), now())
        .await;
    let result = result_of(&messages);
    assert_eq!(result["isError"], false);
    assert_eq!(result["structuredContent"]["salary"], "***");
    assert_eq!(result["structuredContent"]["title"], "one");
    assert_eq!(result["_meta"]["spot/untrusted"], true);
}

#[tokio::test]
async fn get_reveals_the_protected_field_to_a_manager() {
    let (server, _meta) = server().await;
    let id = doc_ids()[0].to_string();
    let body = request(
        1,
        "tools/call",
        json!({ "name": format!("get_{DOCTYPE}"), "arguments": { "id": id } }),
    );
    let messages = server
        .handle_text(&body, Some(&Credential::bearer("bob")), now())
        .await;
    let result = result_of(&messages);
    assert_eq!(result["structuredContent"]["salary"], "100.00");
}

#[tokio::test]
async fn unknown_tool_is_a_protocol_error() {
    let (server, _meta) = server().await;
    let body = request(
        1,
        "tools/call",
        json!({ "name": "no_such_tool", "arguments": {} }),
    );
    let messages = server
        .handle_text(&body, Some(&Credential::bearer("alice")), now())
        .await;
    let error = support::error_of(&messages);
    assert_eq!(error["code"], -32602);
    assert_eq!(error["data"]["code"], "mcp.unknown_tool");
}

#[tokio::test]
async fn bad_arguments_are_refused() {
    let (server, _meta) = server().await;
    let body = request(
        1,
        "tools/call",
        json!({ "name": format!("get_{DOCTYPE}"), "arguments": {} }),
    );
    let messages = server
        .handle_text(&body, Some(&Credential::bearer("alice")), now())
        .await;
    let error = support::error_of(&messages);
    assert_eq!(error["data"]["code"], "mcp.invalid_arguments");
}

#[tokio::test]
async fn a_cancelled_request_is_refused() {
    let (server, _meta) = server().await;
    let cancel = json!({
        "jsonrpc": "2.0", "method": "notifications/cancelled",
        "params": { "requestId": 42, "reason": "user" }
    })
    .to_string();
    let _ = server
        .handle_text(&cancel, Some(&Credential::bearer("alice")), now())
        .await;
    let id = doc_ids()[0].to_string();
    let body = request(
        42,
        "tools/call",
        json!({ "name": format!("get_{DOCTYPE}"), "arguments": { "id": id } }),
    );
    let messages = server
        .handle_text(&body, Some(&Credential::bearer("alice")), now())
        .await;
    let error = support::error_of(&messages);
    assert_eq!(error["data"]["code"], "mcp.cancelled");
}

#[tokio::test]
async fn a_progress_token_yields_progress_notifications() {
    let (server, _meta) = server().await;
    let mut params = json!({
        "name": format!("get_{DOCTYPE}"),
        "arguments": { "id": doc_ids()[0].to_string() },
    });
    params.as_object_mut().unwrap().insert(
        "_meta".to_owned(),
        json!({
            "io.modelcontextprotocol/protocolVersion": spot_mcp::PROTOCOL_VERSION,
            "io.modelcontextprotocol/clientCapabilities": {},
            "progressToken": "tok-1",
        }),
    );
    let body =
        json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": params }).to_string();
    let messages = server
        .handle_text(&body, Some(&Credential::bearer("alice")), now())
        .await;
    let progress: Vec<&Json> = messages
        .iter()
        .filter(|m| m["method"] == "notifications/progress")
        .collect();
    assert_eq!(progress.len(), 2);
    assert_eq!(progress[0]["params"]["progressToken"], "tok-1");
}

#[tokio::test]
async fn resources_list_and_read_a_document() {
    let (server, _meta) = server().await;
    let body = request(1, "resources/list", json!({}));
    let messages = server
        .handle_text(&body, Some(&Credential::bearer("alice")), now())
        .await;
    let result = result_of(&messages);
    let resources = result["resources"].as_array().unwrap();
    assert!(
        resources
            .iter()
            .any(|r| r["uri"] == format!("spot://{}/{DOCTYPE}", site()))
    );

    let uri = format!("spot://{}/{DOCTYPE}/{}", site(), doc_ids()[1]);
    let body = request(1, "resources/read", json!({ "uri": uri }));
    let messages = server
        .handle_text(&body, Some(&Credential::bearer("alice")), now())
        .await;
    let result = result_of(&messages);
    let text = result["contents"][0]["text"].as_str().unwrap();
    assert!(text.contains("\"title\":\"two\""));
    // The reader sees the masked salary, never the real one.
    assert!(!text.contains("200.00"));
}

#[tokio::test]
async fn prompts_are_listed_and_got() {
    let (server, _meta) = server().await;
    let body = request(1, "prompts/list", json!({}));
    let messages = server
        .handle_text(&body, Some(&Credential::bearer("alice")), now())
        .await;
    let result = result_of(&messages);
    assert!(result["prompts"].as_array().unwrap().len() >= 2);

    let body = request(
        1,
        "prompts/get",
        json!({ "name": "explain_document",
                "arguments": { "doctype": DOCTYPE, "id": doc_ids()[0].to_string() } }),
    );
    let messages = server
        .handle_text(&body, Some(&Credential::bearer("alice")), now())
        .await;
    let result = result_of(&messages);
    assert!(
        result["messages"][0]["content"]["text"]
            .as_str()
            .unwrap()
            .contains("note")
    );
}

#[tokio::test]
async fn an_api_key_is_narrowed_to_its_scope() {
    let (server, _meta) = server().await;
    // The key allows read on `note`, so a get works.
    let id = doc_ids()[0].to_string();
    let body = request(
        1,
        "tools/call",
        json!({ "name": format!("get_{DOCTYPE}"), "arguments": { "id": id } }),
    );
    let messages = server
        .handle_text(
            &body,
            Some(&Credential::api_key("key-alice", "secret")),
            now(),
        )
        .await;
    assert_eq!(result_of(&messages)["isError"], false);
}

#[tokio::test]
async fn a_missing_credential_is_refused() {
    let (server, _meta) = server().await;
    let body = request(1, "tools/list", json!({}));
    let messages = server.handle_text(&body, None, now()).await;
    let error = support::error_of(&messages);
    assert_eq!(error["data"]["code"], "mcp.unauthorized");
}

#[tokio::test]
async fn every_call_is_recorded_to_the_audit_chain() {
    let audit = std::sync::Arc::new(spot_mcp::audit::MemoryAudit::new());
    let (server, _meta) = support::server_with_audit(audit.clone()).await;
    let id = doc_ids()[0].to_string();
    let body = request(
        1,
        "tools/call",
        json!({ "name": format!("get_{DOCTYPE}"), "arguments": { "id": id } }),
    );
    let _ = server
        .handle_text(&body, Some(&Credential::bearer("alice")), now())
        .await;
    let records = audit.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].kind, "mcp.call");
    assert!(records[0].actor.starts_with("user:"));
    assert_eq!(
        records[0].payload.get("tool"),
        Some(&spot_audit_chain::PayloadValue::Text(format!(
            "get_{DOCTYPE}"
        )))
    );
}

#[tokio::test]
async fn an_out_of_tier_write_is_refused() {
    let (server, _meta) = server().await;
    let body = request(
        1,
        "tools/call",
        json!({ "name": format!("create_{DOCTYPE}_draft"), "arguments": { "title": "x" } }),
    );
    let messages = server
        .handle_text(&body, Some(&Credential::bearer("agent0")), now())
        .await;
    let error = support::error_of(&messages);
    assert_eq!(error["data"]["code"], "mcp.permission_denied");
}

#[tokio::test]
async fn filtering_on_an_unreadable_field_is_refused() {
    let (server, _meta) = server().await;
    let body = request(
        1,
        "tools/call",
        json!({
            "name": "query",
            "arguments": {
                "doctype": DOCTYPE,
                "select": [ { "expr": { "field": "id" }, "as": "id" } ],
                "filter": { "op": "eq", "lhs": { "field": "secret" }, "rhs": "secret-one" },
            },
        }),
    );
    let messages = server
        .handle_text(&body, Some(&Credential::bearer("alice")), now())
        .await;
    let error = support::error_of(&messages);
    assert_eq!(error["data"]["code"], "permission.field_denied");
}
