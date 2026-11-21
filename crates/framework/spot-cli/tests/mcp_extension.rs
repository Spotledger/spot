//! WP-AI-04.T6: the extension-tool seam of the MCP service, over real HTTP and the test PostgreSQL
//! (`SPOT_TEST_PG_URL`; skipped with no database, never faked). A test module registered after
//! `mcp` installs fake tools on the `McpHandle` port exactly as a hosting service will (WP-AI-04.T7
//! does it for retrieval, agent memory and the remedy host), and the proof is over `POST /mcp`:
//! an installed tool is listed only for a principal whose `spec` is `Some` and runs as the caller,
//! a refused install leaves the handle unchanged, and a tool uninstalled while a call is in flight
//! ends that call with a tool error (never a transport error) and is gone from the next list.
//! The generic limits, deadline and audit rule of extension tools are proven against the server
//! core in `spot-mcp/tests/extension_tools.rs`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use common::{Env, Reply, admin, expect, fast_options, http, login};
use serde_json::{Value as Json, json};
use spot_cli::services::mcp::{
    ExtensionError, ExtensionSpec, ExtensionTool, InstallError, McpHandle,
};
use spot_cli::services::{RegisterFuture, Registration, SiteCtx};
use spot_mcp::BoxFuture;
use spot_mcp::principal::McpPrincipal;
use std::net::SocketAddr;
use tokio::sync::Notify;

const PASSWORD: &str = "cobalt-harbor-mango-4-Pebble";
const HIDDEN_FROM: &str = "nobody@example.com";

/// The one user id `whoami_probe` is listed for, once the test has learned it (any user before).
/// The principal's user is the engine's user id, not the sign-in e-mail, so the test records the
/// administrator's id from its first answer and the tool is then hidden from everybody else.
static LISTED_FOR: Mutex<Option<String>> = Mutex::new(None);

/// Answers who called; not listed for anyone but [`LISTED_FOR`] once that is set.
struct Whoami;

impl ExtensionTool for Whoami {
    fn name(&self) -> &str {
        "whoami_probe"
    }

    fn spec(&self, principal: &McpPrincipal) -> Option<ExtensionSpec> {
        let listed = LISTED_FOR.lock().unwrap_or_else(PoisonError::into_inner);
        if listed
            .as_deref()
            .is_some_and(|only| only != principal.user())
        {
            return None;
        }
        Some(ExtensionSpec::new(
            "Answers who called.",
            json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        ))
    }

    fn call<'a>(
        &'a self,
        principal: &'a McpPrincipal,
        _arguments: &'a Json,
    ) -> BoxFuture<'a, Result<Json, ExtensionError>> {
        Box::pin(async move { Ok(json!({ "who": principal.user() })) })
    }
}

/// Waits until released; when its service has stopped by then, answers a tool error.
struct Slow {
    started: Notify,
    release: Notify,
    stopped: AtomicBool,
}

impl ExtensionTool for Slow {
    fn name(&self) -> &str {
        "slow_probe"
    }

    fn spec(&self, _principal: &McpPrincipal) -> Option<ExtensionSpec> {
        Some(ExtensionSpec::new(
            "Waits for the test.",
            json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        ))
    }

    fn call<'a>(
        &'a self,
        _principal: &'a McpPrincipal,
        _arguments: &'a Json,
    ) -> BoxFuture<'a, Result<Json, ExtensionError>> {
        Box::pin(async move {
            self.started.notify_one();
            self.release.notified().await;
            if self.stopped.load(Ordering::SeqCst) {
                return Err(ExtensionError::failed(
                    "server.unavailable",
                    "the service has stopped",
                ));
            }
            Ok(json!({ "done": true }))
        })
    }
}

fn slow() -> &'static Arc<Slow> {
    static SLOW: OnceLock<Arc<Slow>> = OnceLock::new();
    SLOW.get_or_init(|| {
        Arc::new(Slow {
            started: Notify::new(),
            release: Notify::new(),
            stopped: AtomicBool::new(false),
        })
    })
}

static HANDLE: Mutex<Option<Arc<McpHandle>>> = Mutex::new(None);

/// The module a hosting service is: it takes the `McpHandle` port and installs its tools. It
/// uninstalls first, so a site built again on a reload does not collide with itself.
fn register_probes<'a, 'b>(ctx: &'a mut SiteCtx<'b>) -> RegisterFuture<'a> {
    Box::pin(async move {
        let Some(handle) = ctx.port::<McpHandle>() else {
            return Ok(());
        };
        for name in ["whoami_probe", "slow_probe"] {
            handle.uninstall_tool(name);
        }
        handle
            .install_tool(Arc::new(Whoami))
            .map_err(|e| spot_cli::Error(e.to_string()))?;
        let tool: Arc<dyn ExtensionTool> = slow().clone();
        handle
            .install_tool(tool)
            .map_err(|e| spot_cli::Error(e.to_string()))?;
        *HANDLE.lock().unwrap() = Some(handle);
        Ok(())
    })
}

/// After `mcp` (1060), as a hosting service starts after the module it plugs into.
static MODULES: [Registration; 1] = [Registration {
    name: "extension_probes",
    order: 1300,
    register: register_probes,
}];

fn rpc(id: u64, method: &str, params: Json) -> Json {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

async fn call(addr: SocketAddr, host: &str, token: &str, body: Json) -> Reply {
    http(addr, host, Some(token), "POST", "/mcp", &[], Some(body)).await
}

fn tool_names(reply: &Reply) -> Vec<String> {
    reply.body["result"]["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .filter_map(|t| t["name"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn error_code(reply: &Reply) -> &str {
    reply.body["error"]["data"]["code"]
        .as_str()
        .unwrap_or_default()
}

/// The session token of a user added before the host started (its staged credential is imported
/// on a reload, so the sign-in is retried).
async fn token_of(addr: SocketAddr, host: &str, user: &str) -> String {
    let mut last = None;
    for _ in 0..60 {
        let reply = login(addr, host, user, PASSWORD).await;
        if reply.status == 200 {
            return reply.body["data"]["token"].as_str().unwrap().to_owned();
        }
        last = Some(reply);
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    panic!("{user} never signed in: {:?}", last.map(|r| r.body));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_service_adds_a_tool_through_the_handle_and_removes_it_again() {
    let Some(env) = Env::new("").await else {
        return;
    };
    let secret = env.site();
    env.user(HIDDEN_FROM, PASSWORD);
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let running =
        spot_cli::start_with_modules(&env.root.0, false, fast_options(), None, &MODULES, async {
            let _ = stopped.await;
        })
        .await
        .unwrap();
    let addr = running.local_addrs()[0];
    let host = env.site.clone();
    let admin = admin(addr, &host, &secret).await;
    let handle = HANDLE
        .lock()
        .unwrap()
        .clone()
        .expect("the probe module installed its tools");

    // Listed for the administrator, and a call runs as that user.
    let listed = call(addr, &host, &admin, rpc(1, "tools/list", json!({}))).await;
    expect(&listed, 200);
    let names = tool_names(&listed);
    assert!(names.iter().any(|n| n == "whoami_probe"), "{names:?}");
    assert!(names.iter().any(|n| n == "get_task"), "{names:?}");
    let answered = call(
        addr,
        &host,
        &admin,
        rpc(
            2,
            "tools/call",
            json!({"name": "whoami_probe", "arguments": {}}),
        ),
    )
    .await;
    expect(&answered, 200);
    let who = answered.body["result"]["structuredContent"]["who"]
        .as_str()
        .unwrap_or_default();
    assert!(!who.is_empty(), "{:?}", answered.body);
    *LISTED_FOR.lock().unwrap_or_else(PoisonError::into_inner) = Some(who.to_owned());

    // Not listed for a principal whose spec is None; a call is the unknown-tool refusal.
    let nobody = token_of(addr, &host, HIDDEN_FROM).await;
    let theirs = call(addr, &host, &nobody, rpc(3, "tools/list", json!({}))).await;
    expect(&theirs, 200);
    assert!(!tool_names(&theirs).iter().any(|n| n == "whoami_probe"));
    let refused = call(
        addr,
        &host,
        &nobody,
        rpc(
            4,
            "tools/call",
            json!({"name": "whoami_probe", "arguments": {}}),
        ),
    )
    .await;
    assert_eq!(
        error_code(&refused),
        "mcp.unknown_tool",
        "{:?}",
        refused.body
    );

    // A refused install leaves the handle unchanged.
    assert_eq!(
        handle.install_tool(Arc::new(Whoami)),
        Err(InstallError::Duplicate)
    );
    struct Collides;
    impl ExtensionTool for Collides {
        fn name(&self) -> &str {
            "search_task"
        }
        fn spec(&self, _principal: &McpPrincipal) -> Option<ExtensionSpec> {
            None
        }
        fn call<'a>(
            &'a self,
            _principal: &'a McpPrincipal,
            _arguments: &'a Json,
        ) -> BoxFuture<'a, Result<Json, ExtensionError>> {
            Box::pin(async { Ok(Json::Null) })
        }
    }
    assert_eq!(
        handle.install_tool(Arc::new(Collides)),
        Err(InstallError::BadName)
    );
    let again = call(addr, &host, &admin, rpc(5, "tools/list", json!({}))).await;
    assert!(tool_names(&again).iter().any(|n| n == "whoami_probe"));
    assert!(tool_names(&again).iter().any(|n| n == "search_task"));

    // Uninstall while a call is in flight: the call ends with a tool error carrying the
    // service's code (an HTTP 200 JSON-RPC error, not a transport failure), the next list lacks
    // the tool and a new call is an unknown tool.
    let in_flight = {
        let (host, admin) = (host.clone(), admin.clone());
        tokio::spawn(async move {
            call(
                addr,
                &host,
                &admin,
                rpc(
                    6,
                    "tools/call",
                    json!({"name": "slow_probe", "arguments": {}}),
                ),
            )
            .await
        })
    };
    slow().started.notified().await;
    assert!(handle.uninstall_tool("slow_probe"));
    slow().stopped.store(true, Ordering::SeqCst);
    slow().release.notify_one();
    let ended = in_flight.await.unwrap();
    assert_eq!(ended.status, 200, "{:?}", ended.body);
    assert_eq!(error_code(&ended), "server.unavailable", "{:?}", ended.body);
    let after = call(addr, &host, &admin, rpc(7, "tools/list", json!({}))).await;
    assert!(!tool_names(&after).iter().any(|n| n == "slow_probe"));
    let gone = call(
        addr,
        &host,
        &admin,
        rpc(
            8,
            "tools/call",
            json!({"name": "slow_probe", "arguments": {}}),
        ),
    )
    .await;
    assert_eq!(error_code(&gone), "mcp.unknown_tool", "{:?}", gone.body);

    let _ = stop.send(());
    let _ = running.join().await;
}
