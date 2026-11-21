//! The argument check every `tools/call` runs before a tool is entered: a schema saying
//! `additionalProperties: false` refuses an unknown argument (`mcp.invalid_arguments`) and the
//! tool does not run; the bounds and `enum` an extension tool declares are enforced the same
//! way; the same call without the extra argument succeeds.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Value as Json, json};
use spot_mcp::principal::Credential;
use spot_mcp::{BoxFuture, ExtensionError, ExtensionSpec, ExtensionTool, McpPrincipal};
use time::OffsetDateTime;

use support::{DOCTYPE, doc_ids, error_of, request, result_of, server};

const NEEDLE: &str = "needle-argument-value";

fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
}

/// A tool with the shape of the real `retrieve` schema that counts how often it was entered.
struct Recording {
    calls: AtomicUsize,
}

impl Recording {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
        })
    }

    fn entered(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl ExtensionTool for Recording {
    fn name(&self) -> &str {
        "retrieve_probe"
    }

    fn spec(&self, _principal: &McpPrincipal) -> Option<ExtensionSpec> {
        Some(ExtensionSpec::new(
            "Finds things by meaning.",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["query"],
                "properties": {
                    "query": { "type": "string", "minLength": 1, "maxLength": 16 },
                    "k": { "type": "integer", "minimum": 1, "maximum": 10 },
                    "kinds": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 2,
                        "uniqueItems": true,
                        "items": { "type": "string", "enum": ["episode", "fact"] }
                    },
                    "options": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": { "deep": { "type": "boolean" } }
                    }
                }
            }),
        ))
    }

    fn call<'a>(
        &'a self,
        _principal: &'a McpPrincipal,
        _arguments: &'a Json,
    ) -> BoxFuture<'a, Result<Json, ExtensionError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(json!({ "hits": [] }))
        })
    }
}

async fn call_as(server: &spot_mcp::Server, user: &str, tool: &str, arguments: Json) -> Vec<Json> {
    let body = request(
        1,
        "tools/call",
        json!({ "name": tool, "arguments": arguments }),
    );
    server
        .handle_text(&body, Some(&Credential::bearer(user)), now())
        .await
}

fn invalid(messages: &[Json]) -> bool {
    error_of(messages)["data"]["code"] == "mcp.invalid_arguments"
}

#[tokio::test]
async fn an_unknown_argument_is_refused_and_the_tool_is_not_run() {
    let (server, _meta) = server().await;
    let tool = Recording::new();
    server.extensions().install(tool.clone()).unwrap();

    let refused = call_as(
        &server,
        "carol",
        "retrieve_probe",
        json!({ "query": "invoices", "tier": 3 }),
    )
    .await;
    assert!(invalid(&refused), "{refused:?}");
    assert_eq!(
        tool.entered(),
        0,
        "the tool ran despite the unknown argument"
    );

    let accepted = call_as(
        &server,
        "carol",
        "retrieve_probe",
        json!({ "query": "invoices" }),
    )
    .await;
    assert_eq!(result_of(&accepted)["isError"], false, "{accepted:?}");
    assert_eq!(tool.entered(), 1);
}

#[tokio::test]
async fn the_refusal_names_the_property_and_never_the_value() {
    let (server, _meta) = server().await;
    let tool = Recording::new();
    server.extensions().install(tool.clone()).unwrap();
    let refused = call_as(
        &server,
        "carol",
        "retrieve_probe",
        json!({ "query": "invoices", "tier": NEEDLE }),
    )
    .await;
    assert!(invalid(&refused));
    let text = serde_json::to_string(&refused).unwrap();
    assert!(text.contains("tier"), "the property name is named: {text}");
    assert!(!text.contains(NEEDLE), "the value leaked: {text}");
}

#[tokio::test]
async fn a_nested_unknown_argument_is_refused_with_its_path() {
    let (server, _meta) = server().await;
    let tool = Recording::new();
    server.extensions().install(tool.clone()).unwrap();
    let refused = call_as(
        &server,
        "carol",
        "retrieve_probe",
        json!({ "query": "x", "options": { "deep": true, "wide": true } }),
    )
    .await;
    assert!(invalid(&refused));
    assert!(
        serde_json::to_string(&refused)
            .unwrap()
            .contains("options.wide")
    );
    assert_eq!(tool.entered(), 0);
    let accepted = call_as(
        &server,
        "carol",
        "retrieve_probe",
        json!({ "query": "x", "options": { "deep": true } }),
    )
    .await;
    assert_eq!(result_of(&accepted)["isError"], false);
}

#[tokio::test]
async fn an_extension_tools_bounds_and_enum_are_enforced_the_same_way() {
    let (server, _meta) = server().await;
    let tool = Recording::new();
    server.extensions().install(tool.clone()).unwrap();
    let bad = [
        json!({ "query": "" }),
        json!({ "query": "this query is longer than sixteen" }),
        json!({ "query": "x", "k": 0 }),
        json!({ "query": "x", "k": 11 }),
        json!({ "query": "x", "kinds": [] }),
        json!({ "query": "x", "kinds": ["episode", "fact", "episode"] }),
        json!({ "query": "x", "kinds": ["procedure"] }),
        json!({ "k": 3 }),
    ];
    for (n, arguments) in bad.into_iter().enumerate() {
        let messages = call_as(&server, "carol", "retrieve_probe", arguments.clone()).await;
        assert!(invalid(&messages), "case {n}: {arguments}");
    }
    assert_eq!(tool.entered(), 0);
    let good = call_as(
        &server,
        "carol",
        "retrieve_probe",
        json!({ "query": "x", "k": 10, "kinds": ["episode", "fact"] }),
    )
    .await;
    assert_eq!(result_of(&good)["isError"], false, "{good:?}");
    assert_eq!(tool.entered(), 1);
}

#[tokio::test]
async fn a_duplicate_item_under_unique_items_is_refused_with_the_later_items_path() {
    let (server, _meta) = server().await;
    let tool = Recording::new();
    server.extensions().install(tool.clone()).unwrap();
    let refused = call_as(
        &server,
        "carol",
        "retrieve_probe",
        json!({ "query": "x", "kinds": ["fact", "fact"] }),
    )
    .await;
    assert!(invalid(&refused), "{refused:?}");
    let text = serde_json::to_string(&refused).unwrap();
    assert!(text.contains("kinds.1"), "the later item is named: {text}");
    assert!(text.contains("duplicate item"), "{text}");
    assert_eq!(tool.entered(), 0);
    let accepted = call_as(
        &server,
        "carol",
        "retrieve_probe",
        json!({ "query": "x", "kinds": ["fact"] }),
    )
    .await;
    assert_eq!(result_of(&accepted)["isError"], false, "{accepted:?}");
    assert_eq!(tool.entered(), 1);
}

#[tokio::test]
async fn a_generated_tool_refuses_an_unknown_argument_too() {
    let (server, _meta) = server().await;
    let id = doc_ids()[0].to_string();
    let get = format!("get_{DOCTYPE}");

    let refused = call_as(&server, "bob", &get, json!({ "id": id, "tier": 3 })).await;
    assert!(invalid(&refused), "{refused:?}");

    let accepted = call_as(&server, "bob", &get, json!({ "id": id })).await;
    assert_eq!(result_of(&accepted)["isError"], false, "{accepted:?}");
    assert_eq!(result_of(&accepted)["structuredContent"]["title"], "one");
}

#[tokio::test]
async fn a_search_filter_on_an_unlisted_field_is_refused() {
    let (server, _meta) = server().await;
    let search = format!("search_{DOCTYPE}");
    let refused = call_as(
        &server,
        "alice",
        &search,
        json!({ "filter": { "no_such_field": "x" } }),
    )
    .await;
    assert!(invalid(&refused), "{refused:?}");
    let accepted = call_as(&server, "alice", &search, json!({})).await;
    assert_eq!(result_of(&accepted)["isError"], false, "{accepted:?}");
}
