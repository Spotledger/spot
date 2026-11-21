//! The typed wire shapes for `/v1/chat/completions` — request build, response read.
//!
//! Outbound building is fallible (a request whose model is unknown), so callers get
//! `Result`s; the provider turns those into `llm.bad_request` before the transport is
//! touched, and the details name the field, never the body contents.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::profile::{MaxTokensField, Profile};
use crate::provider::ModelInfo;
use spot_llm::message::{Message, Part};
use spot_llm::request::{Request, ToolSpec};
use spot_llm::response::Usage;

/// Formats a temperature permille into the API's decimal text, no floats: `0` → `"0"`,
/// `700` → `"0.7"`, `999` → `"0.999"`, `1_000` → `"1"`; over 1_000 is out of range at the
/// caller.
pub(crate) fn temperature_text(permille: u16) -> Option<String> {
    if permille > 1_000 {
        return None;
    }
    let whole = permille / 1_000;
    let mut fraction = format!("{:03}", permille % 1_000);
    while fraction.ends_with('0') {
        fraction.pop();
    }
    if fraction.is_empty() {
        Some(whole.to_string())
    } else {
        Some(format!("{whole}.{fraction}"))
    }
}

/// Failure while building an outbound request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BuildError {
    /// The request's model is not in the provider's catalogue.
    UnknownModel(String),
    /// A request declares tools but the profile says the endpoint does not support them.
    ToolsUnsupported,
    /// The temperature permille is out of range (`0..=1_000`).
    Temperature(u16),
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BuildError::UnknownModel(model) => {
                write!(f, "model '{model}' is not in the catalogue")
            }
            BuildError::ToolsUnsupported => {
                write!(f, "the profile does not support tools")
            }
            BuildError::Temperature(permille) => {
                write!(f, "temperature {permille} is outside 0..=1000")
            }
        }
    }
}

/// Builds the outbound `messages` entry for one spot-llm message.
fn outbound_message(spot: &Message) -> OutboundMessage {
    // The tool answer rides the `tool` role, in the documented Chat Completions shape; any
    // other role maps through the plain content form.
    if let [
        Part::ToolResult {
            tool_use_id,
            content,
            is_error,
        },
    ] = spot.parts.as_slice()
    {
        return OutboundMessage {
            role: "tool".to_owned(),
            content: OutboundContent::Text(format_result(content, *is_error)),
            tool_calls: None,
            tool_call_id: Some(tool_use_id.clone()),
        };
    }
    let mut calls: Vec<OutboundToolCallWire> = Vec::new();
    let text: Vec<String> = spot
        .parts
        .iter()
        .filter_map(|part| match part {
            Part::Text { text } => Some(text.clone()),
            // A ToolResult part that rides a mixed (multi-part) message contributes its
            // content to the plain form's text, so the answer still reaches the endpoint.
            Part::ToolResult { content, .. } => Some(content.clone()),
            _ => None,
        })
        .collect();
    for part in &spot.parts {
        if let Part::ToolUse {
            id,
            name,
            arguments,
        } = part
        {
            calls.push(OutboundToolCallWire {
                id: id.clone(),
                kind: FunctionKind::Function,
                function: FunctionWire {
                    name: name.clone(),
                    arguments: ArgumentsWire::new(arguments.clone()),
                },
            });
        }
    }
    OutboundMessage {
        role: wire_role(spot.role),
        content: if text.is_empty() {
            OutboundContent::Null
        } else {
            OutboundContent::Text(text.join("\n"))
        },
        tool_calls: (!calls.is_empty()).then_some(calls),
        tool_call_id: None,
    }
}

/// The tool result's wire text: the content, marked "Error: " when the tool failed.
fn format_result(content: &str, is_error: bool) -> String {
    if is_error {
        format!("Error: {content}")
    } else {
        content.to_owned()
    }
}

/// The wire role for a spot-llm message.
fn wire_role(role: spot_llm::message::Role) -> String {
    match role {
        spot_llm::message::Role::User => "user".to_owned(),
        spot_llm::message::Role::Assistant => "assistant".to_owned(),
        spot_llm::message::Role::Tool => "tool".to_owned(),
    }
}

/// Builds the outbound tool definitions.
fn outbound_tools(tools: &[ToolSpec]) -> Vec<OutboundTool> {
    tools
        .iter()
        .map(|tool| OutboundTool {
            kind: ToolKind::Function,
            function: OutboundFunction {
                name: tool.name.clone(),
                description: tool.description.clone(),
                parameters: tool.input_schema.clone(),
            },
        })
        .collect()
}

/// Builds the outbound wire body for `request` under `profile`, against the model catalogue
/// `models`. Fallible: the model must be known, tools must be supported when declared, and
/// the temperature must sit in `0..=1_000` permille. `streaming` sets the reply's `stream`
/// flag and, where the profile says usage is streamed, asks for it.
pub fn outbound(
    request: &Request,
    profile: &Profile,
    models: &[ModelInfo],
    streaming: bool,
) -> Result<Outbound, BuildError> {
    let model = models
        .iter()
        .find(|candidate| candidate.id == request.model)
        .ok_or_else(|| BuildError::UnknownModel(request.model.clone()))?;
    if !profile.supports_tools && !request.tools.is_empty() {
        return Err(BuildError::ToolsUnsupported);
    }
    let temperature = request
        .temperature
        .map(|permille| temperature_text(permille).ok_or(BuildError::Temperature(permille)))
        .transpose()?
        .map(|text| {
            // `RawValue::from_string` owns the validated JSON text; the text is built
            // from `u16`s only, so it is always a bare valid number.
            serde_json::value::RawValue::from_string(text)
                .map_err(|_| BuildError::Temperature(request.temperature.unwrap_or_default()))
        })
        .transpose()?;
    Ok(Outbound {
        model: model.id.clone(),
        // Chat Completions carries the system prompt as a first `system`-role message;
        // compatible servers read it there (some accept a top-level field, the message
        // form is the shared one).
        messages: std::iter::once(request.system.as_ref().map(|system| OutboundMessage {
            role: "system".to_owned(),
            content: OutboundContent::Text(system.clone()),
            tool_calls: None,
            tool_call_id: None,
        }))
        .flatten()
        .chain(request.messages.iter().map(outbound_message))
        .collect(),
        tools: (!request.tools.is_empty()).then(|| outbound_tools(&request.tools)),
        max_output_tokens: request
            .max_output_tokens
            .map(|cap| match profile.max_tokens_field {
                MaxTokensField::MaxTokens => CappedTokens::Max { max_tokens: cap },
                MaxTokensField::MaxCompletionTokens => CappedTokens::Completion {
                    max_completion_tokens: cap,
                },
            }),
        temperature,
        stops: request.stop_sequences.clone(),
        stream: streaming,
        stream_options: (streaming && profile.supports_streaming_usage).then_some(StreamOptions {
            include_usage: true,
        }),
    })
}

/// The tool-call wire entry an assistant message carries.
#[derive(Clone, Debug, PartialEq, Serialize)]
struct OutboundToolCallWire {
    /// The call id.
    id: String,
    /// Always `"function"`.
    #[serde(rename = "type")]
    kind: FunctionKind,
    /// The called function.
    function: FunctionWire,
}

/// The `type: "function"` tag.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
enum FunctionKind {
    /// `"function"`.
    #[serde(rename = "function")]
    Function,
}

/// The called function: name plus arguments.
#[derive(Clone, Debug, PartialEq, Serialize)]
struct FunctionWire {
    /// The tool name.
    name: String,
    /// The arguments, as the JSON value the model produced — re-serialised to the wire's
    /// JSON text shape.
    arguments: ArgumentsWire,
}

impl ArgumentsWire {
    /// Wraps `value`.
    fn new(value: Value) -> Self {
        Self(value)
    }
}

/// The tool-declaration wire entry.
#[derive(Clone, Debug, PartialEq, Serialize)]
struct OutboundTool {
    /// Always `"function"`.
    #[serde(rename = "type")]
    kind: ToolKind,
    /// The declared function.
    function: OutboundFunction,
}

/// The tool declaration's `type` tag.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
enum ToolKind {
    /// `"function"`.
    #[serde(rename = "function")]
    Function,
}

/// The declared function: name, description, JSON schema.
#[derive(Clone, Debug, PartialEq, Serialize)]
struct OutboundFunction {
    /// The tool name.
    name: String,
    /// What the tool does, in the caller's words.
    description: String,
    /// The arguments schema.
    parameters: Value,
}

/// The tool-call arguments on the wire: the JSON value re-serialised as its compact text
/// inside a JSON string — the protocol's `function.arguments` shape — never a float in,
/// never a float out.
#[derive(Clone, Debug, PartialEq)]
struct ArgumentsWire(Value);

impl Serialize for ArgumentsWire {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0.to_string())
    }
}

/// The wire body. Only [`outbound`] builds one; `Debug` is manual so the body never
/// prints (it could quote the request).
#[derive(Clone, Serialize)]
pub struct Outbound {
    /// The model id.
    pub(crate) model: String,
    /// The conversation; the system prompt rides as the first `system`-role message.
    messages: Vec<OutboundMessage>,
    /// The tool definitions.
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<OutboundTool>>,
    /// The cap on generated tokens, under the profile's field name.
    #[serde(skip_serializing_if = "Option::is_none", flatten)]
    max_output_tokens: Option<CappedTokens>,
    /// The temperature, as the pre-formatted decimal text — embedded verbatim as a JSON
    /// number, never a float.
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<Box<serde_json::value::RawValue>>,
    /// The sequences that end generation early (the Chat Completions `stop` field).
    #[serde(rename = "stop", skip_serializing_if = "Vec::is_empty")]
    stops: Vec<String>,
    /// Whether the reply should stream as SSE events.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    stream: bool,
    /// `include_usage` when the profile says the endpoint reports streaming usage.
    #[serde(skip_serializing_if = "Option::is_none")]
    stream_options: Option<StreamOptions>,
}

/// The cap on generated tokens under one of the field's two names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(untagged)]
enum CappedTokens {
    /// `{max_tokens: N}`.
    Max {
        /// The cap.
        max_tokens: u32,
    },
    /// `{max_completion_tokens: N}`.
    Completion {
        /// The cap.
        max_completion_tokens: u32,
    },
}

/// The `stream_options` entry asking for streaming usage.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
struct StreamOptions {
    /// Always `true`.
    include_usage: bool,
}

impl std::fmt::Debug for Outbound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Only shapes print, never field values: the body could quote the request.
        f.debug_struct("Outbound")
            .field("model", &self.model)
            .field("messages", &self.messages.len())
            .field("tools", &self.tools.as_ref().map_or(0, Vec::len))
            .field("max_output_tokens", &self.max_output_tokens.is_some())
            .field("temperature", &self.temperature.is_some())
            .field("stops", &self.stops.len())
            .field("stream", &self.stream)
            .field("stream_options", &self.stream_options.is_some())
            .finish()
    }
}

/// One outbound message: role, plain or structured text content, tool calls, tool-call id.
#[derive(Clone, Debug, PartialEq, Serialize)]
struct OutboundMessage {
    /// `"user"`, `"assistant"`, `"tool"` or `"system"`.
    role: String,
    /// The `content` field: a plain string, or `null` when the message has no text (a pure
    /// tool-call assistant turn).
    content: OutboundContent,
    /// The assistant's tool calls, when the message carries any.
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<OutboundToolCallWire>>,
    /// The call id a `tool`-role message answers.
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

/// The `content` field: a string or `null` — the wire's two shapes for a message without
/// tool calls to quote.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(untagged)]
enum OutboundContent {
    /// The classic `"content": "text"` string.
    Text(String),
    /// `"content": null`.
    Null,
}

/// The buffered reply's shape.
#[derive(Clone, Debug, Deserialize)]
pub struct Inbound {
    /// The choice the interface reads (a request names one model and no `n`). An empty
    /// array is tolerated at parse time; the reader raises `llm.protocol` on it.
    #[serde(default)]
    pub choices: Vec<InboundChoice>,
    /// The reported usage; optional — some compatible endpoints do not report it.
    #[serde(default)]
    pub usage: Option<UsageField>,
    /// The endpoint's error object, when the body is an error under a 200 status.
    #[serde(default)]
    pub error: Option<ErrorEntry>,
}

/// One choice of the reply.
#[derive(Clone, Debug, Deserialize)]
pub struct InboundChoice {
    /// The assistant's message.
    #[serde(default)]
    pub message: Option<InboundMessage>,
    /// `stop`, `length`, `tool_calls`, `content_filter`, or an unknown value.
    #[serde(default)]
    pub finish_reason: Option<String>,
}

/// The assistant message of a buffered reply.
#[derive(Clone, Debug, Deserialize)]
pub struct InboundMessage {
    /// The assistant's text; `null` on a pure tool-call reply.
    #[serde(default)]
    pub content: Option<String>,
    /// The tool calls, when the model asked for one or more.
    #[serde(default)]
    pub tool_calls: Option<Vec<InboundToolCall>>,
}

/// One buffered tool call: id, function name, arguments as JSON text.
#[derive(Clone, Debug, Deserialize)]
pub struct InboundToolCall {
    /// The call id the tool result must quote back.
    #[serde(default)]
    pub id: Option<String>,
    /// Always `"function"` when present.
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    /// The called function.
    #[serde(default)]
    pub function: Option<FunctionValue>,
}

/// The called function on the wire: name plus the arguments text.
#[derive(Clone, Debug, Deserialize)]
pub struct FunctionValue {
    /// The tool name the model quoted.
    #[serde(default)]
    pub name: Option<String>,
    /// The arguments as JSON text.
    #[serde(default)]
    pub arguments: Option<String>,
}

/// The reply's usage fields; absent fields read as zero downstream.
#[derive(Clone, Copy, Debug, Default, Deserialize)]
pub struct UsageField {
    /// Prompt tokens read.
    #[serde(default)]
    prompt_tokens: Option<u64>,
    /// Tokens generated.
    #[serde(default)]
    completion_tokens: Option<u64>,
    /// Prompt tokens served from cache (`prompt_tokens_details.cached_tokens`, flattened).
    #[serde(default)]
    cached_tokens: Option<u64>,
}

/// The endpoint's error object — the documented Chat Completions error shape, which
/// compatible servers also return under a 200 status.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorEntry {
    /// The machine error code (`invalid_request_error`, `rate_limit_error`, ...), any
    /// compatible server's own string.
    #[serde(default)]
    pub code: Option<String>,
    /// The endpoint's human message.
    #[serde(default)]
    pub message: Option<String>,
    /// The error's type field, when the server spells one.
    #[serde(default)]
    pub r#type: Option<String>,
}

impl UsageField {
    /// The usage as a spot-llm usage.
    pub fn usage(&self) -> Usage {
        Usage {
            input_tokens: self.prompt_tokens.unwrap_or_default(),
            output_tokens: self.completion_tokens.unwrap_or_default(),
            cache_read_tokens: self.cached_tokens.unwrap_or_default(),
            cache_write_tokens: 0,
        }
    }
}

impl Inbound {
    /// Whether this inbound body is an error object (a 200-status body can be one).
    pub fn is_error(&self) -> bool {
        self.error.is_some()
    }

    /// The parsed error entry, when the body is one.
    pub fn error_entry(&self) -> Option<&ErrorEntry> {
        self.error.as_ref()
    }

    /// The parsed body as a spot-llm [`Usage`], zeroed when the endpoint reported none.
    pub fn usage(&self) -> Usage {
        self.usage.as_ref().map_or(Usage::ZERO, UsageField::usage)
    }
}
