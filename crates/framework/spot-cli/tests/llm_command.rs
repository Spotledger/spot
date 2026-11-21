//! WP-AI-03.T9: `spot llm`. The command runs over the in-memory store (the production path only
//! swaps the store for the site's database), a real egress guard with a scripted resolver and a
//! scripted egress sender that counts every request, a recording audit trail and a scripted key
//! source. The command module is driven without a process, as the other command tests do.
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    dead_code,
    unused_imports
)]

use std::collections::HashMap;
use std::io::Write;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use http::{HeaderMap, StatusCode};
use spot_cli::services::egress::SharedGuard;
use spot_cli::services::llm::gateway::{BuiltinProviders, Deps, LlmGateway, TransportFactory};
use spot_cli::services::llm::prompt::DenyAllReader;
use spot_cli::services::llm::store::Store;
use spot_core::ManualClock;
use spot_document::secrets::SecretVault;
use spot_egress::{
    Authorized, EgressError, EgressPolicy, Guard, HttpRequest, InMemorySink, Purpose, Resolver,
    Response as WireResponse, Sender,
};
use spot_llm::transport::HttpTransport;
use spot_llm::transport::egress::EgressTransport;
use spot_redact::{RedactionPolicy, Redactor};
use spot_secrets::{MemoryKeyring, SecretBox};
use time::macros::datetime;

/// The command registry, driven without a process.
#[path = "../src/commands/mod.rs"]
mod commands;

use commands::llm::{AuditTrail, KeyInput, Services};
use commands::{CommandCtx, Exit};

const KEY: &str = "sk-test-CLI-0123456789";

const POLICY: &str = r#"
[tenants.acme]
[[tenants.acme.allow]]
host = "api.openrouter.test"
purpose = "llm"

[tenants.globex]
[[tenants.globex.allow]]
host = "api.openrouter.test"
purpose = "llm"

[tenants.rebinder]
[[tenants.rebinder.allow]]
host = "rebind.test"
purpose = "llm"
"#;

const CHAT_OK: &str = r#"{"id":"or-1","choices":[{"index":0,"message":{"role":"assistant","content":"pong"},"finish_reason":"stop"}],"usage":{"prompt_tokens":12,"completion_tokens":4}}"#;

// --- the world --------------------------------------------------------------------------------

struct Resolve(HashMap<String, Vec<IpAddr>>);

#[async_trait::async_trait]
impl Resolver for Resolve {
    async fn resolve(&self, host: &str) -> Result<Vec<IpAddr>, std::io::Error> {
        self.0
            .get(host)
            .cloned()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "unknown host"))
    }
}

/// The scripted egress sender: counts requests and keeps their headers and bodies.
struct Stub {
    calls: AtomicUsize,
    seen: Mutex<Vec<(String, String)>>,
}

#[async_trait::async_trait]
impl Sender for Stub {
    async fn send_request(
        &self,
        request: &HttpRequest,
        _authorized: &Authorized,
        _max_body: u64,
    ) -> Result<WireResponse, EgressError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let headers = request
            .headers
            .iter()
            .map(|(name, value)| format!("{}: {}", name, value.to_str().unwrap_or_default()))
            .collect::<Vec<_>>()
            .join("\n");
        let body = request
            .body
            .as_ref()
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .unwrap_or_default();
        self.seen.lock().unwrap().push((headers, body));
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        Ok(WireResponse {
            status: StatusCode::OK,
            headers,
            body: CHAT_OK.as_bytes().to_vec(),
        })
    }
}

struct ViaStub(Arc<Stub>);

impl TransportFactory for ViaStub {
    fn transport(&self, guard: Arc<Guard>, tenant: &str) -> Arc<dyn HttpTransport> {
        Arc::new(EgressTransport::new(
            guard,
            Arc::clone(&self.0) as Arc<dyn Sender>,
            tenant,
            Purpose::Llm,
        ))
    }
}

/// A recording audit trail that can be told to fail.
#[derive(Default)]
struct Trail {
    records: Mutex<Vec<(String, String, Vec<String>)>>,
    fail: AtomicBool,
}

impl AuditTrail for Trail {
    fn record(&self, kind: &str, tenant: &str, fields: &[String]) -> Result<(), String> {
        if self.fail.load(Ordering::SeqCst) {
            return Err("secrets_host.audit_unavailable: the audit chain is not available".into());
        }
        self.records
            .lock()
            .unwrap()
            .push((kind.to_owned(), tenant.to_owned(), fields.to_vec()));
        Ok(())
    }
}

/// A key source holding one line, or no terminal at all.
struct Keys(Option<String>);

impl KeyInput for Keys {
    fn read_key(&mut self, _from_stdin: bool) -> Result<String, String> {
        self.0
            .take()
            .ok_or_else(|| "no terminal; use --stdin".to_owned())
    }
}

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Sink {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

struct Rig {
    gw: LlmGateway,
    store: Store,
    clock: Arc<ManualClock>,
    stub: Arc<Stub>,
    trail: Trail,
    /// Everything every command printed, for the no-secret sweeps.
    printed: Mutex<String>,
}

impl Rig {
    fn new() -> Rig {
        let store = Store::memory();
        let clock = Arc::new(ManualClock::new(datetime!(2026-11-21 12:00:00 UTC)));
        let vault: Arc<dyn SecretVault> =
            Arc::new(SecretBox::new(MemoryKeyring::new("llm-cmd-test").unwrap()));
        let public: IpAddr = "93.184.216.34".parse().unwrap();
        let mut hosts = HashMap::new();
        hosts.insert("api.openrouter.test".to_owned(), vec![public]);
        // A name that points inside the network.
        hosts.insert("rebind.test".to_owned(), vec!["10.0.0.5".parse().unwrap()]);
        let guard = Arc::new(Guard::new(
            EgressPolicy::from_toml(POLICY).unwrap(),
            Arc::new(Resolve(hosts)) as Arc<dyn Resolver>,
            Arc::new(InMemorySink::new()),
            Arc::clone(&clock) as Arc<dyn spot_core::Clock>,
        ));
        let stub = Arc::new(Stub {
            calls: AtomicUsize::new(0),
            seen: Mutex::new(Vec::new()),
        });
        let gw = LlmGateway::new(Deps {
            site: "site-1".to_owned(),
            store: store.clone(),
            vault,
            guard: Arc::new(SharedGuard::new(guard)),
            clock: Arc::clone(&clock) as Arc<dyn spot_core::Clock>,
            providers: Arc::new(BuiltinProviders),
            transports: Arc::new(ViaStub(Arc::clone(&stub))),
            redactor: Arc::new(Redactor::from_policy(&RedactionPolicy::default()).unwrap()),
            reader: Arc::new(DenyAllReader),
            max_retries: 0,
            retention_days: 1,
        });
        Rig {
            gw,
            store,
            clock,
            stub,
            trail: Trail::default(),
            printed: Mutex::new(String::new()),
        }
    }

    /// Runs `spot llm <args>` with `key` as the one line the key source holds.
    async fn run_with_key(&self, key: Option<&str>, args: &[&str]) -> (Exit, String, String) {
        let matches = commands::llm::command()
            .try_get_matches_from(std::iter::once("llm").chain(args.iter().copied()))
            .unwrap();
        let (out, err) = (Sink::default(), Sink::default());
        let mut ctx = CommandCtx::new(
            PathBuf::from("/nonexistent"),
            Arc::clone(&self.clock) as Arc<dyn spot_core::Clock>,
            Box::new(out.clone()),
            Box::new(err.clone()),
        );
        let mut keys = Keys(key.map(str::to_owned));
        let mut services = Services {
            gateway: &self.gw,
            store: &self.store,
            audit: &self.trail,
            keys: &mut keys,
        };
        let exit = commands::llm::execute(&matches, &mut ctx, &mut services)
            .await
            .unwrap();
        let (out, err) = (out.text(), err.text());
        self.printed
            .lock()
            .unwrap()
            .push_str(&format!("{out}{err}"));
        (exit, out, err)
    }

    async fn run(&self, args: &[&str]) -> (Exit, String, String) {
        self.run_with_key(None, args).await
    }

    fn printed(&self) -> String {
        self.printed.lock().unwrap().clone()
    }
}

/// The arguments of a good `set` for `tenant`.
fn set_args(tenant: &str) -> Vec<&str> {
    vec![
        "set",
        "--tenant",
        tenant,
        "--kind",
        "openrouter",
        "--endpoint",
        "https://api.openrouter.test",
        "--model",
        "meta/llama-test",
        "--allow-host",
        "api.openrouter.test",
        "--price-in",
        "1000000",
        "--price-out",
        "1000000",
    ]
}

async fn ok(rig: &Rig, args: &[&str]) -> String {
    let (exit, out, err) = rig.run(args).await;
    assert_eq!(exit, Exit::Ok, "{args:?}\nout: {out}\nerr: {err}");
    out
}

// --- tests ------------------------------------------------------------------------------------

#[tokio::test]
async fn set_then_show_round_trips_and_shows_no_key() {
    let r = Rig::new();
    let out = ok(&r, &set_args("acme")).await;
    assert!(
        out.contains("stored the configuration of tenant `acme`"),
        "{out}"
    );
    ok(&r, &set_args("globex")).await;

    let text = ok(&r, &["show", "--tenant", "acme"]).await;
    for expected in [
        "tenant: acme",
        "kind: openrouter",
        "endpoint: https://api.openrouter.test",
        "model: meta/llama-test",
        "allowed hosts: api.openrouter.test",
        "tasks: chat",
        "input 1000000, output 1000000",
        "budget: none",
        "embedding model: none",
        "key: absent",
    ] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }

    let json = ok(&r, &["show", "--tenant", "acme", "--json"]).await;
    let value: serde_json::Value = serde_json::from_str(json.trim()).unwrap();
    assert_eq!(value["tenant"], "acme");
    assert_eq!(value["kind"], "openrouter");
    assert_eq!(value["allowed_hosts"][0], "api.openrouter.test");
    assert_eq!(value["price"]["input_micro_per_million"], 1_000_000);
    assert_eq!(value["key"], "absent");

    // Without a tenant, every one is listed.
    let all = ok(&r, &["show"]).await;
    assert!(
        all.contains("tenant: acme") && all.contains("tenant: globex"),
        "{all}"
    );
}

#[tokio::test]
async fn an_unacceptable_set_is_refused_with_the_validators_sentence_and_changes_nothing() {
    let r = Rig::new();
    let with = |changes: &[(&str, &str)]| -> Vec<String> {
        let mut args: Vec<String> = set_args("acme").iter().map(|a| (*a).to_owned()).collect();
        for (flag, value) in changes {
            let at = args.iter().position(|a| a.as_str() == *flag).unwrap();
            args[at + 1] = (*value).to_owned();
        }
        args
    };
    let cases: Vec<(Vec<String>, &str, &str)> = vec![
        (
            with(&[("--endpoint", "https://user:hunter2@api.openrouter.test")]),
            "must not carry credentials",
            "hunter2",
        ),
        (
            with(&[("--endpoint", "https://api.openrouter.test/v1?token=s3cret")]),
            "must not carry a query",
            "s3cret",
        ),
        (
            with(&[("--allow-host", "other.test")]),
            "not on the tenant's allow-list",
            "other.test",
        ),
    ];
    for (args, sentence, planted) in cases {
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let (exit, out, err) = r.run(&refs).await;
        assert_eq!(exit, Exit::Refused, "{sentence}");
        assert!(err.contains("llm_host.config_invalid"), "{err}");
        assert!(err.contains(sentence), "{err}");
        assert!(
            !err.contains(planted) && !out.contains(planted),
            "{err}{out}"
        );
    }

    // A budget with no price, an unknown task, and embedding flags that do not fit.
    let no_price = [
        "set",
        "--tenant",
        "acme",
        "--kind",
        "openrouter",
        "--endpoint",
        "https://api.openrouter.test",
        "--model",
        "m",
        "--allow-host",
        "api.openrouter.test",
        "--budget-day",
        "100",
    ];
    let (exit, _, err) = r.run(&no_price).await;
    assert_eq!(exit, Exit::Refused);
    assert!(err.contains("a budget needs a price"), "{err}");

    let mut bad_task = set_args("acme");
    bad_task.extend(["--task", "Bad Task"]);
    let (exit, _, err) = r.run(&bad_task).await;
    assert_eq!(exit, Exit::Refused);
    assert!(err.contains("task name may only hold"), "{err}");
    assert!(!err.contains("Bad Task"), "{err}");

    let anthropic_embeds = [
        "set",
        "--tenant",
        "acme",
        "--kind",
        "anthropic",
        "--endpoint",
        "https://api.openrouter.test",
        "--model",
        "m",
        "--allow-host",
        "api.openrouter.test",
        "--embedding-model",
        "e",
    ];
    let (exit, _, err) = r.run(&anthropic_embeds).await;
    assert_eq!(exit, Exit::Refused);
    assert!(err.contains("no embeddings endpoint"), "{err}");

    let mut dims_only = set_args("acme");
    dims_only.extend(["--embedding-dimensions", "64"]);
    let (exit, _, err) = r.run(&dims_only).await;
    assert_eq!(exit, Exit::Refused);
    assert!(err.contains("need an embedding model"), "{err}");

    // Nothing was stored or audited.
    assert!(r.store.tenants().await.unwrap().is_empty());
    assert!(r.trail.records.lock().unwrap().is_empty());
}

#[tokio::test]
async fn the_embedding_flags_reach_the_configuration() {
    let r = Rig::new();
    let mut args = set_args("acme");
    args.extend([
        "--embedding-model",
        "text-embedding-3-small",
        "--embedding-dimensions",
        "64",
    ]);
    ok(&r, &args).await;
    let json = ok(&r, &["show", "--tenant", "acme", "--json"]).await;
    let value: serde_json::Value = serde_json::from_str(json.trim()).unwrap();
    assert_eq!(value["embedding_model"], "text-embedding-3-small");
    assert_eq!(value["embedding_dimensions"], 64);
    let text = ok(&r, &["show", "--tenant", "acme"]).await;
    assert!(
        text.contains("embedding model: text-embedding-3-small (width 64)"),
        "{text}"
    );
}

#[tokio::test]
async fn the_key_is_stored_write_only_and_appears_nowhere() {
    let r = Rig::new();
    ok(&r, &set_args("acme")).await;

    // A tenant with no configuration has nothing to attach a key to.
    let (exit, _, err) = r
        .run_with_key(Some(KEY), &["key", "set", "--tenant", "nobody", "--stdin"])
        .await;
    assert_eq!(exit, Exit::Refused);
    assert!(err.contains("llm_host.not_configured"), "{err}");

    // No terminal and no --stdin: a usage error, not a guess.
    let (exit, _, err) = r.run(&["key", "set", "--tenant", "acme"]).await;
    assert_eq!(exit, Exit::Usage);
    assert!(err.contains("no terminal"), "{err}");

    let (exit, out, err) = r
        .run_with_key(Some(KEY), &["key", "set", "--tenant", "acme", "--stdin"])
        .await;
    assert_eq!(exit, Exit::Ok, "{err}");
    assert!(out.contains("write-only"), "{out}");
    let shown = ok(&r, &["show", "--tenant", "acme"]).await;
    assert!(shown.contains("key: present"), "{shown}");
    let json = ok(&r, &["show", "--tenant", "acme", "--json"]).await;
    assert!(json.contains(r#""key":"present""#), "{json}");

    // Re-setting the configuration keeps the key.
    let mut changed = set_args("acme");
    changed[8] = "meta/other-model";
    ok(&r, &changed).await;
    assert!(
        ok(&r, &["show", "--tenant", "acme"])
            .await
            .contains("key: present")
    );

    // Sealed at rest: the stored bytes are not the text.
    let sealed = r.store.key("acme").await.unwrap().unwrap();
    assert!(!String::from_utf8_lossy(&sealed).contains(KEY));

    // The key is used, in the provider's own header, and shows up nowhere the operator reads.
    let (exit, out, _) = r.run(&["test", "--tenant", "acme"]).await;
    assert_eq!(exit, Exit::Ok);
    assert_eq!(out.trim(), "ok");
    assert!(r.stub.seen.lock().unwrap()[0].0.contains(KEY));
    ok(&r, &["usage", "--tenant", "acme"]).await;
    ok(&r, &["sends", "--tenant", "acme"]).await;
    assert!(!r.printed().contains(KEY), "{}", r.printed());
    let audited = format!("{:?}", r.trail.records.lock().unwrap());
    assert!(!audited.contains(KEY), "{audited}");
    let sends = format!("{:?}", r.gw.sends("acme", 10).await.unwrap());
    assert!(!sends.contains(KEY), "{sends}");
}

#[tokio::test]
async fn key_remove_makes_the_key_absent_and_the_next_call_refuses() {
    let r = Rig::new();
    ok(&r, &set_args("acme")).await;
    let (exit, _, _) = r
        .run_with_key(Some(KEY), &["key", "set", "--tenant", "acme", "--stdin"])
        .await;
    assert_eq!(exit, Exit::Ok);

    let out = ok(&r, &["key", "remove", "--tenant", "acme"]).await;
    assert!(out.contains("is removed"), "{out}");
    assert!(
        ok(&r, &["show", "--tenant", "acme"])
            .await
            .contains("key: absent")
    );
    // The configuration stayed.
    assert!(
        ok(&r, &["show", "--tenant", "acme"])
            .await
            .contains("tenant: acme")
    );

    let (exit, out, _) = r.run(&["test", "--tenant", "acme"]).await;
    assert_eq!(exit, Exit::Refused);
    assert_eq!(out.trim(), "llm_host.key_missing");
    assert_eq!(r.stub.calls.load(Ordering::SeqCst), 0);

    // Removing again is not an error.
    let out = ok(&r, &["key", "remove", "--tenant", "acme"]).await;
    assert!(out.contains("had no stored key"), "{out}");
}

#[tokio::test]
async fn usage_and_sends_print_rows_without_any_text() {
    let r = Rig::new();
    ok(&r, &set_args("acme")).await;
    r.run_with_key(Some(KEY), &["key", "set", "--tenant", "acme", "--stdin"])
        .await;
    assert_eq!(ok(&r, &["test", "--tenant", "acme"]).await.trim(), "ok");

    let usage = ok(&r, &["usage", "--tenant", "acme"]).await;
    for expected in [
        "2026-11-21",
        "task=chat",
        "provider=openrouter",
        "model=meta/llama-test",
        "requests=1",
        "input=12",
        "output=4",
        "cost_micro=16",
        "total cost: 16 micro-units",
    ] {
        assert!(usage.contains(expected), "{expected}\n{usage}");
    }
    let json = ok(
        &r,
        &["usage", "--tenant", "acme", "--json", "--since", "7d"],
    )
    .await;
    let row: serde_json::Value = serde_json::from_str(json.lines().next().unwrap()).unwrap();
    assert_eq!(row["day"], "2026-11-21");
    assert_eq!(row["input"], 12);
    assert_eq!(row["cost_micro"], 16);

    let sends = ok(&r, &["sends", "--tenant", "acme", "--limit", "5"]).await;
    for expected in [
        " sent ",
        "task=chat",
        "endpoint=https://api.openrouter.test",
        "provider=openrouter",
        "user=operator",
        "purpose=connection test",
        "tokens_in=12",
        "tokens_out=4",
    ] {
        assert!(sends.contains(expected), "{expected}\n{sends}");
    }
    // The word the test sent and the answer it got are in no output.
    for printed in [&usage, &json, &sends] {
        assert!(
            !printed.contains("ping") && !printed.contains("pong"),
            "{printed}"
        );
    }

    // A tenant with nothing recorded, and a bad window.
    ok(&r, &set_args("globex")).await;
    assert!(
        ok(&r, &["usage", "--tenant", "globex"])
            .await
            .contains("no usage recorded")
    );
    assert!(
        ok(&r, &["sends", "--tenant", "globex"])
            .await
            .contains("no send recorded")
    );
    let (exit, _, err) = r
        .run(&["usage", "--tenant", "acme", "--since", "soon"])
        .await;
    assert_eq!(exit, Exit::Usage);
    assert!(err.contains("--since"), "{err}");
}

#[tokio::test]
async fn the_connection_test_goes_through_the_guard_and_the_budget() {
    let r = Rig::new();

    // Reachable and keyed: ok, one request.
    ok(&r, &set_args("acme")).await;
    r.run_with_key(Some(KEY), &["key", "set", "--tenant", "acme", "--stdin"])
        .await;
    let (exit, out, err) = r.run(&["test", "--tenant", "acme"]).await;
    assert_eq!((exit, out.trim(), err.as_str()), (Exit::Ok, "ok", ""));
    assert_eq!(r.stub.calls.load(Ordering::SeqCst), 1);

    // A name that resolves to a private address: the egress guard refuses, the code only.
    let private = [
        "set",
        "--tenant",
        "rebinder",
        "--kind",
        "openrouter",
        "--endpoint",
        "https://rebind.test",
        "--model",
        "m",
        "--allow-host",
        "rebind.test",
    ];
    ok(&r, &private).await;
    r.run_with_key(
        Some(KEY),
        &["key", "set", "--tenant", "rebinder", "--stdin"],
    )
    .await;
    let (exit, out, err) = r.run(&["test", "--tenant", "rebinder"]).await;
    assert_eq!(exit, Exit::Refused);
    assert_eq!(out.trim(), "llm_host.endpoint_denied");
    assert_eq!(err, "");
    assert_eq!(r.stub.calls.load(Ordering::SeqCst), 1, "nothing was sent");

    // A budget the first test call nearly spends: the second is refused and sends nothing.
    let mut capped = set_args("globex");
    capped.extend(["--budget-day", "17"]);
    ok(&r, &capped).await;
    r.run_with_key(Some(KEY), &["key", "set", "--tenant", "globex", "--stdin"])
        .await;
    assert_eq!(ok(&r, &["test", "--tenant", "globex"]).await.trim(), "ok");
    let (exit, out, _) = r.run(&["test", "--tenant", "globex"]).await;
    assert_eq!(exit, Exit::Refused);
    assert_eq!(out.trim(), "llm_host.budget_exceeded");
    assert_eq!(r.stub.calls.load(Ordering::SeqCst), 2);

    // A tenant that does not exist is a refusal too.
    let (exit, _, err) = r.run(&["test", "--tenant", "nobody"]).await;
    assert_eq!(exit, Exit::Refused);
    assert!(err.contains("llm_host.not_configured"), "{err}");
}

#[tokio::test]
async fn missing_tenants_are_refused_and_bad_flags_do_not_parse() {
    let r = Rig::new();
    for verb in ["usage", "sends", "test"] {
        let (exit, _, err) = r.run(&[verb, "--tenant", "nobody"]).await;
        assert_eq!(exit, Exit::Refused, "{verb}");
        assert!(err.contains("llm_host.not_configured"), "{verb}: {err}");
    }
    let (exit, _, err) = r.run(&["show", "--tenant", "nobody"]).await;
    assert_eq!(exit, Exit::Refused);
    assert!(err.contains("llm_host.not_configured"), "{err}");
    let (exit, out, _) = r.run(&["show"]).await;
    assert_eq!((exit, out.trim()), (Exit::Ok, "no tenant is configured"));

    // Usage errors never reach the command: clap refuses them (exit 1 in the binary).
    let parse = |args: &[&str]| {
        commands::llm::command()
            .try_get_matches_from(std::iter::once("llm").chain(args.iter().copied()))
    };
    assert!(parse(&["set", "--tenant", "a"]).is_err());
    assert!(
        parse(&[
            "set",
            "--tenant",
            "a",
            "--kind",
            "nope",
            "--endpoint",
            "e",
            "--model",
            "m"
        ])
        .is_err()
    );
    let mut both = set_args("a");
    both.extend(["--budget-day", "1", "--budget-month", "1"]);
    assert!(parse(&both).is_err());
    assert!(parse(&["key"]).is_err());
    assert!(parse(&["key", "set"]).is_err());
    assert!(parse(&["sends", "--tenant", "a", "--limit", "0"]).is_err());
    // The key is never an argument or an environment variable: no such flag exists.
    assert!(parse(&["key", "set", "--tenant", "a", "--key", KEY]).is_err());
    assert!(parse(&["key", "set", "--tenant", "a", "--value", KEY]).is_err());
}

#[tokio::test]
async fn a_second_tenant_is_untouched() {
    let r = Rig::new();
    ok(&r, &set_args("acme")).await;
    ok(&r, &set_args("globex")).await;
    let before = ok(&r, &["show", "--tenant", "globex", "--json"]).await;

    r.run_with_key(Some(KEY), &["key", "set", "--tenant", "acme", "--stdin"])
        .await;
    let mut changed = set_args("acme");
    changed[8] = "meta/other-model";
    ok(&r, &changed).await;
    ok(&r, &["key", "remove", "--tenant", "acme"]).await;

    assert_eq!(
        ok(&r, &["show", "--tenant", "globex", "--json"]).await,
        before
    );
    assert!(
        ok(&r, &["show", "--tenant", "globex"])
            .await
            .contains("key: absent")
    );
    assert_eq!(
        r.trail
            .records
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, t, _)| t == "globex")
            .count(),
        1
    );
}

#[tokio::test]
async fn every_change_is_audited_by_field_name_and_a_failed_audit_changes_nothing() {
    let r = Rig::new();
    ok(&r, &set_args("acme")).await;
    let mut changed = set_args("acme");
    changed[8] = "meta/other-model";
    ok(&r, &changed).await;
    r.run_with_key(Some(KEY), &["key", "set", "--tenant", "acme", "--stdin"])
        .await;
    ok(&r, &["key", "remove", "--tenant", "acme"]).await;

    {
        let records = r.trail.records.lock().unwrap();
        assert_eq!(records.len(), 4);
        // The first set names every field of a new tenant; the second only what changed.
        assert_eq!(records[0].0, "llm_config_set");
        assert!(
            records[0].2.iter().any(|f| f == "endpoint")
                && records[0].2.iter().any(|f| f == "model")
        );
        assert_eq!(
            records[1],
            (
                "llm_config_set".to_owned(),
                "acme".to_owned(),
                vec!["model".to_owned()]
            )
        );
        assert_eq!(
            records[2],
            (
                "llm_key_set".to_owned(),
                "acme".to_owned(),
                vec!["key".to_owned()]
            )
        );
        assert_eq!(
            records[3],
            (
                "llm_key_removed".to_owned(),
                "acme".to_owned(),
                vec!["key".to_owned()]
            )
        );
        // Names only: no value of any field, no key.
        let printed = format!("{records:?}");
        for value in [
            "https://api.openrouter.test",
            "meta/other-model",
            "meta/llama-test",
            KEY,
        ] {
            assert!(!printed.contains(value), "{value}: {printed}");
        }
    }

    // An audit trail that cannot store its record: the command changes nothing.
    r.trail.fail.store(true, Ordering::SeqCst);
    let mut refused = set_args("acme");
    refused[8] = "meta/never-stored";
    let (exit, _, err) = r.run(&refused).await;
    assert_eq!(exit, Exit::Refused);
    assert!(err.contains("nothing was changed"), "{err}");
    assert!(
        ok(&r, &["show", "--tenant", "acme"])
            .await
            .contains("meta/other-model")
    );
    let (exit, _, _) = r
        .run_with_key(Some(KEY), &["key", "set", "--tenant", "acme", "--stdin"])
        .await;
    assert_eq!(exit, Exit::Refused);
    assert!(!r.gw.has_key("acme").await.unwrap());
}
