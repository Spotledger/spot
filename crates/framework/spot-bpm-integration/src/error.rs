//! Typed errors for `spot-bpm-integration`: one enum with stable `bpm_integration.*` codes.
//!
//! A caller matches on [`IntegrationError::code`], never on the message: the codes are this
//! crate's public vocabulary and their meaning never changes within a major version. Every code
//! has one entry in `spot-errors`' `bpm_integration.toml`, all with `class = "none"` because
//! these errors never ride an [`AppError`](spot_app_api::AppError): the engine turns them into a
//! `NodeOutcome::Fail` (the error branch) or a `NodeOutcome::Retry`, never into an app error.
//!
//! A message never names a secret: the reason a handler reports is passed through the redaction
//! pass first (see [`crate::registry`]), and a value that could not be registered for scrubbing
//! is replaced by a placeholder rather than printed.

/// Result of every fallible operation in this crate.
pub type IntegrationResult<T> = Result<T, IntegrationError>;

/// How a failure should be treated by a node's retry policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryClass {
    /// The failure may resolve on a later attempt: a transport error, a timeout, a retryable
    /// status, or a transient mail failure.
    Transient,
    /// The failure will not change on retry: a malformed config, a denied destination, a
    /// non-retryable status, or a permanent mail failure.
    Permanent,
}

/// One problem while running an integration node.
///
/// The variants are the failure classes the brief names: config, mapping, credential, egress,
/// transport, timeout, HTTP status, mail, webhook, connector and script. Each carries only
/// plain data; no variant can hold a resolved secret.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IntegrationError {
    /// The node's config is not the shape its kind needs.
    #[error("node `{node}` has an invalid integration config: {detail}")]
    ConfigInvalid {
        /// The node id.
        node: String,
        /// What the node needed but did not find.
        detail: String,
    },
    /// A URL is not absolute or not `http`/`https`.
    #[error("the URL is invalid: {detail}")]
    UrlInvalid {
        /// A plain-language reason that names no value.
        detail: String,
    },
    /// A credentialed request's host is not literally written in the node's configuration.
    #[error(
        "node `{node}` refused to send its credential to a host its configuration does not name"
    )]
    CredentialHostMismatch {
        /// The node id.
        node: String,
    },
    /// A template placeholder is malformed or names nothing.
    #[error("the template is invalid: {detail}")]
    TemplateInvalid {
        /// A plain-language reason that names no value.
        detail: String,
    },
    /// A mapping target or expression is not statically valid.
    #[error("the mapping target `{target}` is invalid: {detail}")]
    MappingInvalid {
        /// The target path the mapping writes.
        target: String,
        /// The parser's message.
        detail: String,
    },
    /// A mapping expression did not evaluate against the context.
    #[error("the mapping target `{target}` could not be evaluated: {detail}")]
    MappingEvaluate {
        /// The target path the mapping writes.
        target: String,
        /// The evaluator's message.
        detail: String,
    },
    /// A node named a credential the vault does not hold.
    #[error("no credential is stored under `{reference}`")]
    CredentialMissing {
        /// The reference's name (public; never the value).
        reference: String,
    },
    /// The vault refused to open the credential.
    #[error("the credential `{reference}` could not be resolved: {detail}")]
    SecretResolveFailed {
        /// The reference's name (public; never the value).
        reference: String,
        /// The vault's message, which names no secret.
        detail: String,
    },
    /// The egress policy denied the destination. Never retried.
    #[error("the egress policy denied the call: {code}")]
    EgressDenied {
        /// The egress error's stable code (`egress.*`).
        code: String,
    },
    /// A transport error: connection, DNS, TLS or a malformed exchange. Retryable.
    #[error("the call failed: {detail}")]
    TransportFailed {
        /// The transport's stable code or a plain-language reason.
        detail: String,
    },
    /// The call passed its deadline. Retryable.
    #[error("the call timed out after {seconds} seconds")]
    Timeout {
        /// The node's configured timeout, in whole seconds.
        seconds: u64,
    },
    /// The endpoint answered with a status the node's rules call an error. Not retryable by
    /// default; a retryable status is reported as [`IntegrationError::TransportFailed`] with
    /// the retry rule's code.
    #[error("the endpoint answered with status {status}")]
    HttpStatus {
        /// The response status.
        status: u16,
    },
    /// The mail transport failed transiently (connection, timeout, 4xx, TLS). Retryable.
    #[error("the mail transport failed transiently: {detail}")]
    MailTransient {
        /// The transport's message, which names no credential.
        detail: String,
    },
    /// The mail transport failed permanently (5xx, authentication, invalid message).
    #[error("the mail transport failed permanently: {detail}")]
    MailPermanent {
        /// The transport's message, which names no credential.
        detail: String,
    },
    /// The webhook delivery failed.
    #[error("the webhook delivery failed: {detail}")]
    WebhookFailed {
        /// The transport's message, which names no signing secret.
        detail: String,
    },
    /// A connector call failed.
    #[error("the connector `{connector}` failed: {detail}")]
    ConnectorFailed {
        /// The connector's name.
        connector: String,
        /// The connector host's message, which names no credential.
        detail: String,
    },
    /// A TypeScript code node failed.
    #[error("the code node `{node}` failed: {code}")]
    ScriptFailed {
        /// The node id.
        node: String,
        /// The script engine's stable code (`script.*`).
        code: String,
    },
    /// A TypeScript code node hit one of its limits.
    #[error("the code node `{node}` hit a limit: {detail}")]
    ScriptLimit {
        /// The node id.
        node: String,
        /// Which limit, and how.
        detail: String,
    },
}

impl IntegrationError {
    /// The stable machine-readable code for this error.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::ConfigInvalid { .. } => "bpm_integration.config_invalid",
            Self::UrlInvalid { .. } => "bpm_integration.url_invalid",
            Self::CredentialHostMismatch { .. } => "bpm_integration.credential_host_mismatch",
            Self::TemplateInvalid { .. } => "bpm_integration.template_invalid",
            Self::MappingInvalid { .. } => "bpm_integration.mapping_invalid",
            Self::MappingEvaluate { .. } => "bpm_integration.mapping_evaluate",
            Self::CredentialMissing { .. } => "bpm_integration.credential_missing",
            Self::SecretResolveFailed { .. } => "bpm_integration.secret_resolve_failed",
            Self::EgressDenied { .. } => "bpm_integration.egress_denied",
            Self::TransportFailed { .. } => "bpm_integration.transport_failed",
            Self::Timeout { .. } => "bpm_integration.timeout",
            Self::HttpStatus { .. } => "bpm_integration.http_status",
            Self::MailTransient { .. } => "bpm_integration.mail_transient",
            Self::MailPermanent { .. } => "bpm_integration.mail_permanent",
            Self::WebhookFailed { .. } => "bpm_integration.webhook_failed",
            Self::ConnectorFailed { .. } => "bpm_integration.connector_failed",
            Self::ScriptFailed { .. } => "bpm_integration.script_failed",
            Self::ScriptLimit { .. } => "bpm_integration.script_limit",
        }
    }

    /// Whether the failure should be retried or sent to the error branch.
    #[must_use]
    pub fn retry_class(&self) -> RetryClass {
        match self {
            Self::TransportFailed { .. }
            | Self::Timeout { .. }
            | Self::MailTransient { .. }
            | Self::WebhookFailed { .. } => RetryClass::Transient,
            Self::ConfigInvalid { .. }
            | Self::UrlInvalid { .. }
            | Self::CredentialHostMismatch { .. }
            | Self::TemplateInvalid { .. }
            | Self::MappingInvalid { .. }
            | Self::MappingEvaluate { .. }
            | Self::CredentialMissing { .. }
            | Self::SecretResolveFailed { .. }
            | Self::EgressDenied { .. }
            | Self::HttpStatus { .. }
            | Self::MailPermanent { .. }
            | Self::ConnectorFailed { .. }
            | Self::ScriptFailed { .. }
            | Self::ScriptLimit { .. } => RetryClass::Permanent,
        }
    }
}

/// A `bpm_integration.*` code from an egress error's own code, so a denial is reported under a
/// stable node-level code while keeping the egress code as a parameter.
#[must_use]
pub fn egress_denied(code: &str) -> IntegrationError {
    IntegrationError::EgressDenied {
        code: code.to_owned(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn every_code_has_a_catalogue_entry() {
        let errors = vec![
            IntegrationError::ConfigInvalid {
                node: "n".to_owned(),
                detail: "d".to_owned(),
            },
            IntegrationError::UrlInvalid {
                detail: "d".to_owned(),
            },
            IntegrationError::CredentialHostMismatch {
                node: "n".to_owned(),
            },
            IntegrationError::TemplateInvalid {
                detail: "d".to_owned(),
            },
            IntegrationError::MappingInvalid {
                target: "t".to_owned(),
                detail: "d".to_owned(),
            },
            IntegrationError::MappingEvaluate {
                target: "t".to_owned(),
                detail: "d".to_owned(),
            },
            IntegrationError::CredentialMissing {
                reference: "r".to_owned(),
            },
            IntegrationError::SecretResolveFailed {
                reference: "r".to_owned(),
                detail: "d".to_owned(),
            },
            IntegrationError::EgressDenied {
                code: "egress.denied_by_policy".to_owned(),
            },
            IntegrationError::TransportFailed {
                detail: "d".to_owned(),
            },
            IntegrationError::Timeout { seconds: 5 },
            IntegrationError::HttpStatus { status: 404 },
            IntegrationError::MailTransient {
                detail: "d".to_owned(),
            },
            IntegrationError::MailPermanent {
                detail: "d".to_owned(),
            },
            IntegrationError::WebhookFailed {
                detail: "d".to_owned(),
            },
            IntegrationError::ConnectorFailed {
                connector: "c".to_owned(),
                detail: "d".to_owned(),
            },
            IntegrationError::ScriptFailed {
                node: "n".to_owned(),
                code: "script.thrown".to_owned(),
            },
            IntegrationError::ScriptLimit {
                node: "n".to_owned(),
                detail: "d".to_owned(),
            },
        ];
        let catalogue = spot_errors::Catalogue::load();
        assert_eq!(
            catalogue.load_error(),
            None,
            "the catalogue must load clean"
        );
        let mut seen = std::collections::BTreeSet::new();
        for error in &errors {
            let code = error.code();
            assert!(code.starts_with("bpm_integration."), "{code}");
            assert!(seen.insert(code), "duplicate code {code}");
            assert!(
                catalogue.lookup(code).is_some(),
                "no catalogue entry for `{code}`"
            );
        }
        assert_eq!(seen.len(), 18, "a variant is missing from the list");
    }

    #[test]
    fn transport_and_timeout_are_transient() {
        assert_eq!(
            IntegrationError::TransportFailed {
                detail: "reset".to_owned()
            }
            .retry_class(),
            RetryClass::Transient
        );
        assert_eq!(
            IntegrationError::Timeout { seconds: 1 }.retry_class(),
            RetryClass::Transient
        );
        assert_eq!(
            IntegrationError::EgressDenied {
                code: "egress.denied_by_policy".to_owned()
            }
            .retry_class(),
            RetryClass::Permanent
        );
    }
}
