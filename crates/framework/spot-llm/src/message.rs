//! Messages: a role plus structured parts, so text, tool use and tool results are first class
//! instead of a string with protocol escapes.

use serde::{Deserialize, Serialize};

/// Who is speaking. The system prompt lives on the [`Request`](crate::Request), not here.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// The end user, or the agent acting for them.
    User,
    /// The model: text and possibly [`Part::ToolUse`] parts.
    Assistant,
    /// The caller answering a tool use with a [`Part::ToolResult`].
    Tool,
}

/// One part of a [`Message`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Part {
    /// Plain text.
    Text {
        /// The text itself.
        text: String,
    },
    /// The model asked to call a tool.
    ToolUse {
        /// The call id the tool result must quote back.
        id: String,
        /// The tool name as declared in the request's tools.
        name: String,
        /// Arguments as a JSON value satisfying the tool's schema.
        arguments: serde_json::Value,
    },
    /// The caller's answer to a tool use.
    ToolResult {
        /// The tool-use id this result answers.
        tool_use_id: String,
        /// The result, as text.
        content: String,
        /// Whether the tool failed; defaults to `false`.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        is_error: bool,
    },
}

impl Part {
    /// The part's text, for text parts only.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Part::Text { text } => Some(text),
            _ => None,
        }
    }
}

/// One turn in a conversation: a [`Role`] plus its [`Part`]s in order.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// Who is speaking.
    pub role: Role,
    /// The parts, in order.
    pub parts: Vec<Part>,
}

impl Message {
    /// A message with a single text part.
    pub fn text(role: Role, text: impl Into<String>) -> Self {
        Self {
            role,
            parts: vec![Part::Text { text: text.into() }],
        }
    }

    /// A user text message.
    pub fn user(text: impl Into<String>) -> Self {
        Self::text(Role::User, text)
    }

    /// An assistant text message.
    pub fn assistant(text: impl Into<String>) -> Self {
        Self::text(Role::Assistant, text)
    }

    /// A tool result answering `tool_use_id`; a successful result by default.
    pub fn tool_result(tool_use_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            parts: vec![Part::ToolResult {
                tool_use_id: tool_use_id.into(),
                content: content.into(),
                is_error: false,
            }],
        }
    }

    /// Appends a part.
    pub fn push(&mut self, part: Part) {
        self.parts.push(part);
    }

    /// Appends a text part.
    pub fn push_text(&mut self, text: impl Into<String>) {
        self.push(Part::Text { text: text.into() });
    }

    /// All text parts joined with newlines; tool parts are not text.
    pub fn text_content(&self) -> String {
        let texts: Vec<&str> = self.parts.iter().filter_map(Part::as_text).collect();
        texts.join("\n")
    }

    /// The tool-use parts, in order, as `(id, name, arguments)`.
    pub fn tool_uses(&self) -> Vec<ToolUse> {
        self.parts
            .iter()
            .filter_map(|part| match part {
                Part::ToolUse {
                    id,
                    name,
                    arguments,
                } => Some(ToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    arguments: arguments.clone(),
                }),
                _ => None,
            })
            .collect()
    }
}

/// A tool use, as `(id, name, arguments)` — the readable projection of a
/// [`Part::ToolUse`].
#[derive(Clone, Debug, PartialEq)]
pub struct ToolUse {
    /// The call id the tool result must quote back.
    pub id: String,
    /// The tool name.
    pub name: String,
    /// The arguments.
    pub arguments: serde_json::Value,
}
