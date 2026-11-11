//! Replays recorded exchanges from files, so provider tests run with no network.
//!
//! A recording is a JSON array of [`RecordedExchange`]s: the request half (matched on method
//! and URL) and the response half — one buffered body, an SSE chunk list, or nothing at all.
//! Provider tests (T2, T3) record an exchange once and replay it here, keeping the recorded
//! `Debug` output redacted like every other header print in this crate.

use serde::{Deserialize, Serialize};

use super::{ByteStream, HttpRequest, HttpResponse, HttpTransport, TransportError};
use crate::provider::BoxFuture;
use crate::stream::IterStream;

/// One recorded exchange: what was asked, and what came back.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordedExchange {
    /// The request as recorded; matched by [`HttpMethod`] and URL. The `timeout` field is
    /// ignored when matching. `Debug` redacts headers and the body.
    pub request: HttpRequest,
    /// The response as recorded.
    pub response: RecordedResponse,
}

/// A recorded response: one buffered body, an SSE chunk list, or nothing at all.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RecordedResponse {
    /// One buffered response.
    Complete {
        /// The status code.
        status: u16,
        /// The response headers.
        #[serde(default)]
        headers: Vec<(String, String)>,
        /// The body.
        body: String,
    },
    /// A chunked (SSE) body: each entry is one chunk, in order.
    Stream {
        /// The status code.
        status: u16,
        /// The response headers.
        #[serde(default)]
        headers: Vec<(String, String)>,
        /// The chunks, in order.
        chunks: Vec<String>,
    },
}

/// The replaying transport: every `send`/`send_stream` finds the first recorded exchange
/// matching the method and URL and answers with what was recorded. Exchanges are reusable, so
/// a provider test can call twice; the same request answered twice is the same recorded bytes.
pub struct MockTransport {
    exchanges: Vec<RecordedExchange>,
}

impl MockTransport {
    /// A transport replaying `exchanges`, in record order.
    pub fn new(exchanges: Vec<RecordedExchange>) -> Self {
        Self { exchanges }
    }

    /// Parses a recording (a JSON array of [`RecordedExchange`]s) from `json`.
    pub fn from_json(json: &str) -> Result<Self, TransportError> {
        let exchanges: Vec<RecordedExchange> = serde_json::from_str(json).map_err(|e| {
            TransportError::Protocol(format!("recording is not an exchange array: {e}"))
        })?;
        Ok(Self { exchanges })
    }

    /// Loads a recording from `path` (UTF-8, a JSON array of exchanges).
    pub fn load_file(path: &str) -> Result<Self, TransportError> {
        let json = std::fs::read_to_string(path).map_err(|e| {
            TransportError::Connection(format!("could not read the recording at '{path}': {e}"))
        })?;
        Self::from_json(&json)
    }

    /// The recorded exchanges, in order.
    pub fn exchanges(&self) -> &[RecordedExchange] {
        &self.exchanges
    }

    /// Finds the first exchange matching `request`'s method and URL.
    fn pick(&self, request: &HttpRequest) -> Result<RecordedResponse, TransportError> {
        self.exchanges
            .iter()
            .find(|exchange| {
                exchange.request.method == request.method && exchange.request.url == request.url
            })
            .map(|exchange| exchange.response.clone())
            .ok_or_else(|| {
                TransportError::Protocol(format!(
                    "no recorded exchange for {} {}",
                    request.method.as_str(),
                    request.url
                ))
            })
    }
}

impl HttpTransport for MockTransport {
    fn send<'a>(
        &'a self,
        request: HttpRequest,
    ) -> BoxFuture<'a, Result<HttpResponse, TransportError>> {
        Box::pin(async move {
            match self.pick(&request)? {
                RecordedResponse::Complete {
                    status,
                    headers,
                    body,
                } => Ok(HttpResponse {
                    status,
                    headers,
                    body,
                }),
                RecordedResponse::Stream {
                    status,
                    headers,
                    chunks,
                } => {
                    // A buffered read over a recorded stream concatenates the chunks.
                    Ok(HttpResponse {
                        status,
                        headers,
                        body: chunks.concat(),
                    })
                }
            }
        })
    }

    fn send_stream<'a>(
        &'a self,
        request: HttpRequest,
    ) -> BoxFuture<'a, Result<ByteStream, TransportError>> {
        Box::pin(async move {
            let chunks = match self.pick(&request)? {
                RecordedResponse::Complete { body, .. } => vec![body],
                RecordedResponse::Stream { chunks, .. } => chunks,
            };
            let items: Vec<Result<Vec<u8>, TransportError>> = chunks
                .into_iter()
                .map(|chunk| Ok(chunk.into_bytes()))
                .collect();
            Ok(Box::pin(IterStream::new(items)) as ByteStream)
        })
    }
}
