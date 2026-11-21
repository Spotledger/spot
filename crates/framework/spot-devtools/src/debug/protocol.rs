//! The Debug Adapter Protocol wire: `Content-Length` framing and the message shapes.
//!
//! DAP is JSON with a byte-length header (`Content-Length: <n>\r\n\r\n<n bytes of JSON>`). One
//! connection carries requests from the client, responses to them and events. This module is the
//! whole wire format; the policy and the script session live elsewhere.

use std::io::{BufRead, Write};

use serde_json::{Value, json};

use crate::error::{DevToolsError, Result};

/// The most bytes one DAP message may have. A debugger message is small; a larger one is a
/// malformed header, not a trace payload.
pub const MAX_MESSAGE_BYTES: usize = 8 * 1024 * 1024;

/// A request read from the client.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    /// The client's sequence number, echoed in the response.
    pub seq: u64,
    /// The command (`initialize`, `setBreakpoints`, ...).
    pub command: String,
    /// The command's arguments; `null` when absent.
    pub arguments: Value,
}

/// One outgoing message.
#[derive(Clone, Debug, PartialEq)]
pub enum Outgoing {
    /// The answer to a request.
    Response {
        /// The request's sequence number.
        request_seq: u64,
        /// The command answered.
        command: String,
        /// Whether it succeeded.
        success: bool,
        /// Why it failed, when it did.
        message: Option<String>,
        /// The command-specific body.
        body: Option<Value>,
    },
    /// An asynchronous event.
    Event {
        /// The event name (`stopped`, `terminated`, `output`, ...).
        event: String,
        /// The event body.
        body: Value,
    },
}

impl Outgoing {
    /// A successful response with a body.
    #[must_use]
    pub fn ok(request_seq: u64, command: &str, body: Value) -> Self {
        Self::Response {
            request_seq,
            command: command.to_owned(),
            success: true,
            message: None,
            body: Some(body),
        }
    }

    /// A successful response with no body.
    #[must_use]
    pub fn ok_empty(request_seq: u64, command: &str) -> Self {
        Self::Response {
            request_seq,
            command: command.to_owned(),
            success: true,
            message: None,
            body: None,
        }
    }

    /// A failed response.
    #[must_use]
    pub fn failed(request_seq: u64, command: &str, error: &DevToolsError) -> Self {
        Self::Response {
            request_seq,
            command: command.to_owned(),
            success: false,
            message: Some(error.render()),
            body: None,
        }
    }

    /// An event.
    #[must_use]
    pub fn event(event: &str, body: Value) -> Self {
        Self::Event {
            event: event.to_owned(),
            body,
        }
    }

    /// The message as DAP JSON, with this adapter's next sequence number.
    #[must_use]
    pub fn to_json(&self, seq: u64) -> Value {
        match self {
            Self::Response {
                request_seq,
                command,
                success,
                message,
                body,
            } => {
                let mut value = json!({
                    "seq": seq,
                    "type": "response",
                    "request_seq": request_seq,
                    "success": success,
                    "command": command,
                });
                if let Some(message) = message {
                    value["message"] = Value::String(message.clone());
                }
                if let Some(body) = body {
                    value["body"] = body.clone();
                }
                value
            }
            Self::Event { event, body } => json!({
                "seq": seq,
                "type": "event",
                "event": event,
                "body": body,
            }),
        }
    }
}

/// Reads the headers of one message and returns its declared body length.
///
/// # Errors
///
/// [`DevToolsError::Protocol`] when the headers are not `name: value` lines, or no
/// `Content-Length` is present, or it is over [`MAX_MESSAGE_BYTES`].
pub fn read_length(header: &str) -> Result<usize> {
    let mut length = None;
    for line in header.lines() {
        let Some((name, value)) = line.split_once(':') else {
            return Err(DevToolsError::Protocol {
                message: format!(
                    "header line `{}` is not `name: value` (header `{}`)",
                    line.escape_debug(),
                    header.escape_debug()
                ),
            });
        };
        if name.trim().eq_ignore_ascii_case("content-length") {
            let parsed: usize = value.trim().parse().map_err(|_| DevToolsError::Protocol {
                message: format!("`{value}` is not a byte length"),
            })?;
            length = Some(parsed);
        }
    }
    let length = length.ok_or_else(|| DevToolsError::Protocol {
        message: format!(
            "no Content-Length header (header `{}`)",
            header.escape_debug()
        ),
    })?;
    if length > MAX_MESSAGE_BYTES {
        return Err(DevToolsError::Protocol {
            message: format!("a {length}-byte message exceeds the {MAX_MESSAGE_BYTES}-byte cap"),
        });
    }
    Ok(length)
}

/// Reads one message. `Ok(None)` is a clean end of stream (no bytes of a header).
///
/// # Errors
///
/// [`DevToolsError::Protocol`] for a malformed header, a short body or a body that is not JSON.
pub fn read_message<R: BufRead>(reader: &mut R) -> Result<Option<Vec<u8>>> {
    let mut header = String::new();
    loop {
        let mut line = String::new();
        let read = reader
            .read_line(&mut line)
            .map_err(|e| DevToolsError::Protocol {
                message: format!("reading a header: {e}"),
            })?;
        if read == 0 {
            if header.is_empty() {
                return Ok(None);
            }
            return Err(DevToolsError::Protocol {
                message: "the stream ended in the middle of a header".into(),
            });
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        header.push_str(&line);
    }
    let length = read_length(&header)?;
    let mut body = vec![0_u8; length];
    reader
        .read_exact(&mut body)
        .map_err(|e| DevToolsError::Protocol {
            message: format!("reading a {length}-byte body: {e}"),
        })?;
    Ok(Some(body))
}

/// Parses a request body.
///
/// # Errors
///
/// [`DevToolsError::Protocol`] when the JSON is not a DAP `request`.
pub fn parse_request(body: &[u8]) -> Result<Request> {
    let value: Value = serde_json::from_slice(body).map_err(|e| DevToolsError::Protocol {
        message: format!("the message is not JSON: {e}"),
    })?;
    if value.get("type").and_then(Value::as_str) != Some("request") {
        return Err(DevToolsError::Protocol {
            message: "the message is not a DAP request".into(),
        });
    }
    let command = value
        .get("command")
        .and_then(Value::as_str)
        .ok_or_else(|| DevToolsError::Protocol {
            message: "the request names no command".into(),
        })?
        .to_owned();
    Ok(Request {
        seq: value.get("seq").and_then(Value::as_u64).unwrap_or(0),
        command,
        arguments: value.get("arguments").cloned().unwrap_or(Value::Null),
    })
}

/// Writes one message, framed.
///
/// # Errors
///
/// [`DevToolsError::Io`] when the stream refuses the write.
pub fn write_message<W: Write>(writer: &mut W, message: &Value) -> Result<()> {
    let body = serde_json::to_vec(message).map_err(|e| DevToolsError::Protocol {
        message: format!("serialising a message: {e}"),
    })?;
    write!(writer, "Content-Length: {}\r\n\r\n", body.len())
        .and_then(|()| writer.write_all(&body))
        .and_then(|()| writer.flush())
        .map_err(|e| DevToolsError::Io {
            path: "<dap stream>".into(),
            message: e.to_string(),
        })
}

/// The `initialize` response body: what this adapter supports.
#[must_use]
pub fn capabilities() -> Value {
    json!({
        "supportsConfigurationDoneRequest": true,
        "supportsSetVariable": false,
        "supportsEvaluateForHovers": true,
        "supportsTerminateRequest": true,
        "supportsRestartRequest": false,
        "supportsStepBack": false,
        "supportsFunctionBreakpoints": false,
        "supportsConditionalBreakpoints": false,
        "supportsHitConditionalBreakpoints": false,
        "supportsExceptionInfoRequest": false,
        "supportsDelayedStackTraceLoading": false,
        "supportsLogPoints": false,
        "supportsDataBreakpoints": false,
        "supportsCompletionsRequest": false,
        "supportsDisassembleRequest": false,
        "supportsReadMemoryRequest": false,
        "supportsModulesRequest": false,
        "exceptionBreakpointFilters": [],
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::io::Cursor;

    use super::{Outgoing, capabilities, parse_request, read_length, read_message, write_message};

    #[test]
    fn a_framed_message_round_trips() {
        let mut wire = Vec::new();
        write_message(&mut wire, &capabilities()).expect("write");
        assert!(wire.starts_with(b"Content-Length: "));
        assert!(wire.windows(4).any(|window| window == b"\r\n\r\n"));
        let mut reader = Cursor::new(wire);
        let body = read_message(&mut reader)
            .expect("read")
            .expect("one message");
        let value: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(value["supportsConfigurationDoneRequest"], true);
        assert!(read_message(&mut reader).expect("eof").is_none());
    }

    #[test]
    fn requests_are_parsed_and_other_types_refused() {
        let request =
            parse_request(br#"{"seq":7,"type":"request","command":"launch","arguments":{"x":1}}"#)
                .expect("request");
        assert_eq!(request.seq, 7);
        assert_eq!(request.command, "launch");
        assert_eq!(request.arguments["x"], 1);
        let missing = parse_request(br#"{"seq":1,"type":"request"}"#);
        assert_eq!(missing.expect_err("no command").code(), "devtools.protocol");
        let event = parse_request(br#"{"seq":1,"type":"event","event":"x"}"#);
        assert!(event.is_err());
    }

    #[test]
    fn bad_headers_are_refused() {
        assert_eq!(
            read_length("Nope: 3").expect_err("no length").code(),
            "devtools.protocol"
        );
        assert!(read_length("Content-Length: nope").is_err());
        assert!(read_length("Content-Length: 999999999").is_err());
        assert_eq!(read_length("content-length: 12").expect("length"), 12);
        let mut short = Cursor::new(b"Content-Length: 10\r\n\r\n{}".to_vec());
        assert!(read_message(&mut short).is_err());
        let mut garbage = Cursor::new(b"Content-Length: 1\r\n\r\n{".to_vec());
        let body = read_message(&mut garbage)
            .expect("framing only")
            .expect("body");
        assert!(parse_request(&body).is_err());
    }

    #[test]
    fn responses_and_events_use_the_dap_type_names() {
        let response = Outgoing::ok(3, "initialize", capabilities());
        let json = response.to_json(1);
        assert_eq!(json["type"], "response");
        assert_eq!(json["request_seq"], 3);
        assert_eq!(json["command"], "initialize");
        let event = Outgoing::event("stopped", serde_json::json!({"reason": "breakpoint"}));
        assert_eq!(event.to_json(2)["type"], "event");
    }
}
