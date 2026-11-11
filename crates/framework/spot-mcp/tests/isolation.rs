//! Principal isolation: a tool never returns a value the calling principal cannot read.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use proptest::prelude::*;
use serde_json::Value as Json;
use spot_mcp::principal::Credential;
use time::OffsetDateTime;

const USERS: [&str; 3] = ["alice", "bob", "carol"];

fn collect_strings(value: &Json, out: &mut Vec<String>) {
    match value {
        Json::String(s) => out.push(s.clone()),
        Json::Array(items) => {
            for item in items {
                collect_strings(item, out);
            }
        }
        Json::Object(map) => {
            for item in map.values() {
                collect_strings(item, out);
            }
        }
        _ => {}
    }
}

fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
}

#[test]
fn no_tool_output_contains_a_value_the_principal_cannot_read() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let (server, _meta) = runtime.block_on(support::server());
    let engine = support::engine();
    let principals = support::principals();

    proptest!(|(p in 0..3usize, d in 0..3usize)| {
        let name = USERS[p];
        let principal = principals.get(name).unwrap().clone();
        let can_salary = support::may_read_field(&engine, principal.permissions(), "salary");
        let can_secret = support::may_read_field(&engine, principal.permissions(), "secret");

        // A single document.
        let body = support::request(
            1,
            "tools/call",
            serde_json::json!({
                "name": format!("get_{}", support::DOCTYPE),
                "arguments": { "id": support::doc_ids()[d].to_string() },
            }),
        );
        let messages = runtime.block_on(server.handle_text(
            &body,
            Some(&Credential::bearer(name)),
            now(),
        ));
        let mut strings = Vec::new();
        collect_strings(&support::result_of(&messages)["structuredContent"], &mut strings);
        assert_values(&strings, name, can_salary, can_secret);

        // Every document at once.
        let body = support::request(
            1,
            "tools/call",
            serde_json::json!({ "name": format!("search_{}", support::DOCTYPE), "arguments": {} }),
        );
        let messages = runtime.block_on(server.handle_text(
            &body,
            Some(&Credential::bearer(name)),
            now(),
        ));
        let mut strings = Vec::new();
        collect_strings(&support::result_of(&messages)["structuredContent"], &mut strings);
        assert_values(&strings, name, can_salary, can_secret);
    });
}

fn assert_values(strings: &[String], user: &str, can_salary: bool, can_secret: bool) {
    for value in strings {
        if support::salary_values().contains(&value.as_str()) {
            assert!(can_salary, "a salary value leaked to {user}");
        }
        if support::secret_values().contains(&value.as_str()) {
            assert!(can_secret, "a secret value leaked to {user}");
        }
    }
}

#[test]
fn a_reader_cannot_write() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let (server, _meta) = runtime.block_on(support::server());
    let body = support::request(
        1,
        "tools/call",
        serde_json::json!({
            "name": format!("create_{}_draft", support::DOCTYPE),
            "arguments": { "title": "smuggled" },
        }),
    );
    let messages =
        runtime.block_on(server.handle_text(&body, Some(&Credential::bearer("alice")), now()));
    let error = support::error_of(&messages);
    assert_eq!(error["data"]["code"], "mcp.unknown_tool");
}

#[test]
fn a_writer_may_create_a_draft() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let (server, _meta) = runtime.block_on(support::server());
    let body = support::request(
        1,
        "tools/call",
        serde_json::json!({
            "name": format!("create_{}_draft", support::DOCTYPE),
            "arguments": { "title": "a new note" },
        }),
    );
    let messages =
        runtime.block_on(server.handle_text(&body, Some(&Credential::bearer("carol")), now()));
    let result = support::result_of(&messages);
    assert_eq!(result["isError"], false);
    assert!(result["structuredContent"]["id"].is_string());
}
