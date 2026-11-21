//! The secret sweep: run every integration handler with a canary credential and prove the
//! canary appears nowhere observable.
//!
//! The canary is a long, unlikely string. Each handler is given a credential that resolves to it
//! (or, for the code node, which carries no credential, it is pre-registered and placed in the
//! node's input), and each fake endpoint echoes it back. The test then asserts the canary is
//! absent from:
//!
//! - the node's output (a credential echoed by an endpoint must not flow on),
//! - the execution history (input, output and error),
//! - the `Debug` formatting of the outcome, the history and the config,
//! - and everything the handlers logged, captured through a `tracing` subscriber.
//!
//! It also proves the credential was *used*: the HTTP request carried it in its authorization
//! header and the webhook was signed with it. The sweep is meaningful only if the secret really
//! travelled.

#![allow(clippy::unwrap_used)]

use std::sync::{Arc, Mutex};

use serde_json::{Value as Json, json};
use spot_app_api::prelude::OffsetDateTime;
use spot_bpm_engine::{CaseId, IdempotencyKey, NodeOutcome};
use spot_bpm_integration::{
    CodeNodeHandler, ConnectorNodeHandler, ExecutionHistory, HttpNodeHandler,
    InMemorySecretResolver, IntegrationCtx, IntegrationHandler, MailNodeHandler, SecretRegistry,
    SecretResolver, WebhookNodeHandler,
};
use spot_dag::NodeId;
use spot_secrets::SecretString;
use spot_ts::{EngineConfig, ScriptEngine};
use tracing::Level;
use tracing_subscriber::fmt::MakeWriter;

/// The canary. Long enough for `spot-redact` to register it for matching (at least eight bytes).
const CANARY: &str = "canary-credential-0123456789-abcdef";

/// A writer that keeps everything a subscriber formats.
#[derive(Clone, Default)]
struct Buffer(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Buffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .map_err(|_| std::io::Error::other("poisoned"))?
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Buffer {
    type Writer = Buffer;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

impl Buffer {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
}

/// Runs one handler and returns its outcome, its history and the Case's variables afterwards.
async fn run_handler<H: IntegrationHandler>(
    handler: &H,
    config: Json,
    input: Json,
    secrets: &dyn SecretResolver,
    pre_register: Option<(&str, &str)>,
) -> (NodeOutcome, ExecutionHistory, Json) {
    let case = CaseId::new("case1").unwrap();
    let node = NodeId::new("n1").unwrap();
    let outputs = json!({});
    let mut variables = json!({ "tenant": "acme" });
    let mut registry = SecretRegistry::new().unwrap();
    if let Some((reference, value)) = pre_register {
        registry.register(
            &spot_bpm_integration::CredentialRef::new(reference),
            &SecretString::new(value),
        );
    }
    let mut ctx = IntegrationCtx {
        case: &case,
        node: &node,
        attempt: 1,
        key: IdempotencyKey::new(case.clone(), node.clone(), 1),
        is_retry: false,
        now: now(),
        principal: None,
        input: &input,
        node_outputs: &outputs,
        variables: &mut variables,
        config: &config,
        secrets,
        registry: &mut registry,
    };
    let outcome = handler.run(&mut ctx).await;
    let mut history = ExecutionHistory::new();
    history.record(&ctx, handler.kind(), &outcome);
    (outcome, history, ctx.variables.clone())
}

/// Asserts the canary is absent from every observable surface of one run.
fn assert_no_canary(label: &str, outcome: &NodeOutcome, history: &ExecutionHistory, config: &Json) {
    let surfaces = [
        ("outcome Debug", format!("{outcome:?}")),
        ("history Debug", format!("{history:?}")),
        ("history JSON", history.as_json().to_string()),
        ("config Debug", format!("{config:?}")),
    ];
    for (what, text) in surfaces {
        assert!(
            !text.contains(CANARY),
            "{label}: the canary appeared in the {what}: {text}"
        );
    }
}

#[tokio::test]
async fn no_handler_leaks_the_canary() {
    let buffer = Buffer::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(buffer.clone())
        .with_max_level(Level::DEBUG)
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    // ---- HTTP -----------------------------------------------------------------------------
    let http_egress = Arc::new(spot_bpm_integration::FakeEgress::new());
    http_egress.push_response(spot_bpm_integration::HttpResponse {
        status: 200,
        headers: http::HeaderMap::new(),
        body: format!(r#"{{"echo":"{CANARY}"}}"#).into_bytes(),
    });
    let http = HttpNodeHandler::new(http_egress.clone(), "acme");
    let mut http_secrets = InMemorySecretResolver::new();
    http_secrets.insert("sales_api", CANARY);
    let http_config = json!({
        "url": "https://api.test/x",
        "auth": { "credential_ref": "sales_api", "scheme": "bearer" },
    });
    let (outcome, history, _) =
        run_handler(&http, http_config.clone(), json!({}), &http_secrets, None).await;
    assert_no_canary("http", &outcome, &history, &http_config);
    // The credential really travelled: it is on the wire.
    let calls = http_egress.calls();
    assert!(
        calls[0]
            .headers
            .get(http::header::AUTHORIZATION)
            .unwrap()
            .to_str()
            .unwrap()
            .contains(CANARY),
        "the credential was not sent"
    );

    // ---- HTTP, basic auth: the encoded header value must not reach the logs ----------------
    http_egress.push_response(spot_bpm_integration::HttpResponse {
        status: 200,
        headers: http::HeaderMap::new(),
        body: b"{}".to_vec(),
    });
    let basic_config = json!({
        "url": "https://api.test/y",
        "auth": { "credential_ref": "sales_api", "scheme": "basic", "username": "svc" },
    });
    let (outcome, history, _) =
        run_handler(&http, basic_config.clone(), json!({}), &http_secrets, None).await;
    assert_no_canary("http basic", &outcome, &history, &basic_config);
    let basic_header = http_egress.calls()[1]
        .headers
        .get(http::header::AUTHORIZATION)
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let encoded = basic_header.trim_start_matches("Basic ").to_owned();
    assert!(!encoded.is_empty() && encoded != basic_header);
    assert!(
        !buffer.text().contains(&encoded),
        "the encoded basic credential appeared in the tracing output"
    );

    // ---- Webhook --------------------------------------------------------------------------
    let webhook_port = Arc::new(spot_bpm_integration::FakeWebhook::new());
    webhook_port.push_response(spot_bpm_integration::HttpResponse {
        status: 200,
        headers: http::HeaderMap::new(),
        body: format!(r#"{{"echo":"{CANARY}"}}"#).into_bytes(),
    });
    let webhook = WebhookNodeHandler::new(webhook_port.clone());
    let mut webhook_secrets = InMemorySecretResolver::new();
    webhook_secrets.insert("hook_key", CANARY);
    let webhook_config = json!({ "url": "https://peer.test/hook", "credential_ref": "hook_key" });
    let (outcome, history, _) = run_handler(
        &webhook,
        webhook_config.clone(),
        json!({}),
        &webhook_secrets,
        None,
    )
    .await;
    assert_no_canary("webhook", &outcome, &history, &webhook_config);
    assert_eq!(
        webhook_port.posts().len(),
        1,
        "the webhook was signed and sent"
    );

    // ---- Connector ------------------------------------------------------------------------
    let connector_port = Arc::new(spot_bpm_integration::FakeConnector::new());
    connector_port.push_output(json!({ "echo": CANARY }));
    let connector = ConnectorNodeHandler::new(connector_port.clone());
    let mut connector_secrets = InMemorySecretResolver::new();
    connector_secrets.insert("as2_key", CANARY);
    let connector_config = json!({
        "connector": "as2",
        "operation": "send",
        "credential_ref": "as2_key",
    });
    let (outcome, history, _) = run_handler(
        &connector,
        connector_config.clone(),
        json!({}),
        &connector_secrets,
        None,
    )
    .await;
    assert_no_canary("connector", &outcome, &history, &connector_config);
    // The port saw the reference only.
    assert_eq!(
        connector_port.calls()[0].credential_ref,
        Some(spot_bpm_integration::CredentialRef::new("as2_key"))
    );

    // ---- Mail -----------------------------------------------------------------------------
    let mail_port = Arc::new(spot_bpm_integration::FakeMail::new());
    mail_port.push_receipt(spot_mail::SendReceipt {
        message_id: String::from("m1"),
        accepted: Vec::new(),
        rejected: Vec::new(),
    });
    let mail = MailNodeHandler::new(mail_port.clone());
    let mut mail_secrets = InMemorySecretResolver::new();
    mail_secrets.insert("smtp", CANARY);
    let mail_config = json!({
        "from": "a@b.test",
        "to": ["c@d.test"],
        "subject": "s",
        "text": "t",
        "credential_ref": "smtp",
    });
    let (outcome, history, _) =
        run_handler(&mail, mail_config.clone(), json!({}), &mail_secrets, None).await;
    assert_no_canary("mail", &outcome, &history, &mail_config);

    // ---- Code -----------------------------------------------------------------------------
    // The code node carries no credential; the canary is pre-registered and placed in its input,
    // and the script echoes it. The handler must still not leak it.
    let code = CodeNodeHandler::new(Arc::new(ScriptEngine::new(EngineConfig::default())));
    let code_config = json!({
        "script": "export function main(args) { return { echo: args.token }; }",
        "input": { "token": "doc.input.token" },
    });
    let empty_secrets = InMemorySecretResolver::new();
    let (outcome, history, _) = run_handler(
        &code,
        code_config.clone(),
        json!({ "token": CANARY }),
        &empty_secrets,
        Some(("input_token", CANARY)),
    )
    .await;
    assert_no_canary("code", &outcome, &history, &code_config);

    // ---- The logs -------------------------------------------------------------------------
    let logs = buffer.text();
    assert!(
        !logs.contains(CANARY),
        "the canary appeared in the tracing output:\n{logs}"
    );
    assert!(
        logs.contains("bpm_integration"),
        "the sweep is only meaningful if the handlers logged something:\n{logs}"
    );
}
