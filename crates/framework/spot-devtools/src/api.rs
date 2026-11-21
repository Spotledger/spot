//! The developer trace viewer API: handler functions for `/dev/traces` and `/dev/traces/{id}`.
//!
//! The crate does not own the HTTP server, so these are plain functions over an
//! [`ApiResponse`]: the host wires [`DevApi::handle_checked`], the only routable entry, behind the
//! dev site's router, and the CLI's `spot trace <request-id>` prints
//! [`DevApi::render_request`].
//!
//! Only traces of this collector's site are visible; a trace id that is not in the ring is a 404.
//! Every string in the body came from a span attribute, so it passed the collector's redactor
//! before it could get here.

use serde_json::{Value, json};

use crate::safety::DebugPolicy;
use crate::trace::collector::{Trace, TraceCollector, TraceSummary};
use crate::trace::render::{format_duration, render_layers, render_summaries, render_tree};
use crate::trace::span::micros;

/// The response format of a trace view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// The span tree as JSON.
    Json,
    /// The indented text tree and the layer summary, what a terminal wants.
    Text,
}

impl Format {
    /// `text` selects [`Format::Text`]; anything else is JSON.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        if text.eq_ignore_ascii_case("text") || text.eq_ignore_ascii_case("tree") {
            Self::Text
        } else {
            Self::Json
        }
    }
}

/// What a handler returns: status, content type and body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiResponse {
    /// The HTTP status code.
    pub status: u16,
    /// The `Content-Type`.
    pub content_type: &'static str,
    /// The body.
    pub body: String,
}

impl ApiResponse {
    /// A JSON response.
    #[must_use]
    pub fn json(status: u16, body: &Value) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: body.to_string(),
        }
    }

    /// A text response.
    #[must_use]
    pub fn text(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            content_type: "text/plain; charset=utf-8",
            body: body.into(),
        }
    }
}

/// The two routes the dev site wires.
pub const ROUTES: [&str; 2] = ["/dev/traces", "/dev/traces/{id}"];

/// The viewer: a handle on the site's collector.
///
/// The only entry a router can reach is [`DevApi::handle_checked`], which refuses a production
/// site and a missing or wrong development token before it answers. The unchecked dispatch is
/// private, so a router cannot be wired around the check:
///
/// ```compile_fail,E0624
/// use spot_devtools::{DevApi, TraceCollector};
///
/// let site = spot_core::SiteId::new("acme.example").unwrap();
/// let api = DevApi::new(TraceCollector::new(site, 4));
/// let _ = api.handle("GET", "/dev/traces", None);
/// ```
///
/// ```compile_fail,E0624
/// use spot_devtools::{DevApi, TraceCollector};
///
/// let site = spot_core::SiteId::new("acme.example").unwrap();
/// let api = DevApi::new(TraceCollector::new(site, 4));
/// let _ = api.list(None, 10);
/// ```
#[derive(Clone, Debug)]
pub struct DevApi {
    collector: TraceCollector,
}

impl DevApi {
    /// A viewer over `collector`.
    #[must_use]
    pub fn new(collector: TraceCollector) -> Self {
        Self { collector }
    }

    /// The collector behind the viewer.
    #[must_use]
    pub fn collector(&self) -> &TraceCollector {
        &self.collector
    }

    /// One request against the dev router, **unchecked**; private so only
    /// [`DevApi::handle_checked`] reaches it. `path` is the request path (`/dev/traces` or
    /// `/dev/traces/{id}`), `query` the raw query string without `?`.
    #[must_use]
    fn handle(&self, method: &str, path: &str, query: Option<&str>) -> ApiResponse {
        if !method.eq_ignore_ascii_case("GET") && !method.eq_ignore_ascii_case("HEAD") {
            return ApiResponse::json(
                405,
                &json!({"error": "method_not_allowed", "message": "GET only"}),
            );
        }
        let query = Query::parse(query.unwrap_or(""));
        match path.strip_prefix("/dev/traces") {
            Some("") | Some("/") => {
                let limit = query.number("limit").unwrap_or(50).clamp(1, 500);
                self.list(query.value("request_id"), limit)
            }
            Some(rest) => {
                let id = rest.trim_start_matches('/');
                if id.is_empty() || id.contains('/') {
                    return not_found("no such dev route");
                }
                self.show(id, Format::parse(query.value("format").unwrap_or("json")))
            }
            None => not_found("no such dev route"),
        }
    }

    /// One request against the dev router: it answers `404` for a production site and `401`
    /// unless the caller presented the development token, and only then dispatches. This is the
    /// one routable entry point. `path` is the request path (`/dev/traces` or
    /// `/dev/traces/{id}`), `query` the raw query string without `?`.
    #[must_use]
    pub fn handle_checked(
        &self,
        policy: &DebugPolicy,
        production: bool,
        presented_token: &str,
        method: &str,
        path: &str,
        query: Option<&str>,
    ) -> ApiResponse {
        if policy
            .check_site(self.collector.site(), production)
            .is_err()
        {
            return not_found("no such dev route");
        }
        if policy.check_token(presented_token).is_err() {
            return ApiResponse::json(
                401,
                &json!({"error": "unauthorized", "message": "a development token is required"}),
            );
        }
        self.handle(method, path, query)
    }

    /// The trace list, newest first. With `request_id`, only that request's trace. Private, like
    /// [`DevApi::handle`].
    #[must_use]
    fn list(&self, request_id: Option<&str>, limit: usize) -> ApiResponse {
        let summaries: Vec<TraceSummary> = match request_id {
            Some(id) => self
                .collector
                .by_request(id)
                .map(|trace| vec![trace.summary()])
                .unwrap_or_default(),
            None => self
                .collector
                .summaries()
                .into_iter()
                .take(limit.clamp(1, 500))
                .collect(),
        };
        let traces: Vec<Value> = summaries.iter().map(summary_json).collect();
        ApiResponse::json(
            200,
            &json!({
                "site": self.collector.site().as_str(),
                "traces": traces,
            }),
        )
    }

    /// One trace: JSON, or the text tree plus the layer summary. Private, like [`DevApi::handle`].
    #[must_use]
    fn show(&self, trace_id: &str, format: Format) -> ApiResponse {
        let Some(trace) = self.collector.trace(trace_id) else {
            return not_found("no trace with that id in this site's ring");
        };
        match format {
            Format::Json => ApiResponse::json(200, &trace.to_json()),
            Format::Text => ApiResponse::text(200, render_trace_text(&trace)),
        }
    }

    /// The text the CLI prints for `spot trace <request-id>`; an explanation when the request is
    /// not in the ring.
    #[must_use]
    pub fn render_request(&self, request_id: &str) -> String {
        match self.collector.by_request(request_id) {
            Some(trace) => render_trace_text(&trace),
            None => format!(
                "no trace for request {request_id} in site {}\n",
                self.collector.site().as_str()
            ),
        }
    }

    /// The list text the CLI prints for `spot trace` without a request id.
    #[must_use]
    pub fn render_list(&self) -> String {
        render_summaries(&self.collector.summaries())
    }
}

fn render_trace_text(trace: &Trace) -> String {
    let mut text = render_tree(trace);
    text.push('\n');
    text.push_str(&render_layers(trace));
    text
}

fn summary_json(summary: &TraceSummary) -> Value {
    json!({
        "traceId": summary.trace_id.to_string(),
        "root": summary.root,
        "layer": summary.layer.as_str(),
        "spanCount": summary.span_count,
        "durationMicros": micros(summary.duration),
        "duration": format_duration(summary.duration),
        "errors": summary.error_count,
        "requestId": summary.request_id,
    })
}

fn not_found(why: &str) -> ApiResponse {
    ApiResponse::json(404, &json!({"error": "not_found", "message": why}))
}

/// A minimal query-string reader: `key=value` pairs, percent-decoding `%20` and `+`.
struct Query(Vec<(String, String)>);

impl Query {
    fn parse(raw: &str) -> Self {
        let pairs = raw
            .split('&')
            .filter(|pair| !pair.is_empty())
            .map(|pair| {
                let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
                (decode(key), decode(value))
            })
            .collect();
        Self(pairs)
    }

    fn value(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }

    fn number(&self, key: &str) -> Option<usize> {
        self.value(key)?.parse().ok()
    }
}

fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'+' => out.push(b' '),
            b'%' if at + 2 < bytes.len() => {
                // `get` (not a slice) so a multi-byte character after the `%` cannot panic.
                let decoded = text
                    .get(at + 1..at + 3)
                    .and_then(|hex| u8::from_str_radix(hex, 16).ok());
                if let Some(value) = decoded {
                    out.push(value);
                    at += 3;
                    continue;
                }
                out.push(bytes[at]);
            }
            byte => out.push(byte),
        }
        at += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{ApiResponse, DevApi, Format};
    use crate::trace::collector::TraceCollector;
    use crate::trace::span::Layer;

    fn api() -> DevApi {
        let collector = TraceCollector::new(spot_core::SiteId::new("acme.example").expect("id"), 8);
        let root = collector.root(
            Layer::Http,
            "POST /api/v1/orders",
            [
                ("request_id".into(), "req-1".into()),
                (
                    "authorization".into(),
                    "Bearer spk_live_secret_value".into(),
                ),
                ("route".into(), "/api/v1/orders".into()),
            ],
            Duration::from_millis(1),
        );
        let context = root.context().expect("sampled");
        collector
            .child(
                &context,
                Layer::Storage,
                "storage.insert",
                [],
                Duration::from_millis(2),
            )
            .end(Duration::from_millis(3));
        root.end(Duration::from_millis(4));
        DevApi::new(collector)
    }

    fn body(response: &ApiResponse) -> serde_json::Value {
        serde_json::from_str(&response.body).expect("json body")
    }

    #[test]
    fn the_list_and_the_trace_are_json_with_integer_times() {
        let api = api();
        let list = api.handle("GET", "/dev/traces", None);
        assert_eq!(list.status, 200);
        let json = body(&list);
        assert_eq!(json["site"], "acme.example");
        assert_eq!(json["traces"][0]["requestId"], "req-1");
        assert!(json["traces"][0]["durationMicros"].is_u64());

        let id = json["traces"][0]["traceId"]
            .as_str()
            .expect("id")
            .to_owned();
        let show = api.handle("GET", &format!("/dev/traces/{id}"), Some("format=text"));
        assert_eq!(show.status, 200);
        assert!(show.body.contains("POST /api/v1/orders [http]"));
        assert!(show.body.contains("storage.insert [storage]"));
        assert!(!show.body.contains("spk_live_secret_value"));
    }

    #[test]
    fn query_by_request_id_and_unknown_ids() {
        let api = api();
        let found = api.handle("GET", "/dev/traces", Some("request_id=req-1"));
        assert_eq!(body(&found)["traces"].as_array().expect("array").len(), 1);
        let missing = api.handle("GET", "/dev/traces", Some("request_id=nope"));
        assert_eq!(body(&missing)["traces"].as_array().expect("array").len(), 0);
        assert_eq!(api.handle("GET", "/dev/traces/abc", None).status, 404);
        assert_eq!(api.handle("POST", "/dev/traces", None).status, 405);
        assert_eq!(api.handle("GET", "/dev/other", None).status, 404);
        assert_eq!(
            api.show("0000000000000000000000000000ffff", Format::Json)
                .status,
            404
        );
    }

    #[test]
    fn the_cli_text_names_the_request() {
        let api = api();
        let text = api.render_request("req-1");
        assert!(text.contains("request=req-1"));
        assert!(text.contains("storage"));
        assert!(api.render_request("nope").contains("no trace for request"));
    }
}
