//! Streaming: the events a provider's `stream` yields, and the shared assembler.

use std::pin::Pin;
use std::task::Poll;

use futures_core::Stream;
use serde::{Deserialize, Serialize};

use crate::error::LlmError;
use crate::message::{Message, Part};
use crate::response::{Response, StopReason, Usage};

/// A stream of [`StreamEvent`]s, or one typed [`LlmError`] mid-stream.
pub type BoxStream = Pin<Box<dyn Stream<Item = Result<StreamEvent, LlmError>> + Send + 'static>>;

/// One increment of a streamed reply.
///
/// Providers emit `text_delta` pieces, bracket each tool call with `tool_use_start`,
/// `tool_use_delta` and `tool_use_end` (the index ties them together), report `usage` once,
/// and always end with `stop`. [`assemble`] folds these into the same [`Response`] that
/// `complete` returns for the same request.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    /// A piece of the reply text.
    TextDelta {
        /// The piece, appended to the reply so far.
        delta: String,
    },
    /// A tool call opened.
    ToolUseStart {
        /// The zero-based call index within the reply.
        index: u32,
        /// The call id the tool result must quote back.
        id: String,
        /// The tool name.
        name: String,
    },
    /// A piece of a tool call's arguments: a JSON fragment, concatenated per call index.
    ToolUseDelta {
        /// The call index the fragment belongs to.
        index: u32,
        /// The JSON fragment.
        delta: String,
    },
    /// A tool call's arguments are complete.
    ToolUseEnd {
        /// The call index that is complete.
        index: u32,
    },
    /// Token usage, as the provider reported it.
    Usage {
        /// The usage.
        usage: Usage,
    },
    /// The stream finished. Later events, if any, are ignored.
    Stop {
        /// Why generation stopped.
        reason: StopReason,
    },
}

/// A stream over the owned items of a vector — what mocks and replayed chunk records hand
/// back.
#[derive(Debug)]
pub struct IterStream<T> {
    items: std::vec::IntoIter<T>,
}

impl<T> IterStream<T> {
    /// Wraps `items` into a stream.
    pub fn new(items: Vec<T>) -> Self {
        Self {
            items: items.into_iter(),
        }
    }
}

impl<T: Unpin> IterStream<T> {
    /// Every item wrapped as `Ok`, the convenience for streaming successes.
    pub fn ok(items: Vec<T>) -> Vec<Result<T, crate::error::LlmError>> {
        items.into_iter().map(Ok).collect()
    }
}

impl<T: Unpin> Stream for IterStream<T> {
    type Item = T;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut std::task::Context<'_>) -> Poll<Option<T>> {
        Poll::Ready(self.get_mut().items.next())
    }
}

/// Drains a [`BoxStream`] to its end, returning every event or failure in order.
pub async fn drain(stream: &mut BoxStream) -> Vec<Result<StreamEvent, LlmError>> {
    let mut out = Vec::new();
    loop {
        let item = std::future::poll_fn(|cx| stream.as_mut().poll_next(cx)).await;
        match item {
            Some(event) => out.push(event),
            None => return out,
        }
    }
}

/// Assembles streamed [`StreamEvent`]s into the [`Response`] that `complete` returns for the
/// same request — the shared "stream equals complete" check of the conformance suite.
///
/// Fails with `llm.protocol` when the stream is unusable: an argument fragment without a
/// matching open call, an `end` without a matching start, arguments that are not a JSON value,
/// arguments missing entirely, or a stream ending without a `stop`.
pub fn assemble(events: &[StreamEvent]) -> Result<Response, LlmError> {
    let mut text = String::new();
    let mut calls: Vec<(u32, String, String, String)> = Vec::new();
    let mut usage = Usage::ZERO;
    let mut reason: Option<StopReason> = None;

    for event in events {
        match event {
            StreamEvent::TextDelta { delta } => text.push_str(delta),
            StreamEvent::ToolUseStart { index, id, name } => {
                if calls.iter().any(|(open, ..)| open == index) {
                    return Err(protocol(format!("tool call {index} started twice")));
                }
                calls.push((*index, id.clone(), name.clone(), String::new()));
            }
            StreamEvent::ToolUseDelta { index, delta } => {
                let call = find_open(&mut calls, *index)?;
                call.push_str(delta);
            }
            StreamEvent::ToolUseEnd { index } => {
                find_open(&mut calls, *index)?;
            }
            StreamEvent::Usage { usage: reported } => usage = *reported,
            StreamEvent::Stop { reason: why } => reason = Some(*why),
        }
    }

    let Some(reason) = reason else {
        return Err(protocol("the stream ended without a stop event".into()));
    };

    let mut parts = Vec::new();
    if !text.is_empty() {
        parts.push(Part::Text { text });
    }
    for (_, id, name, args) in calls {
        let arguments: serde_json::Value = if args.is_empty() {
            return Err(protocol(format!(
                "tool call '{name}' ended without argument fragments"
            )));
        } else {
            serde_json::from_str(&args)
                .map_err(|_| protocol(format!("tool call '{name}' arguments are not JSON")))?
        };
        parts.push(Part::ToolUse {
            id,
            name,
            arguments,
        });
    }

    Ok(Response::new(
        Message {
            role: crate::message::Role::Assistant,
            parts,
        },
        reason,
        usage,
    ))
}

fn protocol(detail: String) -> LlmError {
    LlmError::Protocol { detail }
}

/// Finds the open call carrying `index`, whether it takes more fragments or not.
fn find_open(
    calls: &mut [(u32, String, String, String)],
    index: u32,
) -> Result<&mut String, LlmError> {
    calls
        .iter_mut()
        .find(|(open, ..)| *open == index)
        .map(|(.., args)| args)
        .ok_or_else(|| protocol(format!("tool call {index} was never started")))
}
