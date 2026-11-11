//! The provider conformance suite: the checks every provider must pass.
//!
//! [`Fixture`] is the oracle: canned requests, keyed by their `request_id`, plus the
//! responses a correct provider must produce for them. [`run`] executes the scenarios —
//! plain completion, streaming (which must equal the completion), the tool-use round trip,
//! error mapping with the no-leak rule, and the serde stability of the wire shapes — and
//! reports every divergence as a [`Failure`].
//!
//! [`MockProvider::from_fixture`](crate::MockProvider::from_fixture) scripts itself from the
//! same fixture, so `run` is green on the mock out of the box; providers with real exchanges
//! (T2, T3) supply oracles that their recorded transports satisfy.

use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::LlmError;
use crate::message::{Message, Part, Role};
use crate::provider::Provider;
use crate::request::{Request, ToolSpec};
use crate::response::{Response, StopReason, Usage};
use crate::stream::{assemble, drain};

/// One failed check: the scenario, and what differed or broke.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    /// The scenario that failed (`text`, `stream`, `tool_use`, `error`, ...).
    pub scenario: &'static str,
    /// What differed or broke, as one line.
    pub detail: String,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.scenario, self.detail)
    }
}

fn fail(scenario: &'static str, detail: impl Into<String>) -> Failure {
    Failure {
        scenario,
        detail: detail.into(),
    }
}

/// The plain-completion oracle.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextOracle {
    /// The request id; both `complete` and `stream` are called with it.
    pub request_id: String,
    /// The user message.
    pub user_text: String,
    /// The reply a correct provider returns.
    pub reply: String,
    /// The usage a correct provider reports.
    pub usage: Usage,
}

/// The tool-use round-trip oracle: leg one answers with the call, leg two answers with text.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolOracle {
    /// The request id of leg one, which carries the tool.
    pub request_id: String,
    /// The user message asking for the tool.
    pub user_text: String,
    /// The tool declared on both legs.
    pub tool: ToolSpec,
    /// The call id the provider must use.
    pub call_id: String,
    /// The tool name the provider must quote.
    pub call_name: String,
    /// The arguments the provider must produce.
    pub call_arguments: serde_json::Value,
    /// The request id of leg two, which reports the tool result.
    pub result_request_id: String,
    /// The tool result content leg two reports.
    pub result_content: String,
    /// The follow-up text a correct provider returns for leg two.
    pub result_reply: String,
    /// The usage a correct provider reports on both legs.
    pub usage: Usage,
}

/// The error-mapping oracle: the scripted failure, its expected shape, and a token that must
/// never appear in the error text (the no-leak rule).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorOracle {
    /// The request id of the failing request.
    pub request_id: String,
    /// The user message, containing `must_not_leak`.
    pub user_text: String,
    /// The expected `code()`.
    pub code: String,
    /// The expected `retryable()`.
    pub retryable: bool,
    /// The expected `retry_after()`, in seconds, on `llm.rate_limited`.
    pub retry_after_secs: Option<u64>,
    /// A token in the user message that must never surface in error text.
    pub must_not_leak: String,
}

/// The conformance oracle (see the [module docs](self)).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Fixture {
    /// The model id every scenario request uses.
    pub model: String,
    /// The system prompt every scenario request carries.
    pub system: String,
    /// The plain-completion scenario.
    pub text: TextOracle,
    /// The tool-use round-trip scenario.
    pub tool: ToolOracle,
    /// The error-mapping scenario.
    pub error: ErrorOracle,
}

impl Fixture {
    /// A coherent sample: an add-tool conversation, a rate-limited failure, and a secret-shaped
    /// token embedded into the failing request's text.
    pub fn sample() -> Self {
        Self {
            model: "sample-model".to_owned(),
            system: "You are Spotledger's conformance probe.".to_owned(),
            text: TextOracle {
                request_id: "cf-text".to_owned(),
                user_text: "Add 2 and 3.".to_owned(),
                reply: "5".to_owned(),
                usage: Usage {
                    input_tokens: 12,
                    output_tokens: 4,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                },
            },
            tool: ToolOracle {
                request_id: "cf-tool".to_owned(),
                user_text: "Add 2 and 3 with the add tool.".to_owned(),
                tool: ToolSpec {
                    name: "add".to_owned(),
                    description: "Adds two integers.".to_owned(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "a": { "type": "integer" },
                            "b": { "type": "integer" }
                        },
                        "required": ["a", "b"]
                    }),
                },
                call_id: "call_1".to_owned(),
                call_name: "add".to_owned(),
                call_arguments: serde_json::json!({ "a": 2, "b": 3 }),
                result_request_id: "cf-tool-result".to_owned(),
                result_content: "5".to_owned(),
                result_reply: "The sum is 5.".to_owned(),
                usage: Usage {
                    input_tokens: 40,
                    output_tokens: 18,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                },
            },
            error: ErrorOracle {
                request_id: "cf-error".to_owned(),
                user_text: "The on-call token is SECRET-CANNOT-APPEAR; add 1 and 1.".to_owned(),
                code: "llm.rate_limited".to_owned(),
                retryable: true,
                retry_after_secs: Some(2),
                must_not_leak: "SECRET-CANNOT-APPEAR".to_owned(),
            },
        }
    }

    /// The plain-completion request.
    pub fn text_request(&self) -> Request {
        self.base_request(&self.text.request_id)
            .with_message(Message::user(self.text.user_text.clone()))
    }

    /// The tool-use first-leg request: the user ask plus the declared tool.
    pub fn tool_request(&self) -> Request {
        self.base_request(&self.tool.request_id)
            .with_message(Message::user(self.tool.user_text.clone()))
            .with_tool(self.tool.tool.clone())
    }

    /// The tool-result second-leg request: user ask, assistant tool use, tool result.
    pub fn tool_result_request(&self) -> Request {
        let assistant = Message {
            role: Role::Assistant,
            parts: vec![Part::ToolUse {
                id: self.tool.call_id.clone(),
                name: self.tool.call_name.clone(),
                arguments: self.tool.call_arguments.clone(),
            }],
        };
        self.base_request(&self.tool.result_request_id)
            .with_message(Message::user(self.tool.user_text.clone()))
            .with_message(assistant)
            .with_message(Message::tool_result(
                self.tool.call_id.clone(),
                self.tool.result_content.clone(),
            ))
            .with_tool(self.tool.tool.clone())
    }

    /// The error request: its user text embeds the `must_not_leak` token.
    pub fn error_request(&self) -> Request {
        self.base_request(&self.error.request_id)
            .with_message(Message::user(self.error.user_text.clone()))
    }

    fn base_request(&self, request_id: &str) -> Request {
        Request::new(self.model.clone(), request_id).with_system(self.system.clone())
    }
}

/// Runs every scenario against `provider`; the result is green when it is empty.
///
/// The scenarios, in order: the plain completion; the same request streamed and assembled
/// (stream equals complete), including serde stability of the events; the tool-use round trip;
/// error mapping on both `complete` and `stream`; the stop-reason wire names; the capabilities
/// floor; and the serde stability of request and error shapes.
pub async fn run(provider: &dyn Provider, fixture: &Fixture) -> Vec<Failure> {
    let mut failures = Vec::new();

    // Plain completion.
    let expected = Response::new(
        Message::assistant(fixture.text.reply.clone()),
        StopReason::Stop,
        fixture.text.usage,
    );
    match provider.complete(fixture.text_request()).await {
        Ok(response) => {
            if response != expected {
                failures.push(fail(
                    "text",
                    format!(
                        "complete returned {} / {:?} / {:?}, expected {:?}",
                        response.text(),
                        response.stop_reason,
                        response.usage,
                        fixture.text.reply,
                    ),
                ));
            }
        }
        Err(error) => failures.push(fail("text", format!("complete failed: {error}"))),
    }

    // Stream equals complete: the same request id, streamed and assembled.
    check_stream(provider, fixture, &expected, &mut failures).await;
    // Tool-use round trip: leg one answers with the call, leg two with the follow-up text.
    check_tool_round_trip(provider, fixture, &mut failures).await;
    // Error mapping: complete and stream fail with the fixture's shape, no leak.
    check_error_mapping(provider, fixture, &mut failures).await;

    // Stop-reason wire names are stable.
    for (reason, name) in [
        (StopReason::Stop, "stop"),
        (StopReason::Length, "length"),
        (StopReason::ToolUse, "tool_use"),
        (StopReason::ContentFilter, "content_filter"),
    ] {
        match serde_json::to_value(reason) {
            Ok(value) if value == serde_json::Value::String(name.to_owned()) => {}
            other => failures.push(fail(
                "serde",
                format!("stop reason {name:?} did not keep its wire name: {other:?}"),
            )),
        }
    }

    // Capabilities floor: a caller planning by context needs a positive window and output.
    let caps = provider.capabilities(&fixture.model);
    if caps.context_window == 0 || caps.max_output == 0 {
        failures.push(fail(
            "capabilities",
            format!(
                "capabilities for '{}' report no usable window: {caps:?}",
                fixture.model
            ),
        ));
    }

    // Wire stability of the shapes this suite just exercised.
    let request = fixture.text_request();
    if !wire_stable(&request) {
        failures.push(fail(
            "serde",
            "the request did not survive a serde round trip",
        ));
    }
    if !wire_stable(&LlmError::from_code(&fixture.error.code)) {
        failures.push(fail(
            "serde",
            "the fixture error did not survive a serde round trip",
        ));
    }

    failures
}

/// Streams the plain-completion request, assembles the events, and demands the same response
/// `complete` returned — including serde stability of every event on the wire.
async fn check_stream(
    provider: &dyn Provider,
    fixture: &Fixture,
    expected: &Response,
    failures: &mut Vec<Failure>,
) {
    const SCENARIO: &str = "stream";
    let mut stream = match provider.stream(fixture.text_request()).await {
        Ok(stream) => stream,
        Err(error) => {
            failures.push(fail(SCENARIO, format!("stream failed to open: {error}")));
            return;
        }
    };
    let mut events = Vec::new();
    for item in drain(&mut stream).await {
        match item {
            Ok(event) => {
                if !wire_stable(&event) {
                    failures.push(fail(
                        SCENARIO,
                        "an event did not survive a serde round trip",
                    ));
                }
                events.push(event);
            }
            Err(error) => {
                failures.push(fail(SCENARIO, format!("stream failed mid-way: {error}")));
                return;
            }
        }
    }
    match assemble(&events) {
        Ok(assembled) => {
            if assembled != *expected {
                failures.push(fail(
                    SCENARIO,
                    format!(
                        "the assembled stream is {:?} / {:?} / {:?}, expected {:?}",
                        assembled.text(),
                        assembled.stop_reason,
                        assembled.usage,
                        expected.text(),
                    ),
                ));
            }
        }
        Err(error) => failures.push(fail(
            SCENARIO,
            format!("the events do not assemble: {error}"),
        )),
    }
}

/// Checks both tool legs: exactly the oracle's call on leg one, the follow-up text on leg two.
async fn check_tool_round_trip(
    provider: &dyn Provider,
    fixture: &Fixture,
    failures: &mut Vec<Failure>,
) {
    const SCENARIO: &str = "tool_use";
    let response = match provider.complete(fixture.tool_request()).await {
        Ok(response) => response,
        Err(error) => {
            failures.push(fail(SCENARIO, format!("leg one failed: {error}")));
            return;
        }
    };
    let uses = response.message.tool_uses();
    if uses.len() != 1 {
        failures.push(fail(
            SCENARIO,
            format!("leg one answered {} tool uses, expected one", uses.len()),
        ));
        return;
    }
    let call = &uses[0];
    if call.id != fixture.tool.call_id
        || call.name != fixture.tool.call_name
        || call.arguments != fixture.tool.call_arguments
    {
        failures.push(fail(
            SCENARIO,
            format!("the tool call did not match the oracle: {call:?}"),
        ));
    }
    if response.stop_reason != StopReason::ToolUse {
        failures.push(fail(
            SCENARIO,
            format!(
                "leg one stopped with {:?}, expected tool_use",
                response.stop_reason
            ),
        ));
    }
    if response.usage != fixture.tool.usage {
        failures.push(fail(
            SCENARIO,
            format!(
                "leg one reported {:?}, expected the oracle usage",
                response.usage
            ),
        ));
    }

    let second = match provider.complete(fixture.tool_result_request()).await {
        Ok(second) => second,
        Err(error) => {
            failures.push(fail(SCENARIO, format!("leg two failed: {error}")));
            return;
        }
    };
    if second.text() != fixture.tool.result_reply {
        failures.push(fail(
            SCENARIO,
            format!(
                "leg two replied {:?}, expected {:?}",
                second.text(),
                fixture.tool.result_reply
            ),
        ));
    }
    if second.stop_reason != StopReason::Stop {
        failures.push(fail(
            SCENARIO,
            format!(
                "leg two stopped with {:?}, expected stop",
                second.stop_reason
            ),
        ));
    }
    if second.usage != fixture.tool.usage {
        failures.push(fail(
            SCENARIO,
            format!(
                "leg two reported {:?}, expected the oracle usage",
                second.usage
            ),
        ));
    }
}

/// Maps the fixture's error on both paths: `complete` must fail outright and `stream` must
/// fail on the wire, both with the oracle's shape, and the text must never leak the token
/// embedded in the request.
async fn check_error_mapping(
    provider: &dyn Provider,
    fixture: &Fixture,
    failures: &mut Vec<Failure>,
) {
    const SCENARIO: &str = "error";

    let check = |failures: &mut Vec<Failure>, path: &str, error: LlmError| {
        if error.code() != fixture.error.code {
            failures.push(fail(
                SCENARIO,
                format!(
                    "{path} mapped to {}, expected {}",
                    error.code(),
                    fixture.error.code
                ),
            ));
        }
        if error.retryable() != fixture.error.retryable {
            failures.push(fail(
                SCENARIO,
                format!(
                    "{path} retryable is {}, expected {}",
                    error.retryable(),
                    fixture.error.retryable
                ),
            ));
        }
        let expected_after = fixture.error.retry_after_secs.map(Duration::from_secs);
        if error.retry_after() != expected_after {
            failures.push(fail(
                SCENARIO,
                format!(
                    "{path} retry_after is {:?}, expected {:?}",
                    error.retry_after(),
                    expected_after
                ),
            ));
        }
        if error.to_string().contains(&fixture.error.must_not_leak) {
            failures.push(fail(
                SCENARIO,
                format!("{path} leaked secret material from the request into its error text"),
            ));
        }
    };

    match provider.complete(fixture.error_request()).await {
        Ok(response) => failures.push(fail(
            SCENARIO,
            format!(
                "complete should have failed but returned {:?}",
                response.text()
            ),
        )),
        Err(error) => check(failures, "complete", error),
    }

    match provider.stream(fixture.error_request()).await {
        Ok(mut stream) => {
            let items = drain(&mut stream).await;
            match items.into_iter().find(|item| item.is_err()) {
                Some(Err(error)) => check(failures, "stream", error),
                _ => failures.push(fail(
                    SCENARIO,
                    "stream should have failed but produced events",
                )),
            }
        }
        Err(error) => check(failures, "stream", error),
    }
}

/// Whether a value survives a serde round trip unchanged — the wire-stability check.
fn wire_stable<T>(value: &T) -> bool
where
    T: Serialize + for<'de> Deserialize<'de> + PartialEq + fmt::Debug,
{
    serde_json::to_value(value)
        .ok()
        .and_then(|wire| serde_json::from_value::<T>(wire).ok())
        .is_some_and(|back| back == *value)
}
