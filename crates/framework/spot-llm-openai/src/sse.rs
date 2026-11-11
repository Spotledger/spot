//! The SSE parser: Chat Completions `data:` chunks into [`StreamEvent`]s.
//!
//! Every Chat Completions SSE block is `data: {json}\n\n`; the stream terminates with a
//! bare `data: [DONE]`. A delta chunk carries a `choices[0].delta` holding a `content`
//! piece or indexed `tool_calls` fragments — an id, a tool name and an arguments string on
//! the opening entry for an index, further chunks appending argument text under the same
//! index. The parser opens a bracket per index (`ToolUseStart`), emits a bracket delta per
//! fragment, and closes every still-open bracket when the `finish_reason` chunk arrives or
//! at `[DONE]`, so a streamed tool call always brackets the same call
//! [`spot_llm::stream::assemble`] folds back. Usage from any chunk's `usage` object (the
//! final chunk when the profile asked for it, `stream_options.include_usage`) merges into
//! one reported total, emitted as a single `Usage` event at the end.
//!
//! Fails with `llm.protocol` on: a non-JSON or non-object data payload, a streamed error
//! object, a chunk carrying neither `choices` nor `usage`, a tool-call fragment for an
//! index whose opening entry carried no id or name, argument fragments of an unusable
//! type, an unmappable `finish_reason`, a `[DONE]` arriving before any `finish_reason`, or
//! a stream ending without its `[DONE]`.

use serde_json::Value;
use spot_llm::error::LlmError;
use spot_llm::response::{Response, StopReason, Usage};
use spot_llm::stream::{StreamEvent, assemble};

/// One SSE frame as parsed off the wire: its joined `data:` payload. Owned, so multi-line
/// payloads join without borrow gymnastics. (Chat Completions names no `event:` types —
/// the data payload alone decides — so an `event:` line is dropped as framing.)
struct Frame {
    data: String,
}

/// Splits `body` into SSE frames: every block of lines separated by a blank line, with
/// multi-line `data:` payloads joined by `\n`, and comment lines (keep-alives), `event:`
/// names and `id:` / `retry:` fields dropped, as SSE instructs.
fn frames(body: &str) -> Vec<Frame> {
    let mut out: Vec<Frame> = Vec::new();
    for block in body.split("\n\n") {
        let mut data_parts: Vec<&str> = Vec::new();
        for line in block.lines() {
            let line = line.trim_end_matches('\r');
            if line.starts_with(':')
                || line.starts_with("id:")
                || line.starts_with("retry:")
                || line.starts_with("event:")
            {
                continue;
            }
            if let Some(rest) = line.strip_prefix("data:") {
                data_parts.push(rest.strip_prefix(' ').unwrap_or(rest));
            }
            // Any other line is framing noise; SSE says ignore unknown fields.
        }
        if data_parts.is_empty() {
            continue;
        }
        out.push(Frame {
            data: data_parts.join("\n"),
        });
    }
    out
}

/// One tool-call index open on the stream, and its argument fragments so far.
struct OpenCall {
    /// The wire's own call index.
    index: u32,
    /// The argument fragments concatenated, in wire order.
    fragment: String,
    /// Whether the bracket end has already been emitted.
    closed: bool,
}

/// Parses every SSE frame in `body` into [`StreamEvent`]s.
///
/// The tool-call brackets carry the wire's own call indexes; an argument-less call closes
/// over an empty object so the fold sees a valid no-argument call. Fails with
/// `llm.protocol` on any of the shapes [the module doc](self) names.
pub fn parse(body: &str) -> Result<Vec<StreamEvent>, LlmError> {
    let mut events: Vec<StreamEvent> = Vec::new();
    let mut open: Vec<OpenCall> = Vec::new();
    let mut merged = Usage::ZERO;
    let mut saw_done = false;
    let mut reason: Option<StopReason> = None;

    for frame in frames(body) {
        if frame.data.trim() == "[DONE]" {
            saw_done = true;
            break;
        }
        let payload: Value = match serde_json::from_str(&frame.data) {
            Ok(payload) => payload,
            Err(_) => return Err(protocol("an sse data payload was not valid JSON")),
        };
        let Some(fields) = payload.as_object() else {
            return Err(protocol("an sse data payload was not a JSON object"));
        };
        if let Some(entry) = fields.get("error")
            && entry.is_object()
        {
            let detail = serde_json::from_value::<crate::wire::ErrorEntry>(entry.clone())
                .ok()
                .and_then(|entry| entry.message)
                .unwrap_or_else(|| "unspecified".to_owned());
            return Err(protocol(format!(
                "the stream carried an error object: {detail}"
            )));
        }
        if fields.get("choices").is_none() && fields.get("usage").is_none() {
            return Err(protocol(
                "a chunk carried neither choices nor a usage object",
            ));
        }
        if let Some(usage_field) = fields.get("usage") {
            let wire_usage = serde_json::from_value::<crate::wire::UsageField>(usage_field.clone())
                .map_err(|_| protocol("the streamed usage object carried unusable fields"))?;
            merge_usage(wire_usage.usage(), &mut merged);
        }
        let Some(choice) = fields
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|choices| choices.first())
        else {
            // A usage-only final chunk, or an empty choices array: nothing to read here.
            continue;
        };
        let Some(delta) = choice.get("delta") else {
            return Err(protocol("a chunk choice carried no delta"));
        };
        let Some(delta) = delta.as_object() else {
            return Err(protocol("a chunk choice's delta was not an object"));
        };

        if let Some(text) = delta.get("content").and_then(Value::as_str)
            && !text.is_empty()
        {
            events.push(StreamEvent::TextDelta {
                delta: text.to_owned(),
            });
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            fold_tool_calls(calls, &mut events, &mut open)?;
        }
        if let Some(raw) = choice.get("finish_reason").and_then(Value::as_str) {
            let mapped =
                crate::finish_reason::map(raw).map_err(|detail| protocol(detail.clone()))?;
            if reason.is_none() {
                reason = Some(mapped);
                force_close(&mut events, &mut open);
                events.push(StreamEvent::Stop { reason: mapped });
            }
        }
    }

    if !saw_done {
        return Err(protocol("the stream ended before its `[DONE]`"));
    }
    // A stream that ended without a finish_reason chunk still closes its open brackets;
    // the fold below then fails on the missing stop event, as it should.
    force_close(&mut events, &mut open);
    events.push(StreamEvent::Usage { usage: merged });
    Ok(events)
}

/// Parses the stream into the [`Response`] its events fold to — the buffered shape the
/// provider's `complete` returns for the same reply.
pub fn parse_complete(body: &str) -> Result<Response, LlmError> {
    assemble(&parse(body)?)
}

/// Folds one chunk's `tool_calls` array into the events: the opening entry for an index
/// emits the bracket start, every arguments entry emits a bracket delta.
fn fold_tool_calls(
    calls: &[Value],
    events: &mut Vec<StreamEvent>,
    open: &mut Vec<OpenCall>,
) -> Result<(), LlmError> {
    for call in calls {
        let index = call
            .get("index")
            .and_then(Value::as_u64)
            .and_then(|index| u32::try_from(index).ok())
            .ok_or_else(|| protocol("a tool_calls chunk carried no usable index"))?;
        let function = call.get("function").and_then(Value::as_object);
        if !open.iter().any(|entry| entry.index == index) {
            let id = call
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    protocol(format!(
                        "the tool-call chunk for index {index} carried no id on its opening \
                         fragment"
                    ))
                })?
                .to_owned();
            let name = function
                .and_then(|function| function.get("name"))
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    protocol(format!(
                        "the tool-call chunk for index {index} carried no tool name"
                    ))
                })?
                .to_owned();
            open.push(OpenCall {
                index,
                fragment: String::new(),
                closed: false,
            });
            events.push(StreamEvent::ToolUseStart { index, id, name });
        }
        if let Some(args) = function.and_then(|function| function.get("arguments")) {
            let piece = match args {
                Value::String(piece) => piece.clone(),
                // Some compatible servers send the arguments object whole on one entry.
                Value::Object(_) => args.to_string(),
                // A null is the opening entry's "no arguments yet".
                Value::Null => String::new(),
                _ => {
                    return Err(protocol(format!(
                        "the tool-call delta for index {index} carried arguments of an \
                         unusable type"
                    )));
                }
            };
            if !piece.is_empty() {
                if let Some(entry) = open.iter_mut().find(|entry| entry.index == index) {
                    entry.fragment.push_str(&piece);
                }
                events.push(StreamEvent::ToolUseDelta {
                    index,
                    delta: piece,
                });
            }
        }
    }
    Ok(())
}

/// Emits the bracket end for every still-open call — the finish_reason chunk's or the
/// `[DONE]`'s close. An argument-less call (no fragments at all) closes over an empty
/// object, so the fold sees a valid no-argument call rather than a missing one.
fn force_close(events: &mut Vec<StreamEvent>, open: &mut [OpenCall]) {
    for entry in open.iter_mut() {
        if !entry.closed {
            entry.closed = true;
            if entry.fragment.is_empty() {
                events.push(StreamEvent::ToolUseDelta {
                    index: entry.index,
                    delta: "{}".to_owned(),
                });
            }
            events.push(StreamEvent::ToolUseEnd { index: entry.index });
        }
    }
}

/// Merges one wire usage into the running total: the complementary halves of the stream
/// (an early chunk may carry none, the final one carries the output) merge field-wise,
/// a non-zero value winning and a zero never clobbering one.
fn merge_usage(incoming: Usage, merged: &mut Usage) {
    if incoming.input_tokens != 0 {
        merged.input_tokens = incoming.input_tokens;
    }
    if incoming.output_tokens != 0 {
        merged.output_tokens = incoming.output_tokens;
    }
    if incoming.cache_read_tokens != 0 {
        merged.cache_read_tokens = incoming.cache_read_tokens;
    }
}

/// A `llm.protocol` error with `detail`.
fn protocol(detail: impl Into<String>) -> LlmError {
    LlmError::Protocol {
        detail: detail.into(),
    }
}
