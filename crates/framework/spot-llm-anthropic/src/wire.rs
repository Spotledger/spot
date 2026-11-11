//! The typed wire shapes for `/v1/messages` — request build, response read.
//!
//! Outbound building is fallible (missing model, temperature over 1 000), so callers get
//! `Result`s; the provider turns those into `llm.bad_request` / `llm.protocol` before the
//! transport is touched, and the details name the field, never the body contents.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::cache::CacheMarker;
use crate::provider::ModelInfo;
use spot_llm::request::{Request, ToolSpec};
use spot_llm::response::Usage;

/// Failure while building an outbound request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BuildError {
    /// The request's model is not in the provider's catalogue.
    UnknownModel(String),
    /// `max_tokens` cannot sit inside the model's context window, or exceeds the model cap.
    MaxTokens(u64),
    /// The temperature permille is out of range (`0..=1_000`).
    Temperature(u16),
}

/// Every content block the outbound messages carry, from the spot-llm parts.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OutboundBlock {
    /// Plain text.
    Text {
        /// The text.
        text: String,
    },
    /// The assistant's tool call.
    ToolUse {
        /// The call id.
        id: String,
        /// The tool name.
        name: String,
        /// The arguments, as the model produced them.
        #[serde(rename = "input")]
        input: Value,
    },
    /// The caller's tool answer.
    ToolResult {
        /// The call id answered.
        tool_use_id: String,
        /// The result, as text.
        content: String,
        /// Whether the tool failed.
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        is_error: bool,
    },
}

/// Builds the outbound content blocks for one message.
fn blocks_of(message: &spot_llm::message::Message) -> Vec<OutboundBlock> {
    message
        .parts
        .iter()
        .map(|part| match part {
            spot_llm::message::Part::Text { text } => OutboundBlock::Text { text: text.clone() },
            spot_llm::message::Part::ToolUse {
                id,
                name,
                arguments,
            } => OutboundBlock::ToolUse {
                id: id.clone(),
                name: name.clone(),
                input: arguments.clone(),
            },
            spot_llm::message::Part::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => OutboundBlock::ToolResult {
                tool_use_id: tool_use_id.clone(),
                content: content.clone(),
                is_error: *is_error,
            },
        })
        .collect()
}

/// One message on the wire: the role spelled the API's way, plus its blocks.
#[derive(Clone, Debug, PartialEq, Serialize)]
struct OutboundMessage {
    /// `"user"` or `"assistant"`; tool results ride the `user` role, as documented.
    role: &'static str,
    /// The blocks, in order.
    content: Vec<OutboundBlock>,
}

/// One tool definition on the wire: name, description, JSON schema, optional cache marker.
#[derive(Clone, Debug, PartialEq, Serialize)]
struct OutboundTool {
    /// The tool name.
    name: String,
    /// The description.
    description: String,
    /// The JSON schema.
    #[serde(rename = "input_schema")]
    schema: Value,
    /// `{"type": "ephemeral"}` on the last tool when caching is on.
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<Ephemeral>,
}

/// The cache marker's wire shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
struct Ephemeral {
    /// Always `"ephemeral"`.
    r#type: EphemeralKind,
}

/// The only cache-control type the API documents.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
enum EphemeralKind {
    /// `{"type": "ephemeral"}`.
    #[serde(rename = "ephemeral")]
    Ephemeral,
}

/// The outbound request body. Only [`Outbound::from_request`] builds one; `Debug` and
/// `PartialEq` are manual so the body never prints or compares (only shapes).
#[derive(Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct Outbound {
    /// The Messages API's version-spelled model id.
    model: String,
    /// The required cap on generated tokens.
    max_tokens: u64,
    /// The system prompt, one text block; `cache_control` rides it when marked.
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<Vec<OutboundSystemBlock>>,
    /// The conversation.
    messages: Vec<OutboundMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    /// The tool definitions.
    tools: Vec<OutboundTool>,
    /// The temperature, as the pre-formatted decimal text — embedded verbatim as a JSON
    /// number, never a float.
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<Box<serde_json::value::RawValue>>,
    /// The stop sequences.
    #[serde(rename = "stop_sequences", skip_serializing_if = "Vec::is_empty")]
    stops: Vec<String>,
    /// Whether the reply should stream as SSE events.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    stream: bool,
}

/// One system block: text plus its optional `cache_control`.
#[derive(Clone, Debug, PartialEq, Serialize)]
struct OutboundSystemBlock {
    /// `"text"`.
    r#type: SystemBlockKind,
    /// The prompt text.
    text: String,
    /// `{"type": "ephemeral"}` when the system is marked.
    #[serde(skip_serializing_if = "Option::is_none")]
    cache_control: Option<Ephemeral>,
}

/// The system block's type tag.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
enum SystemBlockKind {
    /// `"text"`.
    #[serde(rename = "text")]
    Text,
}

/// Formats a temperature permille into the API's decimal text, no floats: `0` → `"0"`,
/// `700` → `"0.7"`, `1_000` → `"1"`; `1_234` is rejected by the range check at the caller.
pub(crate) fn temperature_text(permille: u16) -> Option<String> {
    if permille > 1_000 {
        return None;
    }
    if permille.is_multiple_of(1_000) {
        // An integer temperature: "0" and "1" style, no decimal part.
        return Some((permille / 1_000).to_string());
    }
    if permille.is_multiple_of(100) {
        // One decimal digit: 700 permille is "0.7".
        return Some(format!("{}.{}", permille / 1_000, permille % 1_000 / 100));
    }
    if permille.is_multiple_of(10) {
        // Two decimal digits: 120 permille is "0.12".
        return Some(format!("{}.{}", permille / 1_000, permille % 1_000 / 10));
    }
    // Three decimal digits: 999 permille is "0.999".
    Some(format!("{}.{}", permille / 1_000, permille % 1_000))
}

fn ephemeral() -> Ephemeral {
    Ephemeral {
        r#type: EphemeralKind::Ephemeral,
    }
}

impl std::fmt::Debug for Outbound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Only shapes print, never field values: the body could quote the request.
        f.debug_struct("Outbound")
            .field("model", &self.model)
            .field("max_tokens", &self.max_tokens)
            .field("system", &self.system.as_ref().map_or(0, Vec::len))
            .field("messages", &self.messages.len())
            .field("tools", &self.tools.len())
            .field("temperature", &self.temperature.is_some())
            .field("stops", &self.stops.len())
            .field("stream", &self.stream)
            .finish()
    }
}

impl PartialEq for Outbound {
    fn eq(&self, other: &Self) -> bool {
        // Equality compares everything except the raw temperature text, which has no cheap
        // equality; two bodies differing only in it are unequal on the wire anyway.
        self.model == other.model
            && self.max_tokens == other.max_tokens
            && self.system == other.system
            && self.messages == other.messages
            && self.tools == other.tools
            && self.temperature.is_some() == other.temperature.is_some()
            && self.stops == other.stops
            && self.stream == other.stream
    }
}

impl Eq for Outbound {}

impl Outbound {
    /// Builds the wire body for `request` under `options`, against the model catalogue
    /// `models`. Fallible: the model must be known, `max_tokens` must fit, and the
    /// temperature must sit in `0..=1_000` permille. `streaming` sets the reply's `stream`
    /// flag (the API streams when asked in the body, on the same endpoint).
    pub fn from_request(
        request: &Request,
        options: &crate::provider::AnthropicOptions,
        models: &[ModelInfo],
        streaming: bool,
    ) -> Result<Self, BuildError> {
        let model = models
            .iter()
            .find(|candidate| candidate.id == request.model)
            .ok_or_else(|| BuildError::UnknownModel(request.model.clone()))?;

        // max_tokens is required by the API: the caller's override, else the model's cap.
        let requested = options
            .max_tokens
            .map(u64::from)
            .unwrap_or(model.max_output);
        if requested == 0 || requested > model.max_output {
            return Err(BuildError::MaxTokens(model.max_output));
        }

        // Temperature: permille in, pre-formatted decimal text out; no float crosses here.
        let temperature = request
            .temperature
            .map(|permille| temperature_text(permille).ok_or(BuildError::Temperature(permille)))
            .transpose()?
            .map(|text| {
                // `RawValue::from_string` owns the validated JSON text; the text is built
                // from `u16`s only, so it is always a bare valid number.
                serde_json::value::RawValue::from_string(text)
                    .map_err(|_| BuildError::Temperature(request.temperature.unwrap_or(0)))
            })
            .transpose()?;

        let stops = request.stop_sequences.clone();
        let messages: Vec<OutboundMessage> = request
            .messages
            .iter()
            .map(|message| OutboundMessage {
                role: wire_role(message.role),
                content: blocks_of(message),
            })
            .collect();
        let tools = tools_of(&request.tools, options.cache);
        let system = request.system.as_ref().map(|text| {
            let block = OutboundSystemBlock {
                r#type: SystemBlockKind::Text,
                text: text.clone(),
                cache_control: options.cache.marks_system().then(ephemeral),
            };
            vec![block]
        });
        Ok(Self {
            model: model.id.clone(),
            max_tokens: requested,
            system,
            messages,
            tools,
            temperature,
            stops,
            stream: streaming,
        })
    }
}

/// The wire role for a spot-llm message: tool results answer on the `user` role.
fn wire_role(role: spot_llm::message::Role) -> &'static str {
    match role {
        spot_llm::message::Role::User | spot_llm::message::Role::Tool => "user",
        spot_llm::message::Role::Assistant => "assistant",
    }
}

/// Builds the tool definitions, marking the last one when caching asks.
fn tools_of(tools: &[ToolSpec], cache: CacheMarker) -> Vec<OutboundTool> {
    let last = tools.len().checked_sub(1);
    tools
        .iter()
        .enumerate()
        .map(|(at, tool)| OutboundTool {
            name: tool.name.clone(),
            description: tool.description.clone(),
            schema: tool.input_schema.clone(),
            cache_control: (cache.marks_last_tool() && last == Some(at)).then(ephemeral),
        })
        .collect()
}

/// The inbound reply's content blocks (only the kinds this interface carries).
#[derive(Clone, Debug, Deserialize)]
pub struct InboundBlock {
    /// `"text"`, `"tool_use"`, or an unknown kind (skipped).
    #[serde(rename = "type", default)]
    pub kind: String,
    /// The text on a text block.
    #[serde(default)]
    pub text: Option<String>,
    /// The call id on a tool_use block.
    #[serde(default)]
    pub id: Option<String>,
    /// The tool name on a tool_use block.
    #[serde(default)]
    pub name: Option<String>,
    /// The arguments on a tool_use block.
    #[serde(default)]
    pub input: Option<Value>,
}

/// The inbound reply's usage fields; absent fields read as zero downstream.
#[derive(Clone, Copy, Debug, Default, Deserialize)]
struct UsageField {
    /// Prompt tokens read.
    #[serde(default)]
    input_tokens: Option<u64>,
    /// Tokens generated.
    #[serde(default)]
    output_tokens: Option<u64>,
    /// Prompt tokens served from cache.
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
    /// Prompt tokens written to cache.
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
}

/// The buffered reply's shape.
#[derive(Clone, Debug, Deserialize)]
pub struct Inbound {
    /// The content blocks, in order.
    pub content: Vec<InboundBlock>,
    /// The wire stop reason.
    #[serde(default)]
    pub stop_reason: Option<String>,
    /// The reported usage.
    #[serde(default)]
    usage: UsageField,
}

impl Inbound {
    /// The reply as a spot-llm usage.
    pub fn usage(&self) -> Usage {
        Usage {
            input_tokens: self.usage.input_tokens.unwrap_or(0),
            output_tokens: self.usage.output_tokens.unwrap_or(0),
            cache_read_tokens: self.usage.cache_read_input_tokens.unwrap_or(0),
            cache_write_tokens: self.usage.cache_creation_input_tokens.unwrap_or(0),
        }
    }
}
