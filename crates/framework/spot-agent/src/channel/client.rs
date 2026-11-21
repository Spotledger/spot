//! The channel client of contract `agent-channel` 0.1.0: `poll`, `report`, `heartbeat`,
//! `fetch_update`, plus the enrolment call. Every request after enrolment is signed by
//! the agent key over method, path, body digest, timestamp and nonce.
//!
//! Headers: `X-Spot-Agent-Id`, `X-Spot-Timestamp`, `X-Spot-Nonce` and
//! `X-Spot-Agent-Signature` (base64 of the Ed25519 signature over
//! [`signing_input`]). The contract names only the signature header; the others carry the
//! signed fields (recorded as an open question for the gateway side).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use spot_agent_enroll::keys::KeyStore;

use super::timefmt::{parse_http_date, rfc3339};
use super::transport::{HttpRequest, HttpResponse, Transport};
use crate::enrol::{EnrolGateway, EnrolRequestWire, EnrolResponseWire};
use crate::error::AgentError;

/// The long-poll timeout the agent waits for an answer.
pub const POLL_TIMEOUT: Duration = Duration::from_secs(45);
/// Everything else.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// Largest accepted result output, bytes.
pub const MAX_OUTPUT: usize = 64 * 1024;

/// Why a call did not succeed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChannelError {
    /// `403 agent.revoked`: stop polling and delete the local keys.
    Revoked,
    /// Network, TLS, 429, 5xx, replayed request nonce: retry with backoff.
    Retryable(String),
    /// The gateway refused for a reason retrying will not change (code only).
    Fatal(String),
}

impl std::fmt::Display for ChannelError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Revoked => f.write_str("agent.revoked"),
            Self::Retryable(d) => write!(f, "retryable: {d}"),
            Self::Fatal(c) => write!(f, "fatal: {c}"),
        }
    }
}

/// The bytes the agent signs for one request.
#[must_use]
pub fn signing_input(
    method: &str,
    path: &str,
    body: &[u8],
    timestamp: i64,
    nonce: &str,
) -> Vec<u8> {
    let digest = Sha256::digest(body);
    let mut hex = String::with_capacity(64);
    for b in digest {
        hex.push_str(&format!("{b:02x}"));
    }
    format!("spotledger-agent-request-v1\n{method}\n{path}\n{hex}\n{timestamp}\n{nonce}")
        .into_bytes()
}

/// What the agent reports for one command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResultReport {
    /// The command id.
    pub command_id: String,
    /// Result sequence.
    pub seq: u64,
    /// `succeeded`, `failed`, `refused` or `unsupported` (`indeterminate` is sent as `failed`
    /// with code `agent.indeterminate`: the contract has no fifth outcome).
    pub outcome: String,
    /// Result code.
    pub code: Option<String>,
    /// Redacted output, at most 64 KiB.
    pub output: Option<String>,
}

/// The client.
pub struct Client {
    transport: Arc<dyn Transport>,
    keys: Arc<dyn KeyStore>,
    agent_id: String,
    offset: Mutex<i64>,
    counter: Mutex<u64>,
}

fn error_code(body: &[u8]) -> Option<String> {
    let v: Value = serde_json::from_slice(body).ok()?;
    v.get("error")?.get("code")?.as_str().map(str::to_owned)
}

impl Client {
    /// A client for `agent_id` signing with `keys` over `transport`.
    #[must_use]
    pub fn new(
        transport: Arc<dyn Transport>,
        keys: Arc<dyn KeyStore>,
        agent_id: impl Into<String>,
    ) -> Self {
        Self {
            transport,
            keys,
            agent_id: agent_id.into(),
            offset: Mutex::new(0),
            counter: Mutex::new(0),
        }
    }

    /// The learned gateway clock offset in seconds (gateway minus local), from `Date`.
    #[must_use]
    pub fn clock_skew(&self) -> i64 {
        self.offset.lock().map_or(0, |o| *o)
    }

    fn nonce(&self, now: i64) -> String {
        let mut random = [0u8; 12];
        rand::Rng::fill(&mut rand::rng(), &mut random);
        let n = self.counter.lock().map_or(0, |mut c| {
            *c += 1;
            *c
        });
        let mut out = String::new();
        for b in random {
            out.push_str(&format!("{b:02x}"));
        }
        out.push_str(&format!("{:08x}", (now as u64) ^ n));
        out
    }

    fn signed_headers(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
        now: i64,
    ) -> Vec<(String, String)> {
        let timestamp = now.saturating_add(self.clock_skew());
        let nonce = self.nonce(now);
        let signature = self
            .keys
            .sign(&signing_input(method, path, body, timestamp, &nonce));
        vec![
            ("X-Spot-Agent-Id".to_owned(), self.agent_id.clone()),
            ("X-Spot-Timestamp".to_owned(), timestamp.to_string()),
            ("X-Spot-Nonce".to_owned(), nonce),
            (
                "X-Spot-Agent-Signature".to_owned(),
                STANDARD.encode(signature.to_bytes()),
            ),
        ]
    }

    fn call(
        &self,
        method: &'static str,
        path: &str,
        body: Vec<u8>,
        mut extra: Vec<(String, String)>,
        now: i64,
        timeout: Duration,
    ) -> Result<HttpResponse, ChannelError> {
        let mut headers = self.signed_headers(method, path, &body, now);
        headers.append(&mut extra);
        let response = self
            .transport
            .send(&HttpRequest {
                method,
                path: path.to_owned(),
                headers,
                body,
                timeout,
            })
            .map_err(|e| ChannelError::Retryable(e.code().to_owned()))?;
        if let Some(date) = response.header("date").and_then(parse_http_date)
            && let Ok(mut offset) = self.offset.lock()
        {
            *offset = date.saturating_sub(now);
        }
        match response.status {
            200 => Ok(response),
            403 if error_code(&response.body).as_deref() == Some("agent.revoked") => {
                Err(ChannelError::Revoked)
            }
            429 | 500..=599 => Err(ChannelError::Retryable(format!("http {}", response.status))),
            status => {
                let code =
                    error_code(&response.body).unwrap_or_else(|| format!("agent.http_{status}"));
                if code == "agent.replayed" {
                    Err(ChannelError::Retryable(code))
                } else {
                    Err(ChannelError::Fatal(code))
                }
            }
        }
    }

    /// Long poll: the signed command envelopes the gateway has for this agent.
    ///
    /// # Errors
    /// [`ChannelError`].
    pub fn poll(&self, last_result_seq: u64, now: i64) -> Result<Vec<Value>, ChannelError> {
        let at = rfc3339(now);
        let proof = self
            .keys
            .sign(format!("spotledger-agent-liveness-v1\n{}\n{at}", self.agent_id).as_bytes());
        let body = json!({
            "agent_id": self.agent_id,
            "last_result_seq": last_result_seq,
            "liveness": {"at": at, "proof": STANDARD.encode(proof.to_bytes())},
        });
        let response = self.call(
            "POST",
            "/agent/v1/poll",
            body.to_string().into_bytes(),
            Vec::new(),
            now,
            POLL_TIMEOUT,
        )?;
        let parsed: Value = serde_json::from_slice(&response.body)
            .map_err(|_| ChannelError::Fatal("agent.invalid".to_owned()))?;
        let commands = parsed
            .get("data")
            .and_then(|d| d.get("commands"))
            .and_then(Value::as_array)
            .ok_or_else(|| ChannelError::Fatal("agent.invalid".to_owned()))?;
        Ok(commands.iter().take(20).cloned().collect())
    }

    /// Reports one result (signed, idempotent by command id).
    ///
    /// # Errors
    /// [`ChannelError`].
    pub fn report(&self, report: &ResultReport, now: i64) -> Result<(), ChannelError> {
        let output = report.output.as_deref().map(|o| truncate(o, MAX_OUTPUT));
        let signature = self.keys.sign(&result_signing_input(
            &self.agent_id,
            report,
            output.as_deref(),
        ));
        let mut body = json!({
            "command_id": report.command_id,
            "seq": report.seq,
            "outcome": report.outcome,
            "signature": STANDARD.encode(signature.to_bytes()),
        });
        if let Some(code) = &report.code {
            body["code"] = json!(code);
        }
        if let Some(output) = output {
            body["output"] = json!(output);
        }
        let extra = vec![("Idempotency-Key".to_owned(), report.command_id.clone())];
        self.call(
            "POST",
            "/agent/v1/results",
            body.to_string().into_bytes(),
            extra,
            now,
            CALL_TIMEOUT,
        )
        .map(|_| ())
    }

    /// Heartbeat: counts, sizes and health only.
    ///
    /// # Errors
    /// [`ChannelError`].
    pub fn heartbeat(
        &self,
        series: &[(String, String)],
        state: &str,
        now: i64,
    ) -> Result<(), ChannelError> {
        let series: Vec<Value> = series
            .iter()
            .take(200)
            .map(|(n, v)| json!({"name": n, "value": v}))
            .collect();
        let body = json!({"agent_id": self.agent_id, "at": rfc3339(now), "series": series, "state": state});
        self.call(
            "POST",
            "/agent/v1/heartbeat",
            body.to_string().into_bytes(),
            Vec::new(),
            now,
            CALL_TIMEOUT,
        )
        .map(|_| ())
    }

    /// The signed update manifest for this agent (verification is WP-ADM-03.T3).
    ///
    /// # Errors
    /// [`ChannelError`].
    pub fn fetch_update(&self, channel: Option<&str>, now: i64) -> Result<Value, ChannelError> {
        let path = match channel {
            Some(c)
                if c.len() <= 40
                    && c.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') =>
            {
                format!("/agent/v1/update?channel={c}")
            }
            Some(_) => return Err(ChannelError::Fatal("agent.invalid".to_owned())),
            None => "/agent/v1/update".to_owned(),
        };
        let response = self.call("GET", &path, Vec::new(), Vec::new(), now, CALL_TIMEOUT)?;
        let parsed: Value = serde_json::from_slice(&response.body)
            .map_err(|_| ChannelError::Fatal("agent.invalid".to_owned()))?;
        parsed
            .get("data")
            .cloned()
            .ok_or_else(|| ChannelError::Fatal("agent.invalid".to_owned()))
    }
}

/// The bytes the agent signs for a result.
#[must_use]
pub fn result_signing_input(agent_id: &str, r: &ResultReport, output: Option<&str>) -> Vec<u8> {
    let digest = Sha256::digest(output.unwrap_or("").as_bytes());
    let mut hex = String::new();
    for b in digest {
        hex.push_str(&format!("{b:02x}"));
    }
    format!(
        "spotledger-agent-result-v1\n{agent_id}\n{}\n{}\n{}\n{}\n{hex}",
        r.command_id,
        r.seq,
        r.outcome,
        r.code.as_deref().unwrap_or("")
    )
    .into_bytes()
}

/// Truncates to at most `max` bytes on a character boundary.
#[must_use]
pub fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

/// The enrolment call over a transport (unsigned: the one-time token is the credential).
pub struct HttpsEnrolGateway {
    transport: Arc<dyn Transport>,
}

impl HttpsEnrolGateway {
    /// Wraps `transport`.
    #[must_use]
    pub fn new(transport: Arc<dyn Transport>) -> Self {
        Self { transport }
    }
}

impl EnrolGateway for HttpsEnrolGateway {
    fn enrol(&self, request: &EnrolRequestWire) -> Result<EnrolResponseWire, AgentError> {
        let body = serde_json::to_vec(request)
            .map_err(|_| AgentError::GatewayProtocol("cannot encode the request".to_owned()))?;
        let response = self.transport.send(&HttpRequest {
            method: "POST",
            path: "/agent/v1/enrol".to_owned(),
            headers: Vec::new(),
            body,
            timeout: CALL_TIMEOUT,
        })?;
        match response.status {
            200 => {
                let parsed: Value = serde_json::from_slice(&response.body).map_err(|_| {
                    AgentError::GatewayProtocol("the enrolment answer is not JSON".to_owned())
                })?;
                serde_json::from_value(parsed.get("data").cloned().unwrap_or(Value::Null)).map_err(
                    |_| AgentError::GatewayProtocol("the enrolment answer is invalid".to_owned()),
                )
            }
            401 => Err(AgentError::TokenInvalid),
            s => Err(AgentError::GatewayProtocol(format!(
                "the gateway answered http {s}"
            ))),
        }
    }
}
