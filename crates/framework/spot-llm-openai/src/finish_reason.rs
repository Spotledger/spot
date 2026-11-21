//! The `finish_reason` mapping: the Chat Completions wire names onto [`StopReason`].

use spot_llm::response::StopReason;

/// Maps a wire `finish_reason` onto [`StopReason`]: `stop`, `length`, `tool_calls` and
/// `content_filter` are the documented families. Failures name the unknown value — the
/// fold must not invent a reason the wire never carried.
pub fn map(name: &str) -> Result<StopReason, String> {
    match name {
        "stop" | "end_turn" => Ok(StopReason::Stop),
        "length" | "max_tokens" | "max_output_tokens" => Ok(StopReason::Length),
        "tool_calls" | "function_call" => Ok(StopReason::ToolUse),
        "content_filter" => Ok(StopReason::ContentFilter),
        other => Err(format!("unknown finish_reason '{other}'")),
    }
}
