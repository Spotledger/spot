//! `ConnectorPort`: the trait the business-process engine (WP-BPM-06) drives connectors
//! through.
//!
//! WP-BPM-06 defines the trait in `spot-bpm-integration`, which is not merged yet. This is the
//! equivalent trait, with the same name and shape, so the BPM crate can adopt it without a
//! rewrite: `describe`, `invoke`, `webhook`, `ping`, all async, all keyed by the connector's
//! [`ConnectorId`], all returning [`crate::ConnectorError`] with its retry classification.

use async_trait::async_trait;
use serde_json::Value;

use crate::error::Result;
use crate::model::{ConnectorId, ConnectorInfo, HttpRequest, HttpResponse};

/// One connector invocation: the connector, the operation, the input and the invocation
/// context. `now_micros` and `deadline_micros` are arguments, never read from a clock.
#[derive(Clone, Debug, PartialEq)]
pub struct ConnectorCall {
    /// The installed connector.
    pub connector: ConnectorId,
    /// The operation to invoke.
    pub operation: String,
    /// The operation's input, as JSON.
    pub input: Value,
    /// The tenant the call runs for.
    pub tenant: String,
    /// The actor (user or service), when there is one.
    pub actor: Option<String>,
    /// The invocation's time, in microseconds since the Unix epoch.
    pub now_micros: i64,
    /// The deadline, in microseconds since the Unix epoch.
    pub deadline_micros: i64,
}

/// What an invocation returned.
#[derive(Clone, Debug, PartialEq)]
pub struct ConnectorOutput {
    /// The operation's output, as JSON.
    pub value: Value,
}

/// A connector's health, from [`ConnectorPort::ping`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectorHealth {
    /// Whether the connector is healthy.
    pub healthy: bool,
    /// A short, secret-free detail.
    pub detail: String,
}

/// The port the BPM integration node drives. Implemented by [`crate::ConnectorHost`].
#[async_trait]
pub trait ConnectorPort: Send + Sync {
    /// The connector's identity, operations and auth kinds.
    async fn describe(&self, connector: &ConnectorId) -> Result<ConnectorInfo>;

    /// Invokes one operation.
    async fn invoke(&self, call: ConnectorCall) -> Result<ConnectorOutput>;

    /// Delivers an inbound webhook request to the connector.
    async fn webhook(
        &self,
        connector: &ConnectorId,
        request: HttpRequest,
        now_micros: i64,
    ) -> Result<HttpResponse>;

    /// A cheap health check: whether the connector is installed, compiled and its breaker is
    /// not open.
    async fn ping(&self, connector: &ConnectorId) -> Result<ConnectorHealth>;
}
