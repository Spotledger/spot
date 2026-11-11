//! What the host provides to a connector: the outbound HTTP service (through the egress
//! policy), the log sink and the key-value store. The credential vault is consulted here too,
//! so an injected credential never reaches the guest.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use spot_egress::{Authorized, EgressError, Guard, Method, Purpose, Request, Sender};
use spot_wasm::LogLevel;

use crate::config::ConnectorConfig;
use crate::error::{ConnectorError, Result};
use crate::model::{ConnectorId, HttpRequest, HttpResponse};
use crate::monitor::ConnectorMonitor;
use crate::vault::{CredentialRef, CredentialVault};

/// Who and when a call runs for. Time is an argument, never read from a clock.
#[derive(Clone, Debug)]
pub struct HttpContext {
    /// The tenant the call runs for.
    pub tenant: String,
    /// The actor (user or service), when there is one.
    pub actor: Option<String>,
    /// The invocation's time, in microseconds since the Unix epoch.
    pub now_micros: i64,
}

/// The outbound transport. Production is [`EgressHttp`]; tests record exchanges.
#[async_trait]
pub trait HttpService: Send + Sync {
    /// Sends `request` for `ctx`, returning the response.
    async fn send(&self, request: HttpRequest, ctx: &HttpContext) -> Result<HttpResponse>;
}

/// The production [`HttpService`]: authorize through the egress guard, then send to the pinned
/// addresses only. A denied destination is an error before any connection is made.
pub struct EgressHttp {
    guard: Arc<Guard>,
    sender: Arc<dyn Sender>,
    max_body: u64,
}

impl EgressHttp {
    /// An egress service over `guard` and `sender`; responses are read up to `max_body` bytes.
    pub fn new(guard: Arc<Guard>, sender: Arc<dyn Sender>, max_body: u64) -> Self {
        Self {
            guard,
            sender,
            max_body,
        }
    }
}

#[async_trait]
impl HttpService for EgressHttp {
    async fn send(&self, request: HttpRequest, ctx: &HttpContext) -> Result<HttpResponse> {
        let url: url::Url = request
            .url
            .parse()
            .map_err(|_| ConnectorError::BadRequest {
                reason: "the request URL is not a URL".into(),
            })?;
        let method = parse_method(&request.method)?;
        let mut headers = http::HeaderMap::new();
        for (name, value) in &request.headers {
            let name = http::HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                ConnectorError::BadRequest {
                    reason: "an outbound header name is invalid".into(),
                }
            })?;
            let value =
                http::HeaderValue::from_str(value).map_err(|_| ConnectorError::BadRequest {
                    reason: "an outbound header value is invalid".into(),
                })?;
            headers.insert(name, value);
        }
        let authorized: Authorized = self
            .guard
            .authorize(&Request {
                url: url.clone(),
                purpose: Purpose::Connector,
                tenant: ctx.tenant.clone(),
                actor: ctx.actor.clone(),
            })
            .await
            .map_err(|e| map_egress(&e, &request.url))?;
        let body = request.body.map(spot_egress::Bytes::from);
        let outbound = spot_egress::HttpRequest::new(method, url, headers, body);
        let response = self
            .sender
            .send_request(&outbound, &authorized, self.max_body)
            .await
            .map_err(|e| map_egress(&e, &request.url))?;
        let headers = response
            .headers
            .iter()
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|v| (name.as_str().to_owned(), v.to_owned()))
            })
            .collect();
        Ok(HttpResponse {
            status: response.status.as_u16(),
            headers,
            body: String::from_utf8_lossy(&response.body).into_owned(),
        })
    }
}

fn parse_method(method: &str) -> Result<Method> {
    match method.to_ascii_uppercase().as_str() {
        "GET" => Ok(Method::Get),
        "POST" => Ok(Method::Post),
        "PUT" => Ok(Method::Put),
        "PATCH" => Ok(Method::Patch),
        "DELETE" => Ok(Method::Delete),
        other => Err(ConnectorError::BadRequest {
            reason: format!("the method `{other}` is not one of GET, POST, PUT, PATCH, DELETE"),
        }),
    }
}

fn map_egress(error: &EgressError, url: &str) -> ConnectorError {
    let host = url
        .parse::<url::Url>()
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".to_owned());
    match error {
        EgressError::PolicyDenied
        | EgressError::AddressDenied { .. }
        | EgressError::PortDenied { .. }
        | EgressError::RedirectDenied => ConnectorError::EgressDenied { host },
        EgressError::InvalidUrl => ConnectorError::BadRequest {
            reason: "the request URL is invalid".into(),
        },
        EgressError::RequestTooLarge { .. } => ConnectorError::BadRequest {
            reason: "the request body is too large".into(),
        },
        EgressError::ResponseTooLarge => ConnectorError::OutputTooLarge { limit: 0 },
        EgressError::ResolutionFailed
        | EgressError::ResolutionMixed
        | EgressError::TooManyRedirects
        | EgressError::InvalidConfig(_) => ConnectorError::HostFailure {
            reason: "the outbound request could not be completed".into(),
        },
    }
}

/// Where a connector's log lines go. The host decides; the guest never owns the sink.
pub trait LogSink: Send + Sync {
    /// Writes one line.
    fn log(&self, level: LogLevel, message: &str);
}

/// A sink that discards lines.
#[derive(Default)]
pub struct NullLogSink;

impl LogSink for NullLogSink {
    fn log(&self, _level: LogLevel, _message: &str) {}
}

/// A sink that records lines, for tests and the test kit.
#[derive(Default)]
pub struct RecordingLogSink {
    lines: Mutex<Vec<(LogLevel, String)>>,
}

impl RecordingLogSink {
    /// The recorded lines.
    pub fn lines(&self) -> Vec<(LogLevel, String)> {
        self.lines.lock().map(|l| l.clone()).unwrap_or_default()
    }
}

impl LogSink for RecordingLogSink {
    fn log(&self, level: LogLevel, message: &str) {
        if let Ok(mut lines) = self.lines.lock() {
            lines.push((level, message.to_owned()));
        }
    }
}

/// Small per-tenant, per-connector state. The host owns it; the guest only reads and writes through the
/// world's `kv` functions.
pub trait KvStore: Send + Sync {
    /// The value for `key`, or `None`.
    fn get(&self, tenant: &str, connector: &ConnectorId, key: &str) -> Option<String>;
    /// Writes `value` for `key`, refusing when the connector's budget would be exceeded. The
    /// budget and the entries are per `(tenant, connector)`: one tenant never sees, overwrites
    /// or spends the budget of another.
    fn set(
        &self,
        tenant: &str,
        connector: &ConnectorId,
        key: &str,
        value: &str,
        budget: usize,
    ) -> Result<()>;
}

/// An in-memory key-value store, bounded per `(tenant, connector)`.
#[derive(Default)]
pub struct InMemoryKv {
    entries: Mutex<HashMap<KvKey, String>>,
}

/// `(tenant, connector, key)`.
type KvKey = (String, String, String);

impl InMemoryKv {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }
}

impl KvStore for InMemoryKv {
    fn get(&self, tenant: &str, connector: &ConnectorId, key: &str) -> Option<String> {
        self.entries.lock().ok().and_then(|m| {
            m.get(&(
                tenant.to_owned(),
                connector.as_str().to_owned(),
                key.to_owned(),
            ))
            .cloned()
        })
    }

    fn set(
        &self,
        tenant: &str,
        connector: &ConnectorId,
        key: &str,
        value: &str,
        budget: usize,
    ) -> Result<()> {
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| ConnectorError::HostFailure {
                reason: "the key-value store is poisoned".into(),
            })?;
        let full_key = (
            tenant.to_owned(),
            connector.as_str().to_owned(),
            key.to_owned(),
        );
        let used: usize = entries
            .iter()
            .filter(|((t, id, _), _)| t == tenant && id == connector.as_str())
            .map(|((_, _, k), v)| k.len() + v.len())
            .sum();
        let previous = entries.get(&full_key).map_or(0, |v| key.len() + v.len());
        let after = used.saturating_sub(previous) + key.len() + value.len();
        if after > budget {
            return Err(ConnectorError::KvLimit { limit: budget });
        }
        entries.insert(full_key, value.to_owned());
        Ok(())
    }
}

/// Everything the host's side of one call needs: the connector's configuration, the vault, the
/// transport, the sinks and the monitor.
pub struct CallServices {
    /// The connector's host-side configuration.
    pub config: Arc<ConnectorConfig>,
    /// The credential vault.
    pub vault: Arc<dyn CredentialVault>,
    /// The outbound transport.
    pub http: Arc<dyn HttpService>,
    /// Where log lines go.
    pub logs: Arc<dyn LogSink>,
    /// The key-value store.
    pub kv: Arc<dyn KvStore>,
    /// The per-connector monitor and breaker.
    pub monitor: Arc<ConnectorMonitor>,
    /// Who and when this call runs for.
    pub ctx: HttpContext,
}

impl CallServices {
    /// Serves `http-request`: parse the guest's JSON request, inject the declared credential
    /// (so the guest never holds it) and send it through the transport, which enforces egress.
    pub async fn http_request(&self, request_json: &str) -> Result<String> {
        let mut request: HttpRequest =
            serde_json::from_str(request_json).map_err(|_| ConnectorError::BadRequest {
                reason: "the request is not JSON of the form {method,url,headers,body}".into(),
            })?;
        if let Some(auth) = &self.config.auth
            && auth.kind.is_injected()
        {
            // The credential goes only to a host the connector declared. Without this a
            // connector could send its token to any host the egress policy allows.
            // An empty list declares no host, so it denies every host (fail closed); the
            // config is also refused at load.
            let host = request
                .url
                .parse::<url::Url>()
                .ok()
                .and_then(|u| u.host_str().map(str::to_ascii_lowercase));
            let declared = host.as_deref().is_some_and(|h| {
                self.config
                    .endpoints
                    .iter()
                    .any(|e| e.eq_ignore_ascii_case(h))
            });
            if !declared {
                return Err(ConnectorError::EgressDenied {
                    host: host.unwrap_or_else(|| "unknown".to_owned()),
                });
            }
            let (name, value) = auth
                .header(self.vault.as_ref(), &self.ctx.tenant, self.ctx.now_micros)
                .await?;
            request
                .headers
                .retain(|(existing, _)| !existing.eq_ignore_ascii_case(&name));
            request.headers.push((name, value));
        }
        let response = self.http.send(request, &self.ctx).await?;
        serde_json::to_string(&response).map_err(|_| ConnectorError::HostFailure {
            reason: "the response could not be serialised".into(),
        })
    }

    /// Serves `secrets-get`: only a reference the connector declared **and** the host does not
    /// inject may be read.
    pub async fn secrets_get(&self, reference: &str) -> Result<String> {
        let Some(reference) = CredentialRef::new(reference) else {
            return Err(ConnectorError::SecretDenied {
                reference: String::new(),
            });
        };
        if !self.config.may_read_secret(&reference) {
            return Err(ConnectorError::SecretDenied {
                reference: reference.to_string(),
            });
        }
        let secret = self
            .vault
            .resolve_for(&self.ctx.tenant, &reference, self.ctx.now_micros)
            .await?;
        Ok(secret.expose().to_owned())
    }

    /// Serves `kv-get`.
    pub fn kv_get(&self, key: &str) -> Option<String> {
        self.kv.get(&self.ctx.tenant, &self.config.id, key)
    }

    /// Serves `kv-set`.
    pub fn kv_set(&self, key: &str, value: &str) -> Result<()> {
        self.kv.set(
            &self.ctx.tenant,
            &self.config.id,
            key,
            value,
            self.config.kv_bytes,
        )
    }
}
