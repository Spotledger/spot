//! The connector test kit: a mock host that answers recorded HTTP exchanges, a mock token
//! transport for OAuth2, and the mapping checker. A connector author runs the same world
//! against this kit without a network.
//!
//! Enabled by the `testkit` feature (on by default, so the acceptance tests exercise it).

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;

use crate::error::Result;
use crate::imports::{HttpContext, HttpService};
use crate::model::{HttpRequest, HttpResponse};
use crate::vault::TokenTransport;

pub use crate::mapping::{
    Coercion, MappingReport, MappingRule, MappingSpec, RuleOutcome, check_mapping,
};

/// A recorded exchange: a URL prefix and the response to return for it.
#[derive(Clone, Debug)]
pub struct RecordedExchange {
    /// A URL prefix the request must start with.
    pub url_prefix: String,
    /// The method, or `None` for any.
    pub method: Option<String>,
    /// The response.
    pub response: HttpResponse,
}

/// A mock HTTP transport: it records every request and answers from a list of recorded
/// exchanges (or a default 404).
pub struct MockHttp {
    exchanges: Mutex<Vec<RecordedExchange>>,
    requests: Mutex<Vec<HttpRequest>>,
    default: HttpResponse,
}

impl Default for MockHttp {
    fn default() -> Self {
        Self::new()
    }
}

impl MockHttp {
    /// An empty mock; unmatched requests get a 404 with an empty body.
    pub fn new() -> Self {
        Self {
            exchanges: Mutex::new(Vec::new()),
            requests: Mutex::new(Vec::new()),
            default: HttpResponse {
                status: 404,
                headers: Vec::new(),
                body: "not recorded".into(),
            },
        }
    }

    /// Adds a recorded exchange.
    pub fn record(&self, exchange: RecordedExchange) {
        if let Ok(mut exchanges) = self.exchanges.lock() {
            exchanges.push(exchange);
        }
    }

    /// Records a `GET`-style exchange by URL prefix.
    pub fn record_url(&self, url_prefix: &str, status: u16, body: &str) {
        self.record(RecordedExchange {
            url_prefix: url_prefix.to_owned(),
            method: None,
            response: HttpResponse {
                status,
                headers: Vec::new(),
                body: body.to_owned(),
            },
        });
    }

    /// Every request the mock saw, in order.
    pub fn requests(&self) -> Vec<HttpRequest> {
        self.requests.lock().map(|r| r.clone()).unwrap_or_default()
    }
}

#[async_trait]
impl HttpService for MockHttp {
    async fn send(&self, request: HttpRequest, _ctx: &HttpContext) -> Result<HttpResponse> {
        if let Ok(mut requests) = self.requests.lock() {
            requests.push(request.clone());
        }
        let exchanges = self.exchanges.lock().map(|e| e.clone()).unwrap_or_default();
        for exchange in &exchanges {
            let method_ok = exchange
                .method
                .as_ref()
                .is_none_or(|m| m.eq_ignore_ascii_case(&request.method));
            if method_ok && request.url.starts_with(&exchange.url_prefix) {
                return Ok(exchange.response.clone());
            }
        }
        Ok(self.default.clone())
    }
}

/// A mock token transport for OAuth2: it counts calls and answers one fixed body.
pub struct MockTokenTransport {
    calls: AtomicU64,
    body: String,
}

impl MockTokenTransport {
    /// A transport that always answers `body`.
    pub fn new(body: impl Into<String>) -> Self {
        Self {
            calls: AtomicU64::new(0),
            body: body.into(),
        }
    }

    /// How many times a token was requested.
    pub fn calls(&self) -> u64 {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl TokenTransport for MockTokenTransport {
    async fn post_form(&self, _url: &str, _form: &str, _now_micros: i64) -> Result<String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.body.clone())
    }
}
