//! The limits: input size, row cap, depth cap, timeout and per-principal rate limiting, and the
//! proof that no handle is held across calls.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::json;
use spot_app_api::context::Invocation;
use spot_coordinator::RateLimit;
use spot_mcp::principal::Credential;
use spot_mcp::query::QueryPort;
use spot_mcp::{Limits, McpError};
use spot_storage_contract::{Catalog, Query, Rows};
use time::OffsetDateTime;

use support::result_of;

fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[tokio::test]
async fn an_oversized_message_is_refused() {
    let limits = Limits {
        max_input_bytes: 16,
        ..Limits::default()
    };
    let (server, _meta) = support::server_with(limits, None).await;
    let body = support::request(1, "tools/list", json!({}));
    let messages = server
        .handle_text(&body, Some(&Credential::bearer("alice")), now())
        .await;
    assert_eq!(
        support::error_of(&messages)["data"]["code"],
        "mcp.limit_exceeded"
    );
}

#[tokio::test]
async fn the_row_cap_is_enforced() {
    let limits = Limits {
        max_rows: 1,
        ..Limits::default()
    };
    let (server, _meta) = support::server_with(limits, None).await;
    let body = support::request(
        1,
        "tools/call",
        json!({ "name": format!("search_{}", support::DOCTYPE), "arguments": {} }),
    );
    let messages = server
        .handle_text(&body, Some(&Credential::bearer("bob")), now())
        .await;
    let rows = result_of(&messages)["structuredContent"]["rows"]
        .as_array()
        .unwrap()
        .len();
    assert_eq!(rows, 1);
}

#[tokio::test]
async fn a_too_deep_query_is_refused() {
    let (server, _meta) = support::server().await;
    let mut filter = json!({ "field": "title" });
    for _ in 0..40 {
        filter = json!({ "not": filter });
    }
    let body = support::request(
        1,
        "tools/call",
        json!({
            "name": "query",
            "arguments": {
                "doctype": support::DOCTYPE,
                "select": [ { "expr": { "field": "id" }, "as": "id" } ],
                "filter": filter,
            },
        }),
    );
    let messages = server
        .handle_text(&body, Some(&Credential::bearer("bob")), now())
        .await;
    assert_eq!(
        support::error_of(&messages)["data"]["code"],
        "mcp.limit_exceeded"
    );
}

#[tokio::test]
async fn a_slow_call_times_out() {
    let limits = Limits {
        call_timeout: Duration::from_millis(20),
        ..Limits::default()
    };
    let (server, _meta) = support::server_with(limits, Some(Arc::new(SlowPort))).await;
    let body = support::request(
        1,
        "tools/call",
        json!({ "name": format!("search_{}", support::DOCTYPE), "arguments": {} }),
    );
    let messages = server
        .handle_text(&body, Some(&Credential::bearer("bob")), now())
        .await;
    assert_eq!(support::error_of(&messages)["data"]["code"], "mcp.timeout");
}

#[tokio::test]
async fn a_spent_rate_limit_is_refused() {
    let limits = Limits {
        rate: RateLimit {
            capacity: 1,
            per: Duration::from_secs(3600),
        },
        ..Limits::default()
    };
    let (server, _meta) = support::server_with(limits, None).await;
    let body = support::request(
        1,
        "tools/call",
        json!({ "name": format!("search_{}", support::DOCTYPE), "arguments": {} }),
    );
    let first = server
        .handle_text(&body, Some(&Credential::bearer("bob")), now())
        .await;
    assert!(support::error_of(&first).is_null());
    let second = server
        .handle_text(&body, Some(&Credential::bearer("bob")), now())
        .await;
    assert_eq!(
        support::error_of(&second)["data"]["code"],
        "mcp.rate_limited"
    );
}

#[test]
fn no_read_handle_is_held_across_calls() {
    let runtime = runtime();
    let world = runtime.block_on(support::world());
    let counting = Arc::new(CountingPort::new(world.port.clone()));
    let (server, _meta) = runtime.block_on(support::server_with(
        Limits::default(),
        Some(counting.clone()),
    ));
    let body = support::request(
        1,
        "tools/call",
        json!({ "name": format!("search_{}", support::DOCTYPE), "arguments": {} }),
    );
    for _ in 0..3 {
        let messages =
            runtime.block_on(server.handle_text(&body, Some(&Credential::bearer("bob")), now()));
        assert!(result_of(&messages)["structuredContent"]["rows"].is_array());
        // Every read finished before the call returned: no handle is held across calls.
        assert_eq!(counting.active(), 0);
        assert_eq!(counting.peak(), 1);
    }
}

/// A query port that sleeps, so the per-call timeout has something to interrupt.
struct SlowPort;

impl QueryPort for SlowPort {
    fn read<'a>(
        &'a self,
        _inv: &'a Invocation,
        _catalog: Arc<Catalog>,
        _query: &'a Query,
    ) -> spot_mcp::BoxFuture<'a, Result<Rows, McpError>> {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok(Rows::default())
        })
    }
}

/// A query port that counts concurrent reads, to prove none is held across calls.
struct CountingPort {
    inner: Arc<support::BackendPort>,
    active: AtomicUsize,
    peak: AtomicUsize,
}

impl CountingPort {
    fn new(inner: Arc<support::BackendPort>) -> Self {
        Self {
            inner,
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        }
    }

    fn active(&self) -> usize {
        self.active.load(Ordering::SeqCst)
    }

    fn peak(&self) -> usize {
        self.peak.load(Ordering::SeqCst)
    }
}

impl QueryPort for CountingPort {
    fn read<'a>(
        &'a self,
        inv: &'a Invocation,
        catalog: Arc<Catalog>,
        query: &'a Query,
    ) -> spot_mcp::BoxFuture<'a, Result<Rows, McpError>> {
        Box::pin(async move {
            let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            let result = self.inner.read(inv, catalog, query).await;
            self.active.fetch_sub(1, Ordering::SeqCst);
            result
        })
    }
}
