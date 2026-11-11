//! The scripted provider every test, mock and the conformance suite build on.

use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Serialize};

use crate::conformance::{ErrorOracle, Fixture};
use crate::error::LlmError;
use crate::message::{Message, Part};
use crate::provider::{Capabilities, Provider};
use crate::request::Request;
use crate::response::Response;
use crate::stream::{BoxStream, IterStream, StreamEvent, assemble};

/// One scripted step: what it fires on, and what it then does.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MockStep {
    /// Fire only for a request whose `request_id` is this; `None` fires for any request.
    pub on_request_id: Option<String>,
    /// What to do when the step fires.
    pub action: MockAction,
}

impl MockStep {
    /// A step firing on `on_request_id` (`None` fires for anything) with `action`.
    pub fn new(on_request_id: Option<&str>, action: MockAction) -> Self {
        Self {
            on_request_id: on_request_id.map(str::to_owned),
            action,
        }
    }
}

/// What a scripted step does when it fires. Steps are reusable: the scan restarts for every
/// request, so a request id scripted once answers every call that carries it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum MockAction {
    /// Answer a `complete` with this response; a `stream` of the same request synthesises its
    /// events from the response, so the two agree by construction.
    Respond(Response),
    /// Answer a `stream` with these events; a `complete` of the same request assembles them.
    Stream(Vec<StreamEvent>),
    /// Fail both calls with this error.
    Fail(LlmError),
    /// Sleep before continuing the scan — a load-test delay (WP-AI-05). Uses tokio's clock, so
    /// a paused clock in tests makes the pause instant.
    Pause {
        /// The milliseconds to sleep.
        millis: u64,
    },
}

/// The scripted provider: steps in order, a record of every request it received, and
/// capabilities chosen at build time. This is what the agent and router tests (T4) use.
pub struct MockProvider {
    id: String,
    capabilities: Capabilities,
    steps: Mutex<Vec<MockStep>>,
    recorded: Mutex<Vec<Request>>,
}

impl Default for MockProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl MockProvider {
    /// A provider with id `mock` and [`Capabilities::mock`] defaults.
    pub fn new() -> Self {
        Self::titled("mock")
    }

    /// A provider with a caller-chosen id (to test provider id plumbing) and
    /// [`Capabilities::mock`] defaults.
    pub fn titled(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            capabilities: Capabilities::mock(),
            steps: Mutex::new(Vec::new()),
            recorded: Mutex::new(Vec::new()),
        }
    }

    /// Overrides the capabilities `capabilities` reports.
    pub fn with_capabilities(mut self, capabilities: Capabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// Scripts a provider from a conformance [`Fixture`]: the text, tool and follow-up
    /// scenarios respond with the fixture's oracle values, and the error scenario fails with
    /// the fixture's expected error. The conformance suite runs green on the result.
    pub fn from_fixture(fixture: &Fixture) -> Self {
        let mut provider = Self::new();
        provider.push_step(MockStep::new(
            Some(&fixture.text.request_id),
            MockAction::Respond(Response::new(
                Message::assistant(fixture.text.reply.clone()),
                crate::response::StopReason::Stop,
                fixture.text.usage,
            )),
        ));
        provider.push_step(MockStep::new(
            Some(&fixture.tool.request_id),
            MockAction::Respond(Response::new(
                Message {
                    role: crate::message::Role::Assistant,
                    parts: vec![Part::ToolUse {
                        id: fixture.tool.call_id.clone(),
                        name: fixture.tool.call_name.clone(),
                        arguments: fixture.tool.call_arguments.clone(),
                    }],
                },
                crate::response::StopReason::ToolUse,
                fixture.tool.usage,
            )),
        ));
        provider.push_step(MockStep::new(
            Some(&fixture.tool.result_request_id),
            MockAction::Respond(Response::new(
                Message::assistant(fixture.tool.result_reply.clone()),
                crate::response::StopReason::Stop,
                fixture.tool.usage,
            )),
        ));
        provider.push_step(MockStep::new(
            Some(&fixture.error.request_id),
            MockAction::Fail(oracle_error(&fixture.error)),
        ));
        provider
    }

    /// Adds a step; later builders keep chaining.
    pub fn push_step(&mut self, step: MockStep) {
        self.steps().push(step);
    }

    /// Builds, in order: responds to `request_id`.
    pub fn responds_to(mut self, request_id: &str, response: Response) -> Self {
        self.push_step(MockStep::new(
            Some(request_id),
            MockAction::Respond(response),
        ));
        self
    }

    /// Builds, in order: streams `events` to `request_id`.
    pub fn streams_to(mut self, request_id: &str, events: Vec<StreamEvent>) -> Self {
        self.push_step(MockStep::new(Some(request_id), MockAction::Stream(events)));
        self
    }

    /// Builds, in order: fails `request_id` with `error`.
    pub fn fails_for(mut self, request_id: &str, error: LlmError) -> Self {
        self.push_step(MockStep::new(Some(request_id), MockAction::Fail(error)));
        self
    }

    /// Builds, in order: pauses `millis` before answering `request_id`.
    pub fn pauses_for(mut self, request_id: &str, millis: u64) -> Self {
        self.push_step(MockStep::new(
            Some(request_id),
            MockAction::Pause { millis },
        ));
        self
    }

    /// Every request this provider received, in order. Test assertions read this, as the
    /// agent and router tests will.
    pub fn requests(&self) -> Vec<Request> {
        self.recorded().clone()
    }

    /// How many requests this provider received.
    pub fn recorded_count(&self) -> usize {
        self.recorded().len()
    }

    fn steps(&self) -> std::sync::MutexGuard<'_, Vec<MockStep>> {
        self.steps.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn recorded(&self) -> std::sync::MutexGuard<'_, Vec<Request>> {
        self.recorded.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records `request`, then picks the action for it: every matching pause sleeps, and the
    /// first responding, streaming or failing step answers. Nothing scripted: `llm.bad_request`.
    async fn answer(&self, request: &Request) -> Result<MockAction, LlmError> {
        self.recorded().push(request.clone());
        let id = request.request_id.as_str();
        let steps = self.steps().clone();
        let mut pauses: Vec<u64> = Vec::new();
        let mut found: Option<MockAction> = None;
        for step in &steps {
            let fires = step.on_request_id.as_deref().is_none_or(|on| on == id);
            if !fires {
                continue;
            }
            match &step.action {
                MockAction::Pause { millis } => pauses.push(*millis),
                hit => {
                    found = Some(hit.clone());
                    break;
                }
            }
        }
        for millis in pauses {
            tokio::time::sleep(tokio::time::Duration::from_millis(millis)).await;
        }
        found.ok_or_else(|| LlmError::BadRequest {
            detail: format!("no scripted step for request '{id}'"),
        })
    }
}

impl std::fmt::Debug for MockProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MockProvider")
            .field("id", &self.id)
            .field(
                "steps",
                &self
                    .steps
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .len(),
            )
            .field(
                "recorded",
                &self
                    .recorded
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .len(),
            )
            .finish()
    }
}

impl Provider for MockProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn capabilities(&self, _model: &str) -> Capabilities {
        self.capabilities
    }

    fn complete<'a>(
        &'a self,
        request: Request,
    ) -> crate::provider::BoxFuture<'a, Result<Response, LlmError>> {
        Box::pin(async move {
            match self.answer(&request).await? {
                MockAction::Respond(response) => Ok(response),
                MockAction::Stream(events) => assemble(&events),
                MockAction::Fail(error) => Err(error),
                MockAction::Pause { .. } => unreachable_mock("pause cannot answer"),
            }
        })
    }

    fn stream<'a>(
        &'a self,
        request: Request,
    ) -> crate::provider::BoxFuture<'a, Result<BoxStream, LlmError>> {
        Box::pin(async move {
            let events = match self.answer(&request).await? {
                MockAction::Respond(response) => synthesise(&response),
                MockAction::Stream(events) => events,
                MockAction::Fail(error) => return Err(error),
                MockAction::Pause { .. } => unreachable_mock("pause cannot answer"),
            };
            Ok(Box::pin(IterStream::new(IterStream::ok(events))) as BoxStream)
        })
    }
}

/// The match arms above are total; this documents why none exist for `Pause` inside answers.
fn unreachable_mock(why: &str) -> ! {
    panic!("mock provider internal error: {why}")
}

/// Builds the fixture oracle's error: `retry_after` only makes sense for rate limits.
fn oracle_error(oracle: &ErrorOracle) -> LlmError {
    match oracle.code.as_str() {
        "llm.rate_limited" => LlmError::RateLimited {
            retry_after: oracle.retry_after_secs,
        },
        "llm.bad_request" => LlmError::BadRequest {
            detail: String::new(),
        },
        "llm.protocol" => LlmError::Protocol {
            detail: String::new(),
        },
        other => LlmError::from_code(other).unwrap_or(LlmError::BadRequest {
            detail: format!("fixture error code '{other}' is not in the llm.* contract"),
        }),
    }
}

/// Chunks `text` into at most `per_chunk` characters, so streamed deltas are reassembled
/// across more than one event (and any UTF-8 boundary stays intact).
fn chunks_of(text: &str, per_chunk: usize) -> Vec<StreamEvent> {
    let chars: Vec<char> = text.chars().collect();
    chars
        .chunks(per_chunk)
        .map(|chunk| StreamEvent::TextDelta {
            delta: chunk.iter().collect(),
        })
        .collect()
}

/// Builds the events a `Respond` step streams: text deltas, every tool use bracketed, usage,
/// stop — so a response and its stream agree by construction.
fn synthesise(response: &Response) -> Vec<StreamEvent> {
    const PER_CHUNK: usize = 12;
    let mut events = chunks_of(&response.text(), PER_CHUNK);
    for (index, use_part) in response.message.tool_uses().into_iter().enumerate() {
        let index = index as u32;
        events.push(StreamEvent::ToolUseStart {
            index,
            id: use_part.id.clone(),
            name: use_part.name.clone(),
        });
        events.push(StreamEvent::ToolUseDelta {
            index,
            delta: use_part.arguments.to_string(),
        });
        events.push(StreamEvent::ToolUseEnd { index });
    }
    events.push(StreamEvent::Usage {
        usage: response.usage,
    });
    events.push(StreamEvent::Stop {
        reason: response.stop_reason,
    });
    events
}
