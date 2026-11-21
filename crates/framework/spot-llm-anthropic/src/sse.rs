//! The SSE parser: Anthropic's event stream into [`StreamEvent`]s.
//!
//! Every documented event (`message_start`, `content_block_start` / `content_block_delta` /
//! `content_block_stop`, `message_delta`, `message_stop`, `ping`, `error`) is mapped; unknown
//! event kinds and unknown block kinds are tolerated and skipped. Usage arrives in two halves
//! — `message_start` carries the input and cache totals, `message_delta` the final output —
//! so the parser merges them per field (a non-zero value wins) and emits one [`Usage`].
//! A data payload that is not the documented JSON shape, a stop or argument fragment for a
//! block that never opened, or a stream ending without its `message_stop` is a
//! `llm.protocol` failure.

use serde_json::Value;
use spot_llm::error::LlmError;
use spot_llm::response::{Response, StopReason, Usage};
use spot_llm::stream::{StreamEvent, assemble};

/// One SSE frame as parsed off the wire: its `event:` name, if any, and its joined `data:`
/// payload. Frames are owned, so multi-line payloads join without borrow gymnastics.
struct Frame {
    event: Option<String>,
    data: String,
}

/// Splits `body` into SSE frames: every block of lines separated by a blank line, with
/// multi-line `data:` payloads joined by `\n`, and comment lines (Anthropic's `: ping`
/// keep-alives) plus `id:` / `retry:` fields dropped, as SSE instructs.
fn frames(body: &str) -> Vec<Frame> {
    let mut out: Vec<Frame> = Vec::new();
    for block in body.split("\n\n") {
        let mut event: Option<String> = None;
        let mut data_parts: Vec<&str> = Vec::new();
        for line in block.lines() {
            let line = line.trim_end_matches('\r');
            if line.starts_with(':') || line.starts_with("id:") || line.starts_with("retry:") {
                continue;
            }
            if let Some(rest) = line.strip_prefix("event:") {
                event = Some(rest.trim_start().to_owned());
            } else if let Some(rest) = line.strip_prefix("data:") {
                data_parts.push(rest.strip_prefix(' ').unwrap_or(rest));
            }
            // Any other line is framing noise; SSE says ignore unknown fields.
        }
        if data_parts.is_empty() {
            continue;
        }
        out.push(Frame {
            event,
            data: data_parts.join("\n"),
        });
    }
    out
}

/// Parses every SSE frame in `body` into [`StreamEvent`]s.
///
/// The merged usage is appended once, after every frame; the tool bracket events carry the
/// wire's own block indexes. Fails with `llm.protocol` on: a non-JSON or non-object data
/// payload, a malformed payload for a documented event kind, a `message_delta` stop reason
/// the interface does not model, an argument fragment or stop for a block that never opened
/// as a tool use, or a stream ending without its `message_stop`.
pub fn parse_events(body: &str) -> Result<Vec<StreamEvent>, LlmError> {
    let mut events: Vec<StreamEvent> = Vec::new();
    let mut merged = Usage::ZERO;
    let mut open_tools: Vec<u32> = Vec::new();
    let mut saw_message_stop = false;
    let mut ended_with_error: Option<String> = None;

    for frame in frames(body) {
        let payload: Value = match serde_json::from_str(&frame.data) {
            Ok(payload) => payload,
            Err(_) => return Err(protocol("an sse data payload was not valid JSON")),
        };
        let Some(fields) = payload.as_object() else {
            return Err(protocol("an sse data payload was not a JSON object"));
        };
        // The data payload's `type` field decides; the `event:` line fills in for a payload
        // that carries none.
        let kind = fields
            .get("type")
            .and_then(Value::as_str)
            .or(frame.event.as_deref())
            .unwrap_or_default();

        match kind {
            "message_start" => {
                let message = fields
                    .get("message")
                    .and_then(Value::as_object)
                    .ok_or_else(|| protocol("message_start carried no message object"))?;
                let usage_field = message
                    .get("usage")
                    .ok_or_else(|| protocol("message_start carried no usage"))?;
                merge_usage(usage_field, &mut merged);
            }
            "content_block_start" => {
                let index = index_of(fields)?;
                let block = fields
                    .get("content_block")
                    .and_then(Value::as_object)
                    .ok_or_else(|| protocol("content_block_start carried no content_block"))?;
                // Text opens nothing; unknown block kinds (future thinking or redacted
                // blocks) are tolerated and skipped.
                let is_tool_use = block.get("type").and_then(Value::as_str) == Some("tool_use");
                if is_tool_use {
                    let id = block
                        .get("id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| protocol("a tool_use block carried no id"))?
                        .to_owned();
                    let name = block
                        .get("name")
                        .and_then(Value::as_str)
                        .ok_or_else(|| protocol("a tool_use block carried no name"))?
                        .to_owned();
                    open_tools.push(index);
                    events.push(StreamEvent::ToolUseStart { index, id, name });
                }
            }
            "content_block_delta" => {
                let index = index_of(fields)?;
                let delta = fields
                    .get("delta")
                    .and_then(Value::as_object)
                    .ok_or_else(|| protocol("content_block_delta carried no delta"))?;
                match delta
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                {
                    "text_delta" => {
                        let text = delta
                            .get("text")
                            .and_then(Value::as_str)
                            .ok_or_else(|| protocol("a text_delta carried no text"))?
                            .to_owned();
                        events.push(StreamEvent::TextDelta { delta: text });
                    }
                    "input_json_delta" => {
                        if !open_tools.contains(&index) {
                            return Err(protocol(format!(
                                "an argument fragment arrived for block {index}, which never \
                                 opened as a tool use"
                            )));
                        }
                        let fragment = delta
                            .get("partial_json")
                            .and_then(Value::as_str)
                            .ok_or_else(|| protocol("an input_json_delta carried no partial_json"))?
                            .to_owned();
                        events.push(StreamEvent::ToolUseDelta {
                            index,
                            delta: fragment,
                        });
                    }
                    // Unknown delta kinds are tolerated and skipped.
                    _ => {}
                }
            }
            "content_block_stop" => {
                let index = index_of(fields)?;
                // Only a tool-use block's stop closes a call; text (and unknown) block stops
                // carry no tool semantics.
                if let Some(at) = open_tools.iter().position(|open| *open == index) {
                    open_tools.remove(at);
                    events.push(StreamEvent::ToolUseEnd { index });
                }
            }
            "message_delta" => {
                if let Some(raw) = fields
                    .get("delta")
                    .and_then(Value::as_object)
                    .and_then(|delta| delta.get("stop_reason"))
                {
                    events.push(StreamEvent::Stop {
                        reason: stop_reason_of(raw)?,
                    });
                }
                if let Some(usage_field) = fields.get("usage") {
                    merge_usage(usage_field, &mut merged);
                }
            }
            "message_stop" => saw_message_stop = true,
            "ping" => {}
            "error" => {
                ended_with_error = Some(stream_error_detail(&payload));
                break;
            }
            // An unknown event kind is tolerated: skipped, not fatal.
            _ => {}
        }
    }

    if let Some(message) = ended_with_error {
        return Err(protocol(format!(
            "the stream carried an error event: {message}"
        )));
    }
    if !saw_message_stop {
        return Err(protocol("the stream ended before its message_stop"));
    }
    events.push(StreamEvent::Usage { usage: merged });
    Ok(events)
}

/// Parses the stream into the [`Response`] its events fold to — the buffered shape the
/// provider's `complete` returns for the same reply.
pub fn parse(body: &str) -> Result<Response, LlmError> {
    assemble(&parse_events(body)?)
}

/// The provider's message on a stream-level `error` event, when readable; never a header, key
/// or request body — the error body is the provider's own sentence.
fn stream_error_detail(fields: &Value) -> String {
    let inner = fields.get("error").unwrap_or(fields);
    serde_json::from_value::<StreamErrorBody>(inner.clone())
        .ok()
        .and_then(|body| body.message)
        .unwrap_or_else(|| "unspecified".to_owned())
}

/// The inner shape of a stream-level error event, read only for its message.
#[derive(serde::Deserialize)]
struct StreamErrorBody {
    message: Option<String>,
}

/// Reads the zero-based block `index` off an event payload.
fn index_of(fields: &serde_json::Map<String, Value>) -> Result<u32, LlmError> {
    fields
        .get("index")
        .and_then(Value::as_u64)
        .and_then(|index| u32::try_from(index).ok())
        .ok_or_else(|| protocol("a block event carried no usable index"))
}

/// Merges one wire usage object into the running total: the documented halves are
/// complementary (`message_start` names input and cache totals, `message_delta` the final
/// output), so a non-zero incoming value overwrites and a zero never clobbers one.
fn merge_usage(field: &Value, merged: &mut Usage) {
    let read = |name: &str| field.get(name).and_then(Value::as_u64);
    if let Some(value) = read("input_tokens")
        && value != 0
    {
        merged.input_tokens = value;
    }
    if let Some(value) = read("output_tokens")
        && value != 0
    {
        merged.output_tokens = value;
    }
    if let Some(value) = read("cache_read_input_tokens")
        && value != 0
    {
        merged.cache_read_tokens = value;
    }
    if let Some(value) = read("cache_creation_input_tokens")
        && value != 0
    {
        merged.cache_write_tokens = value;
    }
}

/// Maps a wire `stop_reason` string onto [`StopReason`]: `end_turn`, `max_tokens`, `tool_use`
/// and `refusal`; an undocumented value is a protocol failure, because the fold would
/// otherwise invent a reason the wire never carried.
fn stop_reason_of(raw: &Value) -> Result<StopReason, LlmError> {
    match raw.as_str() {
        Some("end_turn") => Ok(StopReason::Stop),
        Some("max_tokens") => Ok(StopReason::Length),
        Some("tool_use") => Ok(StopReason::ToolUse),
        Some("refusal") => Ok(StopReason::ContentFilter),
        other => Err(protocol(match other {
            Some(name) => format!("unknown stop_reason '{name}'"),
            None => "the wire stop_reason was not a string".to_owned(),
        })),
    }
}

/// A `llm.protocol` error with `detail`.
fn protocol(detail: impl Into<String>) -> LlmError {
    LlmError::Protocol {
        detail: detail.into(),
    }
}
