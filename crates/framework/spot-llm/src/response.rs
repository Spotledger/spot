//! Responses: the model's reply, why it stopped, and the token usage it reports.

use serde::{Deserialize, Serialize};

use crate::message::Message;

/// Token accounting for one request. The units are the provider's own reported counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Prompt tokens read.
    #[serde(default)]
    pub input_tokens: u64,
    /// Tokens generated.
    #[serde(default)]
    pub output_tokens: u64,
    /// Prompt tokens served from cache.
    #[serde(default)]
    pub cache_read_tokens: u64,
    /// Prompt tokens written to cache.
    #[serde(default)]
    pub cache_write_tokens: u64,
}

impl Usage {
    /// All-zero usage.
    pub const ZERO: Usage = Usage {
        input_tokens: 0,
        output_tokens: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
    };

    /// Prompt tokens read plus cache tokens, i.e. everything the input cost.
    pub fn input_total(&self) -> u64 {
        self.input_tokens + self.cache_read_tokens + self.cache_write_tokens
    }
}

/// Why the model stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    /// The model finished on its own.
    Stop,
    /// An output limit was hit (`max_output_tokens`, or the model's own limit).
    Length,
    /// The model asked for a tool call.
    ToolUse,
    /// The provider filtered the content.
    ContentFilter,
}

/// The provider's reply to one request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Response {
    /// The reply message: text and tool-use parts.
    pub message: Message,
    /// Why generation stopped.
    pub stop_reason: StopReason,
    /// The token usage the provider reported.
    pub usage: Usage,
}

impl Response {
    /// A response with the given message, stop reason and usage.
    pub fn new(message: Message, stop_reason: StopReason, usage: Usage) -> Self {
        Self {
            message,
            stop_reason,
            usage,
        }
    }

    /// The reply's text parts, joined with newlines.
    pub fn text(&self) -> String {
        self.message.text_content()
    }
}
