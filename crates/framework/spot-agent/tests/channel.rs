//! WP-ADM-02.T3 tests: the signed-command intake pipeline against scripted gateways, the
//! durable replay window, crash semantics, revocation, backoff bounds and the pinned TLS
//! transport against a loopback gateway.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ed25519_dalek::{Signer as _, SigningKey};
use serde_json::{Value, json};
use spot_agent::channel::backoff::{Backoff, MAX_SECS};
use spot_agent::channel::client::{ChannelError, Client, ResultReport, signing_input};
use spot_agent::channel::intake::{Pipeline, PipelineParts, adapt_envelope};
use spot_agent::channel::serve::{LoopCtx, LoopEnd, run_loop};
use spot_agent::channel::store::{CmdState, CommandRecord, FileCommandStore, FileNonceStore};
use spot_agent::channel::timefmt::{parse_http_date, rfc3339};
use spot_agent::channel::transport::{
    HttpRequest, HttpResponse, TlsTransport, Transport, parse_response, split_url,
};
use spot_agent::error::AgentError;
use spot_agent::logging::{Logger, build_redactor};
use spot_agent::run::Shutdown;
use spot_agent::state::StateDir;
use spot_agent_core::{
    ExecutionPlan, HandlerOutcome, HandlerRegistry, MemoryResultLog, NoCritical, OperationHandler,
    Policy, TrustStore,
};
use spot_agent_enroll::keys::{KeyStore, MemoryKeyStore, Protection};
use spot_mgmt_ops::{
    CatalogueBuilder, OpId, OperationSpec, ParamSpec, ParamType, Risk, TargetKind,
};
use spot_signed_command::{Command, KeyId};

const NOW: i64 = 1_800_000_000;
const AGENT: &str = "agent-1";

fn admin() -> SigningKey {
    SigningKey::from_bytes(&[11u8; 32])
}

fn key_id(k: &SigningKey) -> KeyId {
    KeyId::from_public_key(&k.verifying_key().to_bytes())
}

fn op(id: &str, risk: Risk, params: Vec<ParamSpec>) -> OperationSpec {
    OperationSpec {
        id: OpId::new(id).unwrap(),
        title: id.to_owned(),
        summary: format!("Test {id}."),
        params,
        risk,
        target: TargetKind::Server,
    }
}

fn text_param(name: &str) -> ParamSpec {
    ParamSpec {
        name: name.to_owned(),
        ty: ParamType::Text {
            max_len: 64,
            pattern: None,
        },
        required: true,
        default: None,
        doc: "A parameter.".to_owned(),
        secret: false,
    }
}

struct Counting {
    runs: Arc<AtomicUsize>,
    store: FileCommandStore,
    seen_started: Arc<Mutex<Vec<bool>>>,
    outcome: HandlerOutcome,
}

impl OperationHandler for Counting {
    fn handle(&self, plan: &ExecutionPlan) -> HandlerOutcome {
        self.runs.fetch_add(1, Ordering::SeqCst);
        // Persist-before-execute: the record on disk says `started` while we run.
        let started = self
            .store
            .get(plan.command_id())
            .unwrap()
            .is_some_and(|r| r.state == CmdState::Started);
        self.seen_started.lock().unwrap().push(started);
        self.outcome.clone()
    }
}

struct Fixture {
    dir: tempfile::TempDir,
    runs: Arc<AtomicUsize>,
    seen_started: Arc<Mutex<Vec<bool>>>,
    shutdown: Shutdown,
}

impl Fixture {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
            runs: Arc::new(AtomicUsize::new(0)),
            seen_started: Arc::new(Mutex::new(Vec::new())),
            shutdown: Shutdown::new(),
        }
    }

    fn pipeline(&self, outcome: HandlerOutcome) -> Pipeline {
        let mut builder = CatalogueBuilder::new();
        builder.add("t", op("server.info.read", Risk::Read, vec![]));
        builder.add(
            "t",
            op(
                "server.service.restart",
                Risk::Medium,
                vec![text_param("name")],
            ),
        );
        builder.add(
            "t",
            op("tenant.backup.run", Risk::High, vec![text_param("target")]),
        );
        let mut trust = TrustStore::new();
        trust
            .insert(&admin().verifying_key().to_bytes(), Protection::Hardware)
            .unwrap();
        let store = FileCommandStore::open(self.dir.path().join("commands")).unwrap();
        let mut handlers = HandlerRegistry::new();
        for id in ["server.service.restart", "tenant.backup.run"] {
            handlers.register(
                OpId::new(id).unwrap(),
                Arc::new(Counting {
                    runs: self.runs.clone(),
                    store: store.clone(),
                    seen_started: self.seen_started.clone(),
                    outcome: outcome.clone(),
                }),
            );
        }
        Pipeline::new(PipelineParts {
            agent_id: AGENT.to_owned(),
            catalogue: builder.build().unwrap(),
            trust,
            policy: Policy::default(),
            nonce: Arc::new(FileNonceStore::open(self.dir.path().join("nonces.log"), NOW).unwrap()),
            critical: Box::new(|| Box::new(NoCritical)),
            log: Arc::new(MemoryResultLog::new(AGENT)),
            handlers: Arc::new(handlers),
            store,
            redactor: Arc::new(build_redactor(&[]).unwrap()),
            logger: Logger::new(
                spot_agent::config::LogLevel::Debug,
                Box::new(std::io::sink()),
            ),
            drain: Duration::from_millis(200),
            shutdown: self.shutdown.clone(),
        })
    }
}

/// A contract-shaped envelope (`command` object, `key_id`/`signature` pairs).
fn envelope(
    op_id: &str,
    params: Value,
    audience: &str,
    issued: i64,
    expires: i64,
    signers: &[&SigningKey],
) -> Value {
    let command = Command::new(op_id, params, audience, key_id(&admin()), issued, expires);
    let object: Value = serde_json::from_slice(&command.canonical_bytes().unwrap()).unwrap();
    let signatures: Vec<Value> = signers
        .iter()
        .map(|s| {
            let sig = s.sign(&command.signed_message().unwrap());
            json!({"key_id": key_id(s).as_str(), "signature": STANDARD.encode(sig.to_bytes())})
        })
        .collect();
    json!({"command": object, "signatures": signatures})
}

fn restart(name: &str) -> Value {
    envelope(
        "server.service.restart",
        json!({"name": name}),
        AGENT,
        NOW - 5,
        NOW + 300,
        &[&admin()],
    )
}

fn block<T>(f: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(f)
}

#[test]
fn a_signed_command_executes_once_and_a_replay_rereports() {
    let fx = Fixture::new();
    let pipeline = fx.pipeline(HandlerOutcome::Succeeded);
    let command = restart("spot-runtime");
    let first = block(pipeline.process(&command, NOW)).unwrap().unwrap();
    assert_eq!((first.outcome.as_str(), first.seq), ("succeeded", 1));
    assert_eq!(fx.runs.load(Ordering::SeqCst), 1);
    assert_eq!(
        *fx.seen_started.lock().unwrap(),
        vec![true],
        "the record was `started` on disk before the handler ran"
    );
    // The same command id again: not executed, the stored result is re-reported.
    let again = block(pipeline.process(&command, NOW + 1)).unwrap().unwrap();
    assert_eq!(again, first);
    assert_eq!(fx.runs.load(Ordering::SeqCst), 1);
}

#[test]
fn refused_commands_are_reported_with_core_codes_and_never_run() {
    let fx = Fixture::new();
    let pipeline = fx.pipeline(HandlerOutcome::Succeeded);
    let other = SigningKey::from_bytes(&[99u8; 32]);
    let cases: Vec<(Value, &str)> = vec![
        (
            envelope(
                "server.service.restart",
                json!({"name": "x"}),
                AGENT,
                NOW - 5,
                NOW + 300,
                &[&other],
            ),
            "agent_core.unknown_signer",
        ),
        (
            envelope(
                "server.service.restart",
                json!({"name": "x"}),
                "someone-else",
                NOW - 5,
                NOW + 300,
                &[&admin()],
            ),
            "agent_core.wrong_audience",
        ),
        (
            envelope(
                "server.service.restart",
                json!({"name": "x"}),
                AGENT,
                NOW - 700,
                NOW - 400,
                &[&admin()],
            ),
            "agent_core.expired",
        ),
        (
            envelope(
                "tenant.backup.run",
                json!({"target": "x"}),
                AGENT,
                NOW - 5,
                NOW + 300,
                &[&admin()],
            ),
            "agent_core.insufficient_signatures",
        ),
    ];
    for (command, code) in cases {
        let report = block(pipeline.process(&command, NOW)).unwrap().unwrap();
        assert_eq!(report.outcome, "refused", "{code}");
        assert_eq!(report.code.as_deref(), Some(code));
    }
    assert_eq!(fx.runs.load(Ordering::SeqCst), 0);
    // A tampered signature is refused as invalid.
    let mut tampered = restart("x");
    let sig = tampered["signatures"][0]["signature"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut bytes = STANDARD.decode(sig).unwrap();
    bytes[0] ^= 0xff;
    tampered["signatures"][0]["signature"] = json!(STANDARD.encode(bytes));
    let report = block(pipeline.process(&tampered, NOW)).unwrap().unwrap();
    assert_eq!(report.code.as_deref(), Some("agent_core.invalid_signature"));
    // A gateway-supplied key that is not locally trusted never helps: the contract shape
    // resolves key ids only against the local trust store.
    let mut forged = restart("x");
    forged["signatures"][0]["key_id"] = json!(key_id(&other).as_str());
    let trust = TrustStore::new();
    assert!(adapt_envelope(&forged, &trust).is_err());
}

#[test]
fn an_operation_without_a_handler_is_unsupported() {
    let fx = Fixture::new();
    let pipeline = fx.pipeline(HandlerOutcome::Succeeded);
    let info = envelope(
        "server.info.read",
        json!({}),
        AGENT,
        NOW - 5,
        NOW + 300,
        &[&admin()],
    );
    let report = block(pipeline.process(&info, NOW)).unwrap().unwrap();
    assert_eq!(
        (report.outcome.as_str(), report.code.as_deref()),
        ("unsupported", Some("agent.unsupported"))
    );
    let unknown = envelope(
        "server.nothing.here",
        json!({}),
        AGENT,
        NOW - 5,
        NOW + 300,
        &[&admin()],
    );
    let report = block(pipeline.process(&unknown, NOW)).unwrap().unwrap();
    assert_eq!(report.code.as_deref(), Some("agent.unsupported"));
    assert_eq!(fx.runs.load(Ordering::SeqCst), 0);
}

#[test]
fn handler_failure_output_is_redacted_and_bounded() {
    let fx = Fixture::new();
    let detail = format!("boom password=hunter2hunter2 {}", "x".repeat(200_000));
    let pipeline = fx.pipeline(HandlerOutcome::Failed { detail });
    let report = block(pipeline.process(&restart("x"), NOW))
        .unwrap()
        .unwrap();
    assert_eq!(
        (report.outcome.as_str(), report.code.as_deref()),
        ("failed", Some("agent.handler_failed"))
    );
    let output = report.output.unwrap();
    assert!(!output.contains("hunter2hunter2"));
    assert!(output.len() <= 64 * 1024);
}

/// Crash fault points. The record on disk after a crash decides what a restart does:
/// `pending` (accepted, handler not started) is re-verified and run exactly once;
/// `started` is reported `indeterminate` and never re-run.
#[test]
fn crash_between_persist_and_execute() {
    // pending -> executed exactly once, even if recovery runs twice.
    let fx = Fixture::new();
    let command = restart("spot-runtime");
    let wire = adapt_envelope(&command, &trust()).unwrap();
    let id = command["command"]["id"].as_str().unwrap().to_owned();
    let pipeline = fx.pipeline(HandlerOutcome::Succeeded);
    pipeline
        .store()
        .put(&CommandRecord {
            id: id.clone(),
            state: CmdState::Pending,
            envelope: wire,
            seq: 0,
            outcome: None,
            code: None,
            output: None,
            reported: false,
        })
        .unwrap();
    let reports = block(pipeline.recover(NOW)).unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].outcome, "succeeded");
    assert_eq!(fx.runs.load(Ordering::SeqCst), 1);
    assert_eq!(
        block(pipeline.recover(NOW)).unwrap().len(),
        1,
        "unacknowledged, re-reported, not re-run"
    );
    assert_eq!(fx.runs.load(Ordering::SeqCst), 1);
    pipeline.mark_reported(&id).unwrap();
    assert!(block(pipeline.recover(NOW)).unwrap().is_empty());

    // pending but expired while the agent was down: refused, not run.
    let fx = Fixture::new();
    let pipeline = fx.pipeline(HandlerOutcome::Succeeded);
    let late = restart("x");
    let wire = adapt_envelope(&late, &trust()).unwrap();
    let id = late["command"]["id"].as_str().unwrap().to_owned();
    pipeline
        .store()
        .put(&CommandRecord {
            id,
            state: CmdState::Pending,
            envelope: wire,
            seq: 0,
            outcome: None,
            code: None,
            output: None,
            reported: false,
        })
        .unwrap();
    let reports = block(pipeline.recover(NOW + 10_000)).unwrap();
    assert_eq!(reports[0].outcome, "refused");
    assert_eq!(fx.runs.load(Ordering::SeqCst), 0);

    // started -> indeterminate, never re-run.
    let fx = Fixture::new();
    let pipeline = fx.pipeline(HandlerOutcome::Succeeded);
    let c = restart("x");
    let wire = adapt_envelope(&c, &trust()).unwrap();
    let id = c["command"]["id"].as_str().unwrap().to_owned();
    pipeline
        .store()
        .put(&CommandRecord {
            id,
            state: CmdState::Started,
            envelope: wire,
            seq: 0,
            outcome: None,
            code: None,
            output: None,
            reported: false,
        })
        .unwrap();
    let reports = block(pipeline.recover(NOW)).unwrap();
    assert_eq!(
        (reports[0].outcome.as_str(), reports[0].code.as_deref()),
        ("failed", Some("agent.indeterminate"))
    );
    assert_eq!(fx.runs.load(Ordering::SeqCst), 0);
}

fn trust() -> TrustStore {
    let mut t = TrustStore::new();
    t.insert(&admin().verifying_key().to_bytes(), Protection::Hardware)
        .unwrap();
    t
}

#[test]
fn the_replay_window_survives_a_restart() {
    let fx = Fixture::new();
    let command = restart("spot-runtime");
    {
        let pipeline = fx.pipeline(HandlerOutcome::Succeeded);
        let report = block(pipeline.process(&command, NOW)).unwrap().unwrap();
        assert_eq!(report.outcome, "succeeded");
    }
    // Rebuild the agent core on the same directory; additionally lose the command record
    // so only the durable nonce window stands between the replay and a second execution.
    std::fs::remove_dir_all(fx.dir.path().join("commands")).unwrap();
    let pipeline = fx.pipeline(HandlerOutcome::Succeeded);
    let report = block(pipeline.process(&command, NOW + 5)).unwrap().unwrap();
    assert_eq!(
        (report.outcome.as_str(), report.code.as_deref()),
        ("refused", Some("agent_core.replay"))
    );
    assert_eq!(fx.runs.load(Ordering::SeqCst), 1);
}

#[test]
fn nonce_window_file_semantics() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nonces.log");
    let k = key_id(&admin());
    let run = |store: &FileNonceStore, nonce: [u8; 16], exp: i64, now: i64| {
        block(spot_signed_command::NonceStore::check_and_insert(
            store, &k, nonce, exp, now,
        ))
        .unwrap()
    };
    let store = FileNonceStore::open(&path, NOW).unwrap();
    assert!(run(&store, [1; 16], NOW + 100, NOW));
    assert!(!run(&store, [1; 16], NOW + 100, NOW));
    drop(store);
    let reopened = FileNonceStore::open(&path, NOW).unwrap();
    assert!(
        !run(&reopened, [1; 16], NOW + 100, NOW),
        "durable across a restart"
    );
    assert!(run(&reopened, [2; 16], NOW + 100, NOW));
    drop(reopened);
    // A torn last line (crash during an unacknowledged append) is ignored; a corrupt
    // complete line fails closed; expired entries are swept and the file rewritten.
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str("deadbeef 00");
    std::fs::write(&path, &text).unwrap();
    assert!(FileNonceStore::open(&path, NOW).is_ok());
    std::fs::write(&path, "garbage line\nanother\n").unwrap();
    assert!(FileNonceStore::open(&path, NOW).is_err());
    std::fs::write(
        &path,
        format!("{} {} {}\n", k.as_str(), "01".repeat(16), NOW - 1000),
    )
    .unwrap();
    let swept = FileNonceStore::open(&path, NOW).unwrap();
    assert!(swept.is_empty());
}

/// What a scripted gateway saw: method, path and headers of each request.
type Seen = Vec<(String, String, Vec<(String, String)>)>;

/// A scripted gateway transport that also verifies the agent's request signature.
struct Script {
    pubkey: ed25519_dalek::VerifyingKey,
    handler: Box<dyn Fn(&HttpRequest) -> HttpResponse + Send + Sync>,
    seen: Mutex<Seen>,
}

impl Transport for Script {
    fn send(&self, request: &HttpRequest) -> Result<HttpResponse, AgentError> {
        let get = |n: &str| {
            request
                .headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(n))
                .map(|(_, v)| v.clone())
        };
        if request.path != "/agent/v1/enrol" {
            let ts: i64 = get("X-Spot-Timestamp").unwrap().parse().unwrap();
            let nonce = get("X-Spot-Nonce").unwrap();
            let sig = STANDARD
                .decode(get("X-Spot-Agent-Signature").unwrap())
                .unwrap();
            let sig = ed25519_dalek::Signature::from_slice(&sig).unwrap();
            let input = signing_input(request.method, &request.path, &request.body, ts, &nonce);
            use ed25519_dalek::Verifier as _;
            self.pubkey.verify(&input, &sig).expect(
                "every request is signed over method, path, body digest, timestamp and nonce",
            );
        }
        self.seen.lock().unwrap().push((
            request.path.clone(),
            String::from_utf8_lossy(&request.body).into_owned(),
            request.headers.clone(),
        ));
        Ok((self.handler)(request))
    }
}

fn ok_json(v: Value) -> HttpResponse {
    HttpResponse {
        status: 200,
        headers: vec![(
            "date".to_owned(),
            "Tue, 14 Nov 2023 22:13:20 GMT".to_owned(),
        )],
        body: v.to_string().into_bytes(),
    }
}

fn client_with(
    handler: impl Fn(&HttpRequest) -> HttpResponse + Send + Sync + 'static,
) -> (Arc<Client>, Arc<Script>) {
    let keys: Arc<dyn KeyStore> =
        Arc::new(MemoryKeyStore::from_seed([5u8; 32], Protection::Software));
    let script = Arc::new(Script {
        pubkey: ed25519_dalek::VerifyingKey::from_bytes(keys.public_key().as_bytes()).unwrap(),
        handler: Box::new(handler),
        seen: Mutex::new(Vec::new()),
    });
    let transport: Arc<dyn Transport> = script.clone();
    (Arc::new(Client::new(transport, keys, AGENT)), script)
}

fn loop_ctx(fx: &Fixture, client: Arc<Client>, max_polls: Option<u64>) -> LoopCtx {
    let state = StateDir::new(fx.dir.path().join("state"));
    state.create().unwrap();
    std::fs::write(state.key_path(), "k").unwrap();
    std::fs::write(state.trust_path(), "t").unwrap();
    LoopCtx {
        client,
        pipeline: Arc::new(fx.pipeline(HandlerOutcome::Succeeded)),
        state,
        logger: Logger::new(
            spot_agent::config::LogLevel::Debug,
            Box::new(std::io::sink()),
        ),
        shutdown: fx.shutdown.clone(),
        poll_pause: Duration::from_millis(1),
        now: Arc::new(|| NOW),
        jitter: Arc::new(|| 0),
        max_polls,
    }
}

#[test]
fn the_loop_polls_executes_reports_and_heartbeats_with_signed_requests() {
    let fx = Fixture::new();
    let command = restart("spot-runtime");
    let delivered = Arc::new(AtomicUsize::new(0));
    let d = delivered.clone();
    let (client, script) = client_with(move |req| match req.path.as_str() {
        "/agent/v1/poll" => {
            let first = d.fetch_add(1, Ordering::SeqCst) == 0;
            ok_json(
                json!({"data": {"commands": if first { vec![command.clone()] } else { vec![] }}}),
            )
        }
        _ => ok_json(json!({})),
    });
    let ctx = loop_ctx(&fx, client, Some(2));
    let end = block(run_loop(ctx)).unwrap();
    assert_eq!(end, LoopEnd::PollLimit);
    assert_eq!(fx.runs.load(Ordering::SeqCst), 1);
    let seen = script.seen.lock().unwrap();
    let results: Vec<_> = seen
        .iter()
        .filter(|(p, _, _)| p == "/agent/v1/results")
        .collect();
    assert_eq!(results.len(), 1);
    let body: Value = serde_json::from_str(&results[0].1).unwrap();
    assert_eq!(
        (body["outcome"].as_str(), body["seq"].as_u64()),
        (Some("succeeded"), Some(1))
    );
    assert!(
        results[0]
            .2
            .iter()
            .any(|(k, v)| k == "Idempotency-Key" && v == body["command_id"].as_str().unwrap())
    );
    assert!(seen.iter().any(|(p, _, _)| p == "/agent/v1/heartbeat"));
    // The second poll carries the acknowledged sequence.
    let polls: Vec<Value> = seen
        .iter()
        .filter(|(p, _, _)| p == "/agent/v1/poll")
        .map(|(_, b, _)| serde_json::from_str(b).unwrap())
        .collect();
    assert_eq!(polls[1]["last_result_seq"], 1);
    assert!(polls[0]["liveness"]["at"].as_str().unwrap().ends_with('Z'));
}

#[test]
fn a_revoked_agent_stops_polling_and_deletes_its_keys() {
    let fx = Fixture::new();
    let (client, script) = client_with(|_| {
        HttpResponse {
        status: 403,
        headers: vec![],
        body: json!({"error": {"code": "agent.revoked", "message": "revoked", "retryable": false, "request_id": "r"}}).to_string().into_bytes(),
    }
    });
    let ctx = loop_ctx(&fx, client, None);
    let state = ctx.state.clone();
    assert_eq!(block(run_loop(ctx)).unwrap(), LoopEnd::Revoked);
    assert!(!state.key_path().exists() && !state.trust_path().exists());
    assert_eq!(
        script.seen.lock().unwrap().len(),
        1,
        "it stopped after the first answer"
    );
}

#[test]
fn failures_back_off_and_a_shutdown_ends_the_loop() {
    let fx = Fixture::new();
    let (client, script) = client_with(|_| HttpResponse {
        status: 503,
        headers: vec![],
        body: vec![],
    });
    let ctx = loop_ctx(&fx, client, None);
    let shutdown = fx.shutdown.clone();
    let end = block(async move {
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1500)).await;
            shutdown.trigger();
        });
        run_loop(ctx).await
    })
    .unwrap();
    assert_eq!(end, LoopEnd::Shutdown);
    // 1 s then 2 s steps: at most two polls fit in 1.5 s plus the first immediate one.
    assert!(script.seen.lock().unwrap().len() <= 3);
}

#[test]
fn backoff_is_jittered_exponential_between_one_second_and_five_minutes() {
    for jitter in [0u64, 500, 1000] {
        let mut b = Backoff::new();
        let mut last_step = Duration::ZERO;
        for _ in 0..40 {
            let d = b.next_delay(jitter);
            assert!(
                d >= Duration::from_secs(1) && d <= Duration::from_secs(MAX_SECS),
                "{d:?}"
            );
            if jitter == 1000 {
                assert!(d >= last_step, "full jitter follows the step up");
                last_step = d;
            }
        }
    }
    let mut b = Backoff::new();
    assert_eq!(b.next_delay(1000), Duration::from_secs(1));
    assert_eq!(b.next_delay(1000), Duration::from_secs(2));
    assert_eq!(b.next_delay(1000), Duration::from_secs(4));
    b.reset();
    assert_eq!(b.next_delay(0), Duration::from_secs(1));
    // Jitter is bounded below by half the step.
    let mut b = Backoff::new();
    for _ in 0..4 {
        b.next_delay(0);
    }
    assert_eq!(b.next_delay(0), Duration::from_secs(8));
}

#[test]
fn time_and_framing_helpers() {
    assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
    assert_eq!(rfc3339(1_700_000_000), "2023-11-14T22:13:20Z");
    assert_eq!(
        parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT"),
        Some(784_111_777)
    );
    assert_eq!(parse_http_date("garbage"), None);
    assert_eq!(
        split_url("https://gw.example.test").unwrap(),
        ("gw.example.test".to_owned(), 443)
    );
    assert_eq!(
        split_url("https://gw.example.test:8443/").unwrap(),
        ("gw.example.test".to_owned(), 8443)
    );
    assert_eq!(
        split_url("https://[::1]:9000").unwrap(),
        ("::1".to_owned(), 9000)
    );
    assert!(split_url("http://gw.example.test").is_err());
    assert!(split_url("https://gw/path").is_err());
    let plain = parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi").unwrap();
    assert_eq!(
        (plain.status, plain.body.as_slice()),
        (200, b"hi".as_slice())
    );
    let chunked = parse_response(
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nhi\r\n4\r\n you\r\n0\r\n\r\n",
    )
    .unwrap();
    assert_eq!(chunked.body, b"hi you");
    assert!(parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nhi").is_err());
    let (client, _) = client_with(|_| ok_json(json!({})));
    assert_eq!(client.clock_skew(), 0);
    let _ = (
        ResultReport {
            command_id: "c".into(),
            seq: 1,
            outcome: "succeeded".into(),
            code: None,
            output: None,
        },
        ChannelError::Revoked,
    );
}

/// A loopback TLS 1.3 gateway: answers every request with `{"data":{"commands":[]}}`.
fn tls_gateway(
    cert_der: rustls::pki_types::CertificateDer<'static>,
    key_der: Vec<u8>,
    connections: usize,
) -> u16 {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = Arc::new(
        rustls::ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert_der],
                rustls::pki_types::PrivatePkcs8KeyDer::from(key_der).into(),
            )
            .unwrap(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming().take(connections).flatten() {
            let conn = rustls::ServerConnection::new(config.clone()).unwrap();
            let mut tls = rustls::StreamOwned::new(conn, stream);
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                match tls.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                }
            }
            let body = r#"{"data":{"commands":[]}}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = tls.write_all(response.as_bytes());
            tls.conn.send_close_notify();
            let _ = tls.flush();
        }
    });
    port
}

#[test]
fn tls_is_pinned_to_the_trust_anchor() {
    let real = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
    let other = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
    let port = tls_gateway(real.cert.der().clone(), real.signing_key.serialize_der(), 2);
    let url = format!("https://localhost:{port}");
    let request = HttpRequest {
        method: "POST",
        path: "/agent/v1/poll".to_owned(),
        headers: vec![],
        body: b"{}".to_vec(),
        timeout: Duration::from_secs(10),
    };

    // The gateway's own certificate as the anchor: the exchange works over TLS 1.3.
    let pinned = TlsTransport::new(&url, &real.cert.pem()).unwrap();
    let response = pinned.send(&request).unwrap();
    assert_eq!(response.status, 200);
    assert!(
        String::from_utf8(response.body)
            .unwrap()
            .contains("commands")
    );

    // A gateway presenting a different certificate than the anchor is refused.
    let wrong = TlsTransport::new(&url, &other.cert.pem()).unwrap();
    let err = wrong.send(&request).unwrap_err();
    assert_eq!(err.code(), "agent.transport");

    // No anchor, no connection: an empty or non-certificate CA file is refused.
    assert!(TlsTransport::new(&url, "").is_err());
    assert!(
        TlsTransport::new(
            &url,
            "-----BEGIN CERTIFICATE-----\n!!!\n-----END CERTIFICATE-----"
        )
        .is_err()
    );
}
