//! `spot-bpm-integration` — the integration nodes of the Business Process engine (WP-BPM-06.T1).
//!
//! A Process's Integration node calls another system: an HTTP endpoint, an e-mail, an outgoing
//! webhook, a TypeScript code step or a WASM connector. This crate is the node side of that
//! work package — the handlers the engine registers for `integration` and `script` nodes, the
//! data mapping they share with the rest of the engine, and the rule the whole package is judged
//! by: **a credential never appears in a log or in the execution history**.
//!
//! # Shape
//!
//! - [`IntegrationHandler`] is the asynchronous unit of work. It reads an [`IntegrationCtx`]
//!   (the Case, the node, the attempt, the idempotency key, the mapping context and the vault)
//!   and returns the engine's [`NodeOutcome`]: `Done`, `Retry` or `Fail` (the error branch).
//!   Handlers: [`HttpNodeHandler`], [`MailNodeHandler`], [`WebhookNodeHandler`],
//!   [`CodeNodeHandler`] and [`ConnectorNodeHandler`], in [`IntegrationRegistry`].
//! - Every outbound call goes through a port ([`EgressPort`], [`MailPort`], [`WebhookPort`],
//!   [`ConnectorPort`]) and the production ports go through the egress policy. A handler never
//!   opens a socket. [`ConnectorPort`] is the seam WP-INT-05 implements over `spot-wasm`.
//! - A node carries a [`CredentialRef`] only. [`IntegrationCtx::resolve`] opens it through the
//!   vault at call time and returns the redacting [`SecretString`](spot_secrets::SecretString);
//!   the [`SecretRegistry`] then scrubs the value from anything observable, including strings
//!   that merely contain it. [`ExecutionHistory`] stores redacted inputs and outputs.
//! - [`Mapping`] is the shared expression language (ADR-28) over `{input, case, nodes}`, and
//!   [`validate_config`] is the hook the process validator calls to reject a bad node before a
//!   Case ever runs it.
//! - Failure classes — transport, timeout, 4xx, 5xx, mail, webhook and connector — map to a
//!   retry or the error branch per the node's [`RetryPolicy`](config::RetryPolicy). The HTTP and
//!   webhook handlers send the idempotency key as `Idempotency-Key`.
//!
//! # Example
//!
//! ```
//! use spot_bpm_integration::{CredentialRef, Mapping, SecretRegistry};
//! use spot_secrets::SecretString;
//!
//! let mut registry = SecretRegistry::new().unwrap();
//! let reference = CredentialRef::new("sales_api");
//! assert!(registry.register(&reference, &SecretString::new("canary-secret-value")));
//! assert!(!registry.redact_text("token=canary-secret-value").contains("canary-secret-value"));
//!
//! let mapping = Mapping::from_json("n1", &serde_json::json!({ "id": "doc.input.id" })).unwrap();
//! assert!(mapping.validate().is_ok());
//! ```

#![forbid(unsafe_code)]

pub mod code;
pub mod config;
pub mod connector;
pub mod credential;
pub mod error;
pub mod handler;
pub mod history;
pub mod http;
pub mod mail;
pub mod mapping;
pub mod ports;
pub mod registry;
pub mod webhook;

mod host;
mod util;

use serde_json::Value as Json;

pub use code::CodeNodeHandler;
pub use config::{
    AuthScheme, CodeLimits, CodeNodeConfig, ConnectorNodeConfig, CredentialAuth, HttpMethod,
    HttpNodeConfig, MailNodeConfig, RetryDecision, RetryPolicy, StatusClass, StatusPattern,
    StatusRules, WebhookNodeConfig,
};
pub use connector::ConnectorNodeHandler;
pub use credential::{
    CredentialRef, InMemorySecretResolver, InMemoryVault, SealedEntry, SealedResolver, SealedVault,
    SecretResolver,
};
pub use error::{IntegrationError, IntegrationResult, RetryClass};
pub use handler::{IntegrationCtx, IntegrationHandler, IntegrationKind, IntegrationRegistry};
pub use history::{ExecutionHistory, NodeRecord, RecordState};
pub use http::{HttpNodeHandler, IDEMPOTENCY_HEADER};
pub use mail::MailNodeHandler;
pub use mapping::{Mapping, mapping_context, render_template};
pub use ports::{
    ConnectorCall, ConnectorPort, EgressMailPort, EgressPort, FakeConnector, FakeEgress, FakeMail,
    FakeWebhook, HttpCall, HttpResponse, MailPort, PinnedTransportFactory, RecordingFactory,
    SmtpPinnedFactory, SpotEgressPort, SpotWebhookPort, WebhookPort, WebhookPost,
};
pub use registry::{SecretRegistry, redact, redacted};
pub use webhook::WebhookNodeHandler;

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-bpm-integration";

/// Validates a node's config before a Case runs it: the hook BPM-01's process validator calls.
///
/// It parses the config into its kind's type and checks what is statically checkable — a URL
/// that parses, a mapping whose expressions parse, a mail node with a recipient and a body — so a
/// broken node is refused at publish time rather than at run time.
///
/// # Errors
///
/// [`IntegrationError::ConfigInvalid`], [`IntegrationError::UrlInvalid`] or
/// [`IntegrationError::MappingInvalid`].
pub fn validate_config(kind: IntegrationKind, config: &Json) -> IntegrationResult<()> {
    match kind {
        IntegrationKind::Http => HttpNodeConfig::from_json("validate", config)?.validate(),
        IntegrationKind::Mail => MailNodeConfig::from_json("validate", config)?.validate(),
        IntegrationKind::Webhook => WebhookNodeConfig::from_json("validate", config)?.validate(),
        IntegrationKind::Code => CodeNodeConfig::from_json("validate", config)?.validate(),
        IntegrationKind::Connector => {
            ConnectorNodeConfig::from_json("validate", config)?.validate()
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn validation_catches_a_bad_mapping_before_a_case_runs() {
        let config = json!({
            "url": "https://api.test/x",
            "body": { "x": "doc.==" },
        });
        let error = validate_config(IntegrationKind::Http, &config).unwrap_err();
        assert_eq!(error.code(), "bpm_integration.mapping_invalid");
    }

    #[test]
    fn validation_accepts_a_well_formed_node() {
        let config = json!({
            "method": "post",
            "url": "https://api.test/orders/{input.order_id}",
            "headers": { "X-Tenant": "{case.tenant}" },
            "body": { "id": "doc.input.order_id" },
        });
        assert!(validate_config(IntegrationKind::Http, &config).is_ok());
    }
}
