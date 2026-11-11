//! The span model: one trace id, one span id, a parent, a layer, a name and attributes.
//!
//! Times are `Duration`s on the injected monotonic clock ([`spot_core::Clock::monotonic`]); this
//! crate never reads the wall clock. A span's `start` and `end` are therefore relative to
//! whatever origin the caller's clock uses, and a renderer subtracts the trace's first start
//! before printing.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use serde_json::{Map, Value};

/// The layer a span belongs to: the request's path through the platform.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Layer {
    /// The HTTP transport: request accepted, route, response written.
    Http,
    /// A lifecycle step of a document: validate, before-save, after-save.
    Lifecycle,
    /// One step of a pipeline (workflow, report, import, ...).
    PipelineStep,
    /// A TypeScript script run by `spot-ts`.
    Script,
    /// A WebAssembly host-boundary crossing: one guest export call or host import.
    Wasm,
    /// A storage statement or unit of work.
    Storage,
    /// A background job.
    Job,
}

impl Layer {
    /// Every layer, in the order the request passes through them at its widest.
    pub const ALL: [Layer; 7] = [
        Layer::Http,
        Layer::Lifecycle,
        Layer::PipelineStep,
        Layer::Script,
        Layer::Storage,
        Layer::Wasm,
        Layer::Job,
    ];

    /// The stable lower-case name used in traces, JSON and the CLI.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Lifecycle => "lifecycle",
            Self::PipelineStep => "pipeline_step",
            Self::Script => "script",
            Self::Wasm => "wasm",
            Self::Storage => "storage",
            Self::Job => "job",
        }
    }

    /// The layer `name` names, if it names one.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|layer| layer.as_str() == name)
    }
}

impl fmt::Display for Layer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A trace id: 32 lower-case hex digits, the shape of a W3C `traceparent` id.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TraceId(String);

impl TraceId {
    /// A trace id from a 128-bit value (formatted as 32 hex digits).
    #[must_use]
    pub fn from_u128(value: u128) -> Self {
        Self(format!("{value:032x}"))
    }

    /// A trace id from its hex text, if it is 32 lower-case hex digits.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        if text.len() == 32 && text.bytes().all(|b| b.is_ascii_hexdigit()) {
            Some(Self(text.to_ascii_lowercase()))
        } else {
            None
        }
    }

    /// The hex text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TraceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A span id: 16 lower-case hex digits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SpanId(u64);

impl SpanId {
    /// A span id from its value.
    #[must_use]
    pub fn from_u64(value: u64) -> Self {
        Self(value)
    }

    /// The value.
    #[must_use]
    pub fn value(self) -> u64 {
        self.0
    }

    /// The 16-hex-digit text.
    #[must_use]
    pub fn as_hex(self) -> String {
        format!("{:016x}", self.0)
    }
}

impl fmt::Display for SpanId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_hex())
    }
}

/// Attribute names whose values never enter a trace, whatever the value.
const SENSITIVE_NAMES: &[&str] = &[
    "authorization",
    "proxy-authorization",
    "cookie",
    "set-cookie",
    "api_key",
    "api-key",
    "password",
    "secret",
    "session",
    "session_token",
    "token",
    "access_token",
    "refresh_token",
];

/// Whether an attribute name must never carry its value into a trace. `token` and `secret` match
/// anywhere in the name, so a future `x-csrf-token` is covered without a change.
#[must_use]
pub fn is_sensitive_attribute(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    SENSITIVE_NAMES.contains(&lower.as_str())
        || lower.contains("token")
        || lower.contains("secret")
        || lower.contains("password")
        || lower.contains("authorization")
        || lower.contains("cookie")
        || lower.contains("api_key")
        || lower.contains("api-key")
        || lower.contains("apikey")
}

/// One span of one trace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TraceSpan {
    /// The trace this span belongs to.
    pub trace_id: TraceId,
    /// This span's id.
    pub span_id: SpanId,
    /// The parent span, `None` for the root.
    pub parent: Option<SpanId>,
    /// The layer.
    pub layer: Layer,
    /// What the span is (`lifecycle.before_save`, `script:on_validate`, ...).
    pub name: String,
    /// Low-cardinality attributes. Values are already redacted.
    pub attributes: BTreeMap<String, String>,
    /// When the span started, on the injected monotonic clock.
    pub start: Duration,
    /// When it ended, if it did.
    pub end: Option<Duration>,
    /// The error that ended it, if any. Already redacted.
    pub error: Option<String>,
}

impl TraceSpan {
    /// The span's duration, if it ended.
    #[must_use]
    pub fn duration(&self) -> Option<Duration> {
        self.end.map(|end| end.saturating_sub(self.start))
    }

    /// The span as JSON: integers only (no floats), microsecond offsets relative to `origin`.
    #[must_use]
    pub fn to_json(&self, origin: Duration) -> Value {
        let mut object = Map::new();
        object.insert("traceId".into(), Value::String(self.trace_id.to_string()));
        object.insert("spanId".into(), Value::String(self.span_id.as_hex()));
        if let Some(parent) = self.parent {
            object.insert("parentSpanId".into(), Value::String(parent.as_hex()));
        }
        object.insert("layer".into(), Value::String(self.layer.as_str().into()));
        object.insert("name".into(), Value::String(self.name.clone()));
        object.insert(
            "startMicros".into(),
            Value::from(micros(self.start.saturating_sub(origin))),
        );
        if let Some(end) = self.end {
            object.insert(
                "endMicros".into(),
                Value::from(micros(end.saturating_sub(origin))),
            );
        }
        let attributes: Map<String, Value> = self
            .attributes
            .iter()
            .map(|(k, v)| (k.clone(), Value::String(v.clone())))
            .collect();
        object.insert("attributes".into(), Value::Object(attributes));
        if let Some(error) = &self.error {
            object.insert("error".into(), Value::String(error.clone()));
        }
        Value::Object(object)
    }
}

/// A duration in whole microseconds, saturating at `u64::MAX` (no float, no panic).
#[must_use]
pub fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{Layer, SpanId, TraceId, TraceSpan, is_sensitive_attribute};

    #[test]
    fn layers_round_trip_and_ids_have_a_fixed_shape() {
        for layer in Layer::ALL {
            assert_eq!(Layer::parse(layer.as_str()), Some(layer));
        }
        assert_eq!(Layer::parse("nope"), None);
        assert_eq!(Layer::PipelineStep.as_str(), "pipeline_step");
        let trace = TraceId::from_u128(0xabc);
        assert_eq!(trace.as_str(), "00000000000000000000000000000abc");
        assert_eq!(TraceId::parse(trace.as_str()), Some(trace.clone()));
        assert!(TraceId::parse("ABC").is_none());
        assert_eq!(SpanId::from_u64(0xff).as_hex(), "00000000000000ff");
    }

    #[test]
    fn sensitive_attribute_names_are_recognised_in_every_case() {
        for name in [
            "Authorization",
            "Cookie",
            "x-csrf-token",
            "client_secret",
            "PASSWORD",
        ] {
            assert!(is_sensitive_attribute(name), "{name}");
        }
        assert!(!is_sensitive_attribute("route"));
        assert!(!is_sensitive_attribute("doctype"));
    }

    #[test]
    fn json_offsets_are_relative_integers() {
        let span = TraceSpan {
            trace_id: TraceId::from_u128(1),
            span_id: SpanId::from_u64(2),
            parent: None,
            layer: Layer::Http,
            name: "GET /api/x".into(),
            attributes: [("route".into(), "/api/x".into())].into(),
            start: Duration::from_millis(5),
            end: Some(Duration::from_millis(7)),
            error: None,
        };
        let json = span.to_json(Duration::from_millis(5));
        assert_eq!(json["startMicros"], 0);
        assert_eq!(json["endMicros"], 2000);
        assert_eq!(json["attributes"]["route"], "/api/x");
        assert!(json["endMicros"].is_u64());
    }
}
