//! Typed connector errors with stable `connectors.*` codes and a retry classification.

use thiserror::Error;

/// The stable codes this crate raises. Each has one entry in
/// `crates/framework/spot-errors/catalogue/connectors.toml`.
pub mod codes {
    /// The connector id is unknown.
    pub const NOT_FOUND: &str = "connectors.not_found";
    /// The connector bundle is not a valid bundle.
    pub const BAD_BUNDLE: &str = "connectors.bad_bundle";
    /// The connector bundle's signature did not verify.
    pub const UNVERIFIED: &str = "connectors.unverified";
    /// The operation's input is not valid JSON or does not fit.
    pub const BAD_INPUT: &str = "connectors.bad_input";
    /// An outbound HTTP request the guest built is malformed.
    pub const BAD_REQUEST: &str = "connectors.bad_request";
    /// The connector does not support the named operation.
    pub const UNKNOWN_OPERATION: &str = "connectors.unknown_operation";
    /// The connector does not support the requested feature (an inbound webhook, say).
    pub const UNSUPPORTED: &str = "connectors.unsupported";
    /// The egress policy denied the destination.
    pub const EGRESS_DENIED: &str = "connectors.egress_denied";
    /// The configuration injects authentication but declares no endpoints.
    pub const ENDPOINTS_REQUIRED: &str = "connectors.endpoints_required";
    /// The connector asked for a credential it did not declare.
    pub const SECRET_DENIED: &str = "connectors.secret_denied";
    /// A declared credential could not be resolved.
    pub const SECRET_UNAVAILABLE: &str = "connectors.secret_unavailable";
    /// The host failed for a reason no connector caused.
    pub const HOST_FAILURE: &str = "connectors.host_failure";
    /// The call ran past its deadline.
    pub const TIMEOUT: &str = "connectors.timeout";
    /// The call used all of its fuel.
    pub const OUT_OF_FUEL: &str = "connectors.out_of_fuel";
    /// The connector trapped.
    pub const TRAPPED: &str = "connectors.trapped";
    /// The connector made more host calls than its limit.
    pub const TOO_MANY_HOST_CALLS: &str = "connectors.too_many_host_calls";
    /// The connector's result is larger than the output limit.
    pub const OUTPUT_TOO_LARGE: &str = "connectors.output_too_large";
    /// The connector's circuit breaker is open.
    pub const CIRCUIT_OPEN: &str = "connectors.circuit_open";
    /// A data-mapping rule failed.
    pub const MAPPING: &str = "connectors.mapping";
    /// The connector's key-value store is full.
    pub const KV_LIMIT: &str = "connectors.kv_limit";
    /// An OAuth2 token refresh failed.
    pub const OAUTH_REFRESH: &str = "connectors.oauth_refresh";
    /// The connector returned an error of its own.
    pub const GUEST: &str = "connectors.guest";
}

/// Whether retrying the same call may succeed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FailureClass {
    /// A later attempt may succeed (a transport failure, a timeout, a refused credential
    /// refresh).
    Retryable,
    /// A later attempt will fail the same way (a bad argument, an unknown operation, a
    /// signature that does not verify).
    Permanent,
}

/// Why a connector call failed. Every variant has a stable [`ConnectorError::code`]; none of
/// them carries a secret.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum ConnectorError {
    /// The connector id is unknown.
    #[error("connectors.not_found: no connector `{id}` is installed")]
    NotFound {
        /// The connector id.
        id: String,
    },
    /// The connector bundle is not a valid bundle.
    #[error("connectors.bad_bundle: {reason}")]
    BadBundle {
        /// What was wrong with it.
        reason: String,
    },
    /// The bundle's signature did not verify.
    #[error("connectors.unverified: {reason}")]
    Unverified {
        /// Why it was refused.
        reason: String,
    },
    /// The operation's input is not valid JSON or does not fit the operation.
    #[error("connectors.bad_input: {reason}")]
    BadInput {
        /// What was wrong with it.
        reason: String,
    },
    /// The guest built a malformed outbound HTTP request.
    #[error("connectors.bad_request: {reason}")]
    BadRequest {
        /// What was wrong with it.
        reason: String,
    },
    /// The connector does not support the named operation.
    #[error("connectors.unknown_operation: the connector has no operation `{operation}`")]
    UnknownOperation {
        /// The operation name.
        operation: String,
    },
    /// The connector does not support the requested feature.
    #[error("connectors.unsupported: {what}")]
    Unsupported {
        /// The feature.
        what: String,
    },
    /// The configuration injects authentication but declares no endpoints.
    #[error(
        "connectors.endpoints_required: connector `{connector}` injects authentication but declares no endpoints"
    )]
    EndpointsRequired {
        /// The connector id.
        connector: String,
    },
    /// The egress policy denied the destination.
    #[error("connectors.egress_denied: the outbound destination `{host}` is not allowed")]
    EgressDenied {
        /// The host that was denied.
        host: String,
    },
    /// The connector asked for a credential it did not declare.
    #[error("connectors.secret_denied: the connector may not read `{reference}`")]
    SecretDenied {
        /// The reference.
        reference: String,
    },
    /// A declared credential could not be resolved.
    #[error("connectors.secret_unavailable: `{reference}` could not be resolved")]
    SecretUnavailable {
        /// The reference.
        reference: String,
    },
    /// The host failed for a reason no connector caused.
    #[error("connectors.host_failure: {reason}")]
    HostFailure {
        /// What failed.
        reason: String,
    },
    /// The call ran past its deadline.
    #[error("connectors.timeout: the connector did not finish within {millis} ms")]
    Timeout {
        /// The deadline, in milliseconds.
        millis: u128,
    },
    /// The call used all of its fuel.
    #[error("connectors.out_of_fuel: the connector used all {fuel} units of fuel")]
    OutOfFuel {
        /// The fuel the call was given.
        fuel: u64,
    },
    /// The connector trapped.
    #[error("connectors.trapped: {reason}")]
    Trapped {
        /// The trap's first line.
        reason: String,
    },
    /// The connector made more host calls than its limit.
    #[error("connectors.too_many_host_calls: more than {limit} host calls")]
    TooManyHostCalls {
        /// The limit.
        limit: u32,
    },
    /// The connector's result is larger than the output limit.
    #[error("connectors.output_too_large: the result is larger than {limit} bytes")]
    OutputTooLarge {
        /// The limit, in bytes.
        limit: usize,
    },
    /// The connector's circuit breaker is open.
    #[error("connectors.circuit_open: the breaker is open for {retry_after_secs}s")]
    CircuitOpen {
        /// Seconds until the next probe.
        retry_after_secs: u64,
    },
    /// A data-mapping rule failed.
    #[error("connectors.mapping: rule `{rule}` failed: {reason}")]
    Mapping {
        /// The rule that failed.
        rule: String,
        /// Why.
        reason: String,
    },
    /// The connector's key-value store is full.
    #[error("connectors.kv_limit: the key-value store is at its {limit}-byte limit")]
    KvLimit {
        /// The limit, in bytes.
        limit: usize,
    },
    /// An OAuth2 token refresh failed.
    #[error("connectors.oauth_refresh: {reason}")]
    OAuthRefresh {
        /// Why.
        reason: String,
    },
    /// The connector returned an error of its own.
    #[error("connectors.guest: the connector returned `{code}`: {message}")]
    Guest {
        /// The connector's code.
        code: String,
        /// The connector's message.
        message: String,
        /// The connector's own retry hint.
        retryable: bool,
    },
}

impl ConnectorError {
    /// The stable machine-readable code, also the prefix of the message.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound { .. } => codes::NOT_FOUND,
            Self::BadBundle { .. } => codes::BAD_BUNDLE,
            Self::Unverified { .. } => codes::UNVERIFIED,
            Self::BadInput { .. } => codes::BAD_INPUT,
            Self::BadRequest { .. } => codes::BAD_REQUEST,
            Self::UnknownOperation { .. } => codes::UNKNOWN_OPERATION,
            Self::Unsupported { .. } => codes::UNSUPPORTED,
            Self::EgressDenied { .. } => codes::EGRESS_DENIED,
            Self::EndpointsRequired { .. } => codes::ENDPOINTS_REQUIRED,
            Self::SecretDenied { .. } => codes::SECRET_DENIED,
            Self::SecretUnavailable { .. } => codes::SECRET_UNAVAILABLE,
            Self::HostFailure { .. } => codes::HOST_FAILURE,
            Self::Timeout { .. } => codes::TIMEOUT,
            Self::OutOfFuel { .. } => codes::OUT_OF_FUEL,
            Self::Trapped { .. } => codes::TRAPPED,
            Self::TooManyHostCalls { .. } => codes::TOO_MANY_HOST_CALLS,
            Self::OutputTooLarge { .. } => codes::OUTPUT_TOO_LARGE,
            Self::CircuitOpen { .. } => codes::CIRCUIT_OPEN,
            Self::Mapping { .. } => codes::MAPPING,
            Self::KvLimit { .. } => codes::KV_LIMIT,
            Self::OAuthRefresh { .. } => codes::OAUTH_REFRESH,
            Self::Guest { .. } => codes::GUEST,
        }
    }

    /// Whether retrying the same call may succeed. A guest's own hint is honoured for its own
    /// errors; everything else the host decides.
    pub fn class(&self) -> FailureClass {
        match self {
            Self::Guest { retryable, .. } if *retryable => FailureClass::Retryable,
            Self::Guest { .. } => FailureClass::Permanent,
            Self::NotFound { .. }
            | Self::BadBundle { .. }
            | Self::Unverified { .. }
            | Self::BadInput { .. }
            | Self::BadRequest { .. }
            | Self::UnknownOperation { .. }
            | Self::Unsupported { .. }
            | Self::EgressDenied { .. }
            | Self::EndpointsRequired { .. }
            | Self::SecretDenied { .. }
            | Self::Mapping { .. }
            | Self::KvLimit { .. } => FailureClass::Permanent,
            Self::SecretUnavailable { .. }
            | Self::HostFailure { .. }
            | Self::Timeout { .. }
            | Self::OutOfFuel { .. }
            | Self::Trapped { .. }
            | Self::TooManyHostCalls { .. }
            | Self::OutputTooLarge { .. }
            | Self::CircuitOpen { .. }
            | Self::OAuthRefresh { .. } => FailureClass::Retryable,
        }
    }

    /// Whether a later attempt may succeed.
    pub fn is_retryable(&self) -> bool {
        self.class() == FailureClass::Retryable
    }
}

/// The result type of this crate.
pub type Result<T> = std::result::Result<T, ConnectorError>;
