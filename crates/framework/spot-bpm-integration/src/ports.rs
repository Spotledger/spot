//! The ports a handler calls: HTTP through the egress policy, mail through the mail transport,
//! outgoing webhooks through the webhooks crate, WASM connectors through the connector port.
//!
//! A handler never opens a socket itself. It builds a request and hands it to a port; the
//! production ports below are the only implementations that touch the network, and each one goes
//! through the egress guard first. The fakes at the bottom of this module let a test script the
//! answer and inspect what the handler sent, without any I/O.
//!
//! [`ConnectorPort`] is the seam WP-INT-05 implements: `call(connector, operation, input,
//! credential_ref)`. The credential is a reference; the host resolves it, so a guest connector
//! never receives the value.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use http::HeaderMap;
use serde_json::Value as Json;
use spot_egress::{Authorized, EgressError, Guard, Purpose, Sender};
use spot_mail::{
    MailError, MailTransport, OutgoingMessage, SendReceipt, SmtpConfig, SmtpTransport,
};
use url::Url;

use crate::credential::CredentialRef;
use crate::error::{IntegrationError, egress_denied};

/// One outbound HTTP request, as a handler builds it.
#[derive(Debug, Clone)]
pub struct HttpCall {
    /// The method.
    pub method: spot_egress::Method,
    /// The absolute URL.
    pub url: String,
    /// The request headers.
    pub headers: HeaderMap,
    /// The request body, when there is one.
    pub body: Option<Vec<u8>>,
}

/// One HTTP response, as a handler reads it.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    /// The status code.
    pub status: u16,
    /// The response headers.
    pub headers: HeaderMap,
    /// The response body.
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// The body as text, lossily (a body need not be UTF-8).
    #[must_use]
    pub fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// The body parsed as JSON, when it is JSON.
    #[must_use]
    pub fn body_json(&self) -> Option<Json> {
        serde_json::from_slice(&self.body).ok()
    }
}

/// Sends one outbound HTTP request, through the egress policy.
#[async_trait]
pub trait EgressPort: Send + Sync {
    /// Sends `call` for `tenant`.
    ///
    /// # Errors
    ///
    /// [`IntegrationError::EgressDenied`] when the policy refuses the destination, or
    /// [`IntegrationError::TransportFailed`] for a transport failure.
    async fn send(&self, tenant: &str, call: &HttpCall) -> Result<HttpResponse, IntegrationError>;
}

/// The production [`EgressPort`]: the guard authorizes the URL against the tenant's policy, then
/// the sender writes to the pinned addresses only.
pub struct SpotEgressPort {
    guard: Arc<Guard>,
    sender: Arc<dyn Sender>,
    purpose: Purpose,
    actor: Option<String>,
    max_body: u64,
}

impl SpotEgressPort {
    /// An egress port over `guard` and `sender`; responses are read up to `max_body` bytes.
    /// `purpose` is recorded in the policy check and any denial.
    #[must_use]
    pub fn new(
        guard: Arc<Guard>,
        sender: Arc<dyn Sender>,
        purpose: Purpose,
        actor: Option<String>,
        max_body: u64,
    ) -> Self {
        Self {
            guard,
            sender,
            purpose,
            actor,
            max_body,
        }
    }
}

#[async_trait]
impl EgressPort for SpotEgressPort {
    async fn send(&self, tenant: &str, call: &HttpCall) -> Result<HttpResponse, IntegrationError> {
        let url = Url::parse(&call.url).map_err(|e| IntegrationError::UrlInvalid {
            detail: e.to_string(),
        })?;
        let request = spot_egress::Request {
            url: url.clone(),
            purpose: self.purpose,
            tenant: tenant.to_owned(),
            actor: self.actor.clone(),
        };
        let authorized = self.guard.authorize(&request).await.map_err(from_egress)?;
        let body = call.body.clone().map(spot_egress::Bytes::from);
        let http_request =
            spot_egress::HttpRequest::new(call.method, url, call.headers.clone(), body);
        let response = self
            .sender
            .send_request(&http_request, &authorized, self.max_body)
            .await
            .map_err(from_egress)?;
        Ok(HttpResponse {
            status: response.status.as_u16(),
            headers: response.headers,
            body: response.body,
        })
    }
}

/// Sends one outgoing message through the mail transport.
#[async_trait]
pub trait MailPort: Send + Sync {
    /// Sends `message`.
    ///
    /// # Errors
    ///
    /// [`IntegrationError::MailTransient`] when the transport says the send may succeed later, or
    /// [`IntegrationError::MailPermanent`] when it will not.
    async fn send(&self, message: &OutgoingMessage) -> Result<SendReceipt, IntegrationError>;
}

/// Builds the mail transport for exactly the addresses the egress guard authorised.
///
/// This is the seam that keeps a mail send from resolving the host a second time. A factory is
/// handed the pinned socket addresses and nothing else it could dial by name: the host name
/// serves only as the TLS server name and belongs to the factory's own configuration.
pub trait PinnedTransportFactory: Send + Sync {
    /// A transport that dials `pinned`, in order, and never resolves a host name.
    ///
    /// # Errors
    ///
    /// A [`MailError`] when the transport cannot be built (a bad configuration).
    fn build(&self, pinned: Vec<SocketAddr>) -> Result<Arc<dyn MailTransport>, MailError>;
}

/// The production [`PinnedTransportFactory`]: a [`spot_mail::SmtpTransport`] over `template`
/// with `pinned` set to the authorised addresses. Whatever `pinned` the template carried is
/// replaced.
pub struct SmtpPinnedFactory {
    template: SmtpConfig,
}

impl SmtpPinnedFactory {
    /// A factory for the SMTP server `template` describes (host for the TLS name, port,
    /// security, credentials, timeouts).
    #[must_use]
    pub fn new(template: SmtpConfig) -> Self {
        Self { template }
    }
}

impl PinnedTransportFactory for SmtpPinnedFactory {
    fn build(&self, pinned: Vec<SocketAddr>) -> Result<Arc<dyn MailTransport>, MailError> {
        let mut config = self.template.clone();
        config.pinned = Some(pinned);
        Ok(Arc::new(SmtpTransport::new(config)?))
    }
}

/// A test [`PinnedTransportFactory`]: it records every address list it was asked to build for
/// and hands out one shared transport (a `spot_mail::CaptureTransport`, say), so a test can read
/// both what the factory received and what was sent.
pub struct RecordingFactory {
    transport: Arc<dyn MailTransport>,
    built: Mutex<Vec<Vec<SocketAddr>>>,
}

impl RecordingFactory {
    /// A factory that returns `transport` for every build.
    #[must_use]
    pub fn new(transport: Arc<dyn MailTransport>) -> Self {
        Self {
            transport,
            built: Mutex::new(Vec::new()),
        }
    }

    /// The address lists the factory was asked to build for, in order.
    #[must_use]
    pub fn built(&self) -> Vec<Vec<SocketAddr>> {
        self.built.lock().map(|b| b.clone()).unwrap_or_default()
    }
}

impl PinnedTransportFactory for RecordingFactory {
    fn build(&self, pinned: Vec<SocketAddr>) -> Result<Arc<dyn MailTransport>, MailError> {
        if let Ok(mut built) = self.built.lock() {
            built.push(pinned);
        }
        Ok(Arc::clone(&self.transport))
    }
}

/// The egress code reported when the guard authorised a destination but returned no address to
/// dial. Sending would need a lookup by name, which is what this port never does.
const PINNED_REQUIRED: &str = "egress.pinned_required";

/// The production [`MailPort`]: the egress guard authorizes the SMTP host and port first (the
/// `Purpose::Mail` check the egress crate exists for), then the transport, built for exactly the
/// authorised addresses, sends.
///
/// The port never holds a transport that resolves names. Each send takes the addresses the guard
/// vetted ([`spot_egress::Authorized::socket_addrs`]) and asks the [`PinnedTransportFactory`]
/// for a transport that dials those and only those, so a host that rebinds between the check and
/// the connect cannot reach another address with the tenant's credentials. An authorisation with
/// no addresses is refused and nothing is sent. Transports are cached by the sorted address
/// set; a different DNS answer builds a new one and drops the old one with its idle connections.
pub struct EgressMailPort {
    guard: Arc<Guard>,
    factory: Arc<dyn PinnedTransportFactory>,
    host: String,
    port: u16,
    tenant: String,
    cached: Mutex<Option<CachedTransport>>,
}

/// The transport built for one authorised address set.
struct CachedTransport {
    pinned: Vec<SocketAddr>,
    transport: Arc<dyn MailTransport>,
}

impl EgressMailPort {
    /// A mail port over `guard` and `factory` for the SMTP endpoint `host`:`port`.
    #[must_use]
    pub fn new(
        guard: Arc<Guard>,
        factory: Arc<dyn PinnedTransportFactory>,
        host: impl Into<String>,
        port: u16,
        tenant: impl Into<String>,
    ) -> Self {
        Self {
            guard,
            factory,
            host: host.into(),
            port,
            tenant: tenant.into(),
            cached: Mutex::new(None),
        }
    }

    /// The transport for `pinned` (sorted, deduplicated): the cached one for the same set,
    /// otherwise a new one from the factory.
    fn transport_for(
        &self,
        mut pinned: Vec<SocketAddr>,
    ) -> Result<Arc<dyn MailTransport>, IntegrationError> {
        pinned.sort();
        pinned.dedup();
        let mut cached = self
            .cached
            .lock()
            .map_err(|_| IntegrationError::TransportFailed {
                detail: String::from("the mail transport cache is unavailable"),
            })?;
        if let Some(held) = cached.as_ref()
            && held.pinned == pinned
        {
            return Ok(Arc::clone(&held.transport));
        }
        let transport = self.factory.build(pinned.clone()).map_err(from_mail)?;
        *cached = Some(CachedTransport {
            pinned,
            transport: Arc::clone(&transport),
        });
        Ok(transport)
    }
}

#[async_trait]
impl MailPort for EgressMailPort {
    async fn send(&self, message: &OutgoingMessage) -> Result<SendReceipt, IntegrationError> {
        let authorized = self
            .guard
            .authorize_addr(&self.host, self.port, Purpose::Mail, &self.tenant, None)
            .await
            .map_err(from_egress)?;
        self.send_authorized(&authorized, message).await
    }
}

impl EgressMailPort {
    /// Sends `message` through a transport pinned to what `authorized` names, when the caller
    /// already holds the guard's answer. An answer with no addresses is refused with
    /// `bpm_integration.egress_denied` (`egress.pinned_required`) and nothing is sent.
    ///
    /// # Errors
    ///
    /// [`IntegrationError::EgressDenied`] for an empty address list, otherwise as
    /// [`MailPort::send`].
    pub async fn send_authorized(
        &self,
        authorized: &Authorized,
        message: &OutgoingMessage,
    ) -> Result<SendReceipt, IntegrationError> {
        let pinned = authorized.socket_addrs().to_vec();
        if pinned.is_empty() {
            return Err(egress_denied(PINNED_REQUIRED));
        }
        let transport = self.transport_for(pinned)?;
        transport.send(message).await.map_err(from_mail)
    }
}

/// Posts one signed outgoing webhook.
#[async_trait]
pub trait WebhookPort: Send + Sync {
    /// Posts `body` to `url` with `headers`.
    ///
    /// # Errors
    ///
    /// [`IntegrationError::EgressDenied`] or [`IntegrationError::TransportFailed`].
    async fn post(
        &self,
        url: &str,
        headers: HeaderMap,
        body: Vec<u8>,
    ) -> Result<HttpResponse, IntegrationError>;
}

/// The production [`WebhookPort`]: it reuses `spot-webhooks`' delivery egress, which is the egress
/// connector only.
pub struct SpotWebhookPort {
    egress: Arc<dyn spot_webhooks::delivery::Egress>,
    tenant: String,
}

impl SpotWebhookPort {
    /// A webhook port over the webhooks crate's egress for `tenant`.
    #[must_use]
    pub fn new(
        egress: Arc<dyn spot_webhooks::delivery::Egress>,
        tenant: impl Into<String>,
    ) -> Self {
        Self {
            egress,
            tenant: tenant.into(),
        }
    }
}

#[async_trait]
impl WebhookPort for SpotWebhookPort {
    async fn post(
        &self,
        url: &str,
        headers: HeaderMap,
        body: Vec<u8>,
    ) -> Result<HttpResponse, IntegrationError> {
        let url = Url::parse(url).map_err(|e| IntegrationError::UrlInvalid {
            detail: e.to_string(),
        })?;
        let response = self
            .egress
            .post(&self.tenant, &url, headers, body)
            .await
            .map_err(from_egress)?;
        Ok(HttpResponse {
            status: response.status.as_u16(),
            headers: response.headers,
            body: response.body,
        })
    }
}

/// Invokes one operation of a WASM connector. WP-INT-05 implements this over `spot-wasm`.
///
/// The connector runs sandboxed: it never receives a credential value, only the reference the
/// host resolves for the declared auth scheme.
#[async_trait]
pub trait ConnectorPort: Send + Sync {
    /// Calls `operation` of `connector` with `input`, resolving `credential_ref` when given.
    ///
    /// # Errors
    ///
    /// [`IntegrationError::ConnectorFailed`] when the connector reports an error, or
    /// [`IntegrationError::CredentialMissing`] / [`IntegrationError::SecretResolveFailed`] when
    /// the referenced credential cannot be resolved.
    async fn call(
        &self,
        connector: &str,
        operation: &str,
        input: &Json,
        credential_ref: Option<&CredentialRef>,
    ) -> Result<Json, IntegrationError>;
}

/// The egress error's node-level mapping: a policy refusal is permanent, a resolution failure is
/// a transport failure that may resolve on retry.
#[must_use]
pub fn from_egress(error: EgressError) -> IntegrationError {
    let code = error.code();
    match error {
        EgressError::ResolutionFailed => IntegrationError::TransportFailed {
            detail: code.to_owned(),
        },
        EgressError::PolicyDenied
        | EgressError::AddressDenied { .. }
        | EgressError::PortDenied { .. }
        | EgressError::RedirectDenied
        | EgressError::ResolutionMixed
        | EgressError::TooManyRedirects
        | EgressError::ResponseTooLarge
        | EgressError::RequestTooLarge { .. }
        | EgressError::InvalidUrl
        | EgressError::InvalidConfig(_) => egress_denied(code),
    }
}

/// The mail error's node-level mapping: retryable is transient, everything else is permanent.
#[must_use]
pub fn from_mail(error: MailError) -> IntegrationError {
    if error.is_retryable() {
        IntegrationError::MailTransient {
            detail: error.to_string(),
        }
    } else {
        IntegrationError::MailPermanent {
            detail: error.to_string(),
        }
    }
}

/// A scripted [`EgressPort`] for tests: it answers with the queued responses, in order, and
/// records every call.
#[derive(Default)]
pub struct FakeEgress {
    responses: Mutex<VecDeque<Result<HttpResponse, IntegrationError>>>,
    calls: Mutex<Vec<HttpCall>>,
}

impl FakeEgress {
    /// A fake with no queued responses.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Queues a response.
    pub fn push_response(&self, response: HttpResponse) -> &Self {
        self.push(Ok(response));
        self
    }

    /// Queues an error.
    pub fn push_error(&self, error: IntegrationError) -> &Self {
        self.push(Err(error));
        self
    }

    fn push(&self, answer: Result<HttpResponse, IntegrationError>) {
        if let Ok(mut queue) = self.responses.lock() {
            queue.push_back(answer);
        }
    }

    /// The calls the fake received, in order.
    #[must_use]
    pub fn calls(&self) -> Vec<HttpCall> {
        self.calls
            .lock()
            .map(|calls| calls.clone())
            .unwrap_or_default()
    }
}

#[async_trait]
impl EgressPort for FakeEgress {
    async fn send(&self, _tenant: &str, call: &HttpCall) -> Result<HttpResponse, IntegrationError> {
        if let Ok(mut calls) = self.calls.lock() {
            calls.push(call.clone());
        }
        self.responses
            .lock()
            .map(|mut queue| queue.pop_front())
            .unwrap_or(None)
            .unwrap_or_else(|| {
                Err(IntegrationError::TransportFailed {
                    detail: String::from("the fake egress has no queued response"),
                })
            })
    }
}

/// A scripted [`MailPort`] for tests.
#[derive(Default)]
pub struct FakeMail {
    responses: Mutex<VecDeque<Result<SendReceipt, IntegrationError>>>,
    sent: Mutex<Vec<OutgoingMessage>>,
}

impl FakeMail {
    /// A fake with no queued responses.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Queues a receipt.
    pub fn push_receipt(&self, receipt: SendReceipt) -> &Self {
        if let Ok(mut queue) = self.responses.lock() {
            queue.push_back(Ok(receipt));
        }
        self
    }

    /// Queues an error.
    pub fn push_error(&self, error: IntegrationError) -> &Self {
        if let Ok(mut queue) = self.responses.lock() {
            queue.push_back(Err(error));
        }
        self
    }

    /// The messages the fake received, in order.
    #[must_use]
    pub fn sent(&self) -> Vec<OutgoingMessage> {
        self.sent.lock().map(|s| s.clone()).unwrap_or_default()
    }
}

#[async_trait]
impl MailPort for FakeMail {
    async fn send(&self, message: &OutgoingMessage) -> Result<SendReceipt, IntegrationError> {
        if let Ok(mut sent) = self.sent.lock() {
            sent.push(message.clone());
        }
        self.responses
            .lock()
            .map(|mut queue| queue.pop_front())
            .unwrap_or(None)
            .unwrap_or_else(|| {
                Err(IntegrationError::MailPermanent {
                    detail: String::from("the fake mail port has no queued receipt"),
                })
            })
    }
}

/// One webhook post a [`FakeWebhook`] received.
#[derive(Debug, Clone)]
pub struct WebhookPost {
    /// The URL.
    pub url: String,
    /// The headers, including the signature.
    pub headers: HeaderMap,
    /// The body.
    pub body: Vec<u8>,
}

/// A scripted [`WebhookPort`] for tests.
#[derive(Default)]
pub struct FakeWebhook {
    responses: Mutex<VecDeque<Result<HttpResponse, IntegrationError>>>,
    posts: Mutex<Vec<WebhookPost>>,
}

impl FakeWebhook {
    /// A fake with no queued responses.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Queues a response.
    pub fn push_response(&self, response: HttpResponse) -> &Self {
        if let Ok(mut queue) = self.responses.lock() {
            queue.push_back(Ok(response));
        }
        self
    }

    /// Queues an error.
    pub fn push_error(&self, error: IntegrationError) -> &Self {
        if let Ok(mut queue) = self.responses.lock() {
            queue.push_back(Err(error));
        }
        self
    }

    /// The posts the fake received, in order.
    #[must_use]
    pub fn posts(&self) -> Vec<WebhookPost> {
        self.posts.lock().map(|p| p.clone()).unwrap_or_default()
    }
}

#[async_trait]
impl WebhookPort for FakeWebhook {
    async fn post(
        &self,
        url: &str,
        headers: HeaderMap,
        body: Vec<u8>,
    ) -> Result<HttpResponse, IntegrationError> {
        if let Ok(mut posts) = self.posts.lock() {
            posts.push(WebhookPost {
                url: url.to_owned(),
                headers: headers.clone(),
                body,
            });
        }
        self.responses
            .lock()
            .map(|mut queue| queue.pop_front())
            .unwrap_or(None)
            .unwrap_or_else(|| {
                Err(IntegrationError::WebhookFailed {
                    detail: String::from("the fake webhook port has no queued response"),
                })
            })
    }
}

/// One connector call a [`FakeConnector`] received.
#[derive(Debug, Clone)]
pub struct ConnectorCall {
    /// The connector's name.
    pub connector: String,
    /// The operation.
    pub operation: String,
    /// The input.
    pub input: Json,
    /// The credential reference, when the node named one.
    pub credential_ref: Option<CredentialRef>,
}

/// A scripted [`ConnectorPort`] for tests.
#[derive(Default)]
pub struct FakeConnector {
    responses: Mutex<VecDeque<Result<Json, IntegrationError>>>,
    calls: Mutex<Vec<ConnectorCall>>,
}

impl FakeConnector {
    /// A fake with no queued responses.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Queues a result.
    pub fn push_output(&self, output: Json) -> &Self {
        if let Ok(mut queue) = self.responses.lock() {
            queue.push_back(Ok(output));
        }
        self
    }

    /// Queues an error.
    pub fn push_error(&self, error: IntegrationError) -> &Self {
        if let Ok(mut queue) = self.responses.lock() {
            queue.push_back(Err(error));
        }
        self
    }

    /// The calls the fake received, in order.
    #[must_use]
    pub fn calls(&self) -> Vec<ConnectorCall> {
        self.calls.lock().map(|c| c.clone()).unwrap_or_default()
    }
}

#[async_trait]
impl ConnectorPort for FakeConnector {
    async fn call(
        &self,
        connector: &str,
        operation: &str,
        input: &Json,
        credential_ref: Option<&CredentialRef>,
    ) -> Result<Json, IntegrationError> {
        if let Ok(mut calls) = self.calls.lock() {
            calls.push(ConnectorCall {
                connector: connector.to_owned(),
                operation: operation.to_owned(),
                input: input.clone(),
                credential_ref: credential_ref.cloned(),
            });
        }
        self.responses
            .lock()
            .map(|mut queue| queue.pop_front())
            .unwrap_or(None)
            .unwrap_or_else(|| {
                Err(IntegrationError::ConnectorFailed {
                    connector: connector.to_owned(),
                    detail: String::from("the fake connector has no queued response"),
                })
            })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_egress_policy_denial_is_permanent() {
        let error = from_egress(EgressError::PolicyDenied);
        assert_eq!(error.code(), "bpm_integration.egress_denied");
        assert_eq!(error.retry_class(), crate::error::RetryClass::Permanent);
    }

    #[test]
    fn a_resolution_failure_is_a_transport_failure() {
        let error = from_egress(EgressError::ResolutionFailed);
        assert_eq!(error.code(), "bpm_integration.transport_failed");
        assert_eq!(error.retry_class(), crate::error::RetryClass::Transient);
    }

    #[tokio::test]
    async fn the_fake_egress_records_calls_and_answers() {
        let fake = FakeEgress::new();
        fake.push_response(HttpResponse {
            status: 200,
            headers: HeaderMap::new(),
            body: br#"{"ok":true}"#.to_vec(),
        });
        let call = HttpCall {
            method: spot_egress::Method::Get,
            url: "https://api.test/x".to_owned(),
            headers: HeaderMap::new(),
            body: None,
        };
        let response = fake.send("acme", &call).await.unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body_json(), Some(json!({ "ok": true })));
        assert_eq!(fake.calls().len(), 1);
    }

    #[tokio::test]
    async fn the_fake_connector_hands_back_the_reference_not_a_value() {
        let fake = FakeConnector::new();
        fake.push_output(json!({ "ack": "1" }));
        let reference = CredentialRef::new("as2_key");
        let output = fake
            .call("as2", "send", &json!({ "doc": 1 }), Some(&reference))
            .await
            .unwrap();
        assert_eq!(output, json!({ "ack": "1" }));
        let calls = fake.calls();
        assert_eq!(calls[0].connector, "as2");
        assert_eq!(calls[0].credential_ref, Some(reference));
    }
}
