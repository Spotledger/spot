//! The request a caller sends: model, messages, system prompt, tools and limits.

use serde::{Deserialize, Serialize};

use crate::message::Message;

/// A tool the caller declares; the model may answer with a
/// [`Part::ToolUse`](crate::Part::ToolUse).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    /// The tool name; the model quotes it back in the tool use.
    pub name: String,
    /// What the tool does, in the caller's words.
    pub description: String,
    /// The arguments schema (JSON Schema) the model's arguments must satisfy.
    pub input_schema: serde_json::Value,
}

/// One model request: the conversation, its tools and its limits.
///
/// `system` sits outside the message list, as every provider's system prompt does; `request_id`
/// is the caller's own id for the exchange — providers must not need anything else to
/// correlate logs, and the mock provider scripts responses against it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Request {
    /// The model id, in the provider's own spelling.
    pub model: String,
    /// The conversation so far; the newest message ends it.
    pub messages: Vec<Message>,
    /// The system prompt for the conversation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    /// The tools the model may call.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolSpec>,
    /// The cap on generated tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    /// The temperature in permille (1_000 = 1.0); no float crosses this crate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<u16>,
    /// The sequences that end generation early.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stop_sequences: Vec<String>,
    /// The caller's id for this exchange.
    pub request_id: String,
}

impl Request {
    /// A minimal request for `model` with id `request_id`; add the rest with the builders.
    pub fn new(model: impl Into<String>, request_id: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            messages: Vec::new(),
            system: None,
            tools: Vec::new(),
            max_output_tokens: None,
            temperature: None,
            stop_sequences: Vec::new(),
            request_id: request_id.into(),
        }
    }

    /// Sets the system prompt.
    pub fn with_system(mut self, system: impl Into<String>) -> Self {
        self.system = Some(system.into());
        self
    }

    /// Appends a message to the conversation.
    pub fn with_message(mut self, message: Message) -> Self {
        self.messages.push(message);
        self
    }

    /// Declares a tool the model may call.
    pub fn with_tool(mut self, tool: ToolSpec) -> Self {
        self.tools.push(tool);
        self
    }

    /// Caps the generated tokens.
    pub fn with_max_output_tokens(mut self, max_output_tokens: u32) -> Self {
        self.max_output_tokens = Some(max_output_tokens);
        self
    }

    /// Sets the temperature in permille (1_000 = 1.0).
    pub fn with_temperature_permille(mut self, temperature: u16) -> Self {
        self.temperature = Some(temperature);
        self
    }

    /// Adds a stop sequence.
    pub fn with_stop_sequence(mut self, stop: impl Into<String>) -> Self {
        self.stop_sequences.push(stop.into());
        self
    }
}
