//! How bytes reach the peer: the [`PeerChannel`] seam and its two implementations, the
//! connector (WP-INT-05) and the signed webhook through the egress policy (`spot-webhooks`).
//!
//! A channel only moves one body and reports what happened; it never decides a delivery
//! state. A failure here is always retryable: the message stays where it is and the outbox
//! hands it over again until it is answered or its retention ends.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use http::{HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;
use spot_connectors::{ConnectorCall, ConnectorError, ConnectorPort, CredentialVault};
use spot_egress::EgressError;
use spot_link_exchange::MessageId;
use spot_tenant_link::Topic;
use spot_webhooks::SigningKey;
use spot_webhooks::signing::build_signature_header;
use spot_webhooks::{Egress, USER_AGENT};

use crate::error::codes;
use crate::peer::{ExternalPeer, Outbound};

/// One body to hand to the peer.
#[derive(Clone, Copy, Debug)]
pub struct Delivery<'a> {
    /// The message, for the peer's own deduplication.
    pub message: MessageId,
    /// The topic.
    pub topic: &'a Topic,
    /// The body, already in the peer's format.
    pub body: &'a Value,
    /// The instant of this attempt, Unix milliseconds (an argument, never read here).
    pub now_ms: u64,
}

/// What the peer answered. The body is read for a business acknowledgement and for nothing
/// else; it is never logged.
#[derive(Clone, PartialEq, Eq)]
pub struct PeerAnswer {
    /// An HTTP-style status.
    pub status: u16,
    /// The answer body, possibly empty.
    pub body: String,
}

impl fmt::Debug for PeerAnswer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PeerAnswer")
            .field("status", &self.status)
            .field("body_bytes", &self.body.len())
            .finish()
    }
}

/// Why no answer came back. Always retryable; carries a stable code and nothing else.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ChannelFailure {
    code: &'static str,
}

impl ChannelFailure {
    /// A failure with `code`, one of the `link_external` codes.
    #[must_use]
    pub const fn new(code: &'static str) -> Self {
        Self { code }
    }

    /// The peer could not be reached.
    #[must_use]
    pub const fn unreachable() -> Self {
        Self::new(codes::PEER_UNREACHABLE)
    }

    /// The peer did not answer in time.
    #[must_use]
    pub const fn timed_out() -> Self {
        Self::new(codes::PEER_TIMEOUT)
    }

    /// The stable code.
    #[must_use]
    pub const fn code(self) -> &'static str {
        self.code
    }
}

impl fmt::Debug for ChannelFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ChannelFailure").field(&self.code).finish()
    }
}

/// Moves one body to the peer.
#[async_trait]
pub trait PeerChannel: Send + Sync {
    /// Delivers `delivery` to `peer`.
    ///
    /// # Errors
    /// A [`ChannelFailure`] when no answer came back; the caller retries later.
    async fn deliver(
        &self,
        peer: &ExternalPeer,
        delivery: &Delivery<'_>,
    ) -> Result<PeerAnswer, ChannelFailure>;
}

fn micros(now_ms: u64) -> i64 {
    i64::try_from(now_ms)
        .unwrap_or(i64::MAX / 1_000)
        .saturating_mul(1_000)
}

/// Delivery by a signed `POST` through the egress policy: the `spot-webhooks` signature header
/// and egress connector, with the secret resolved from the vault for this call only.
pub struct WebhookChannel {
    egress: Arc<dyn Egress>,
    vault: Arc<dyn CredentialVault>,
    tenant: String,
}

impl WebhookChannel {
    /// A channel posting for `tenant` through `egress`, signing with secrets from `vault`.
    #[must_use]
    pub fn new(
        egress: Arc<dyn Egress>,
        vault: Arc<dyn CredentialVault>,
        tenant: impl Into<String>,
    ) -> Self {
        Self {
            egress,
            vault,
            tenant: tenant.into(),
        }
    }
}

#[async_trait]
impl PeerChannel for WebhookChannel {
    async fn deliver(
        &self,
        peer: &ExternalPeer,
        delivery: &Delivery<'_>,
    ) -> Result<PeerAnswer, ChannelFailure> {
        let Outbound::WebhookUrl(target) = &peer.outbound else {
            return Err(ChannelFailure::new(codes::MISCONFIGURED));
        };
        let url = url::Url::parse(target).map_err(|_| ChannelFailure::new(codes::MISCONFIGURED))?;
        let body = serde_json::to_vec(delivery.body)
            .map_err(|_| ChannelFailure::new(codes::MISCONFIGURED))?;
        let secret = self
            .vault
            .resolve(&peer.auth, micros(delivery.now_ms))
            .await
            .map_err(|_| ChannelFailure::new(codes::CREDENTIAL_UNAVAILABLE))?;
        let seconds = i64::try_from(delivery.now_ms / 1_000).unwrap_or(i64::MAX);
        let signature = SigningKey::new(secret.expose().as_bytes()).sign(seconds, &body);
        drop(secret);
        let mut headers = HeaderMap::new();
        let mut put = |name: &'static str, value: &str| -> Result<(), ChannelFailure> {
            let value = HeaderValue::from_str(value)
                .map_err(|_| ChannelFailure::new(codes::MISCONFIGURED))?;
            headers.insert(HeaderName::from_static(name), value);
            Ok(())
        };
        put("content-type", "application/json")?;
        put("user-agent", USER_AGENT)?;
        put(
            "x-spot-signature",
            &build_signature_header(seconds, &signature),
        )?;
        put("x-spot-event-id", &delivery.message.to_string())?;
        let response = self
            .egress
            .post(&self.tenant, &url, headers, body)
            .await
            .map_err(|error| ChannelFailure::new(egress_code(&error)))?;
        Ok(PeerAnswer {
            status: response.status.as_u16(),
            body: String::from_utf8_lossy(&response.body).into_owned(),
        })
    }
}

fn egress_code(error: &EgressError) -> &'static str {
    use EgressError as E;
    match error {
        E::AddressDenied { .. } | E::PolicyDenied | E::PortDenied { .. } | E::ResolutionMixed => {
            codes::EGRESS_DENIED
        }
        E::InvalidUrl | E::InvalidConfig(_) | E::RequestTooLarge { .. } => codes::MISCONFIGURED,
        E::ResolutionFailed | E::ResponseTooLarge | E::RedirectDenied | E::TooManyRedirects => {
            codes::PEER_UNREACHABLE
        }
    }
}

/// Delivery by one operation of an installed connector (WP-INT-05), which holds the peer's
/// protocol and its own credential injection.
pub struct ConnectorChannel {
    port: Arc<dyn ConnectorPort>,
    tenant: String,
    deadline_ms: u64,
}

impl ConnectorChannel {
    /// A channel invoking connectors for `tenant`, each call with `deadline_ms` to finish.
    #[must_use]
    pub fn new(port: Arc<dyn ConnectorPort>, tenant: impl Into<String>, deadline_ms: u64) -> Self {
        Self {
            port,
            tenant: tenant.into(),
            deadline_ms,
        }
    }
}

#[async_trait]
impl PeerChannel for ConnectorChannel {
    async fn deliver(
        &self,
        peer: &ExternalPeer,
        delivery: &Delivery<'_>,
    ) -> Result<PeerAnswer, ChannelFailure> {
        let Outbound::ConnectorOp {
            connector,
            operation,
        } = &peer.outbound
        else {
            return Err(ChannelFailure::new(codes::MISCONFIGURED));
        };
        let call = ConnectorCall {
            connector: connector.clone(),
            operation: operation.clone(),
            input: delivery.body.clone(),
            tenant: self.tenant.clone(),
            actor: None,
            now_micros: micros(delivery.now_ms),
            deadline_micros: micros(delivery.now_ms.saturating_add(self.deadline_ms)),
        };
        match self.port.invoke(call).await {
            Ok(output) => Ok(PeerAnswer {
                status: 200,
                body: output.value.to_string(),
            }),
            // The connector's own refusal of this message: the peer said no.
            Err(ConnectorError::Guest {
                retryable: false, ..
            }) => Ok(PeerAnswer {
                status: 422,
                body: String::new(),
            }),
            Err(ConnectorError::Timeout { .. }) => Err(ChannelFailure::timed_out()),
            Err(error) if error.is_retryable() => Err(ChannelFailure::unreachable()),
            // A permanent failure that is not the peer's answer (unknown connector, denied
            // egress, bad mapping): the configuration is wrong, the message must wait.
            Err(_) => Err(ChannelFailure::new(codes::MISCONFIGURED)),
        }
    }
}
