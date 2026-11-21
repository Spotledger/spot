//! The WASM connector node: it maps the Case's data into an operation's input, calls the
//! connector through a [`ConnectorPort`] (WP-INT-05 implements the port over `spot-wasm`) and
//! maps the result back out.
//!
//! The node carries a credential reference only. It resolves the reference so the value is
//! registered for redaction, then passes the **reference** to the port: the host resolves it for
//! the connector's declared auth scheme, so the sandboxed guest never holds the value.
//!
//! A connector's own error is retried within the node's policy before it takes the error branch;
//! a missing or unresolvable credential, or an egress denial, fails at once.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value as Json;
use spot_bpm_engine::NodeOutcome;

use crate::config::ConnectorNodeConfig;
use crate::error::IntegrationError;
use crate::handler::{IntegrationCtx, IntegrationHandler, IntegrationKind};
use crate::ports::ConnectorPort;

/// Runs connector nodes through a connector port.
pub struct ConnectorNodeHandler {
    connector: Arc<dyn ConnectorPort>,
}

impl ConnectorNodeHandler {
    /// A handler that calls through `connector`.
    #[must_use]
    pub fn new(connector: Arc<dyn ConnectorPort>) -> Self {
        Self { connector }
    }
}

#[async_trait]
impl IntegrationHandler for ConnectorNodeHandler {
    fn kind(&self) -> IntegrationKind {
        IntegrationKind::Connector
    }

    async fn run(&self, ctx: &mut IntegrationCtx<'_>) -> NodeOutcome {
        let config = match ConnectorNodeConfig::from_json(&ctx.node.to_string(), ctx.config) {
            Ok(config) => config,
            Err(error) => {
                return ctx.classify_failure(&error, &crate::config::RetryPolicy::default());
            }
        };
        match self.call(ctx, &config).await {
            Ok(output) => NodeOutcome::Done {
                output: Some(output),
                branches: Vec::new(),
            },
            Err(IntegrationError::ConnectorFailed { detail, .. }) => {
                let reason = ctx.redact_reason(&format!("connector failed: {detail}"));
                ctx.retry_or_fail("bpm_integration.connector_failed", &reason, &config.retry)
            }
            Err(error) => ctx.classify_failure(&error, &config.retry),
        }
    }
}

impl ConnectorNodeHandler {
    /// Builds the input, calls the connector and maps the output.
    async fn call(
        &self,
        ctx: &mut IntegrationCtx<'_>,
        config: &ConnectorNodeConfig,
    ) -> Result<Json, IntegrationError> {
        let context = ctx.mapping_context();
        let input = match &config.input {
            Some(mapping) => mapping.apply(&context, ctx.now)?,
            None => ctx.input.clone(),
        };

        // Register the value for redaction, but hand the port the reference only.
        if let Some(reference) = &config.credential_ref {
            let _secret = ctx.resolve(reference).await?;
        }

        tracing::debug!(target: "bpm_integration", node = %ctx.node, attempt = ctx.attempt, connector = %config.connector, operation = %config.operation, "connector call");
        let output = self
            .connector
            .call(
                &config.connector,
                &config.operation,
                &input,
                config.credential_ref.as_ref(),
            )
            .await?;

        let mut output = match &config.output {
            Some(mapping) => {
                let mut extended = context;
                if let Json::Object(map) = &mut extended {
                    map.insert(String::from("output"), output);
                }
                mapping.apply(&extended, ctx.now)?
            }
            None => output,
        };
        ctx.redact_json(&mut output);
        Ok(output)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::credential::{CredentialRef, InMemorySecretResolver};
    use crate::handler::{IntegrationCtx, IntegrationKind};
    use crate::ports::FakeConnector;
    use crate::registry::SecretRegistry;
    use serde_json::json;
    use spot_app_api::prelude::OffsetDateTime;
    use spot_bpm_engine::{CaseId, IdempotencyKey};
    use spot_dag::NodeId;

    const CANARY: &str = "canary-connector-key-0123456789";

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
    }

    async fn run(
        handler: &ConnectorNodeHandler,
        config: &Json,
        input: &Json,
        secrets: &InMemorySecretResolver,
        attempt: u32,
    ) -> NodeOutcome {
        let case = CaseId::new("case1").unwrap();
        let node = NodeId::new("n1").unwrap();
        let outputs = json!({});
        let mut variables = json!({});
        let mut registry = SecretRegistry::new().unwrap();
        let mut ctx = IntegrationCtx {
            case: &case,
            node: &node,
            attempt,
            key: IdempotencyKey::new(case.clone(), node.clone(), attempt),
            is_retry: false,
            now: now(),
            principal: None,
            input,
            node_outputs: &outputs,
            variables: &mut variables,
            config,
            secrets,
            registry: &mut registry,
        };
        handler.run(&mut ctx).await
    }

    #[tokio::test]
    async fn an_operation_is_called_with_mapped_input() {
        let fake = Arc::new(FakeConnector::new());
        fake.push_output(json!({ "ack": "A-1" }));
        let handler = ConnectorNodeHandler::new(fake.clone());
        let secrets = InMemorySecretResolver::new();
        let config = json!({
            "connector": "as2",
            "operation": "send",
            "input": { "document": "doc.input.id" },
            "output": { "receipt": "doc.output.ack" },
        });
        let outcome = run(&handler, &config, &json!({ "id": "DOC-1" }), &secrets, 1).await;
        match outcome {
            NodeOutcome::Done { output, .. } => {
                assert_eq!(output, Some(json!({ "receipt": "A-1" })));
            }
            other => panic!("expected done, got {other:?}"),
        }
        let calls = fake.calls();
        assert_eq!(calls[0].connector, "as2");
        assert_eq!(calls[0].input, json!({ "document": "DOC-1" }));
    }

    #[tokio::test]
    async fn the_port_receives_the_reference_never_the_value() {
        let fake = Arc::new(FakeConnector::new());
        fake.push_output(json!({ "ok": true }));
        let handler = ConnectorNodeHandler::new(fake.clone());
        let mut secrets = InMemorySecretResolver::new();
        secrets.insert("as2_key", CANARY);
        let config = json!({
            "connector": "as2",
            "operation": "send",
            "credential_ref": "as2_key",
        });
        let outcome = run(&handler, &config, &json!({}), &secrets, 1).await;
        assert!(matches!(outcome, NodeOutcome::Done { .. }), "{outcome:?}");
        let calls = fake.calls();
        assert_eq!(calls[0].credential_ref, Some(CredentialRef::new("as2_key")));
        assert!(
            !format!("{:?}", calls[0]).contains(CANARY),
            "the connector call leaked the credential value"
        );
    }

    #[tokio::test]
    async fn a_connector_error_retries_then_fails() {
        let fake = Arc::new(FakeConnector::new());
        fake.push_error(IntegrationError::ConnectorFailed {
            connector: String::from("as2"),
            detail: String::from("the peer refused"),
        });
        let handler = ConnectorNodeHandler::new(fake);
        let secrets = InMemorySecretResolver::new();
        let config = json!({ "connector": "as2", "operation": "send" });
        let outcome = run(&handler, &config, &json!({}), &secrets, 1).await;
        assert!(matches!(outcome, NodeOutcome::Retry { .. }), "{outcome:?}");

        let fake = Arc::new(FakeConnector::new());
        fake.push_error(IntegrationError::ConnectorFailed {
            connector: String::from("as2"),
            detail: String::from("the peer refused"),
        });
        let handler = ConnectorNodeHandler::new(fake);
        let outcome = run(&handler, &config, &json!({}), &secrets, 3).await;
        match outcome {
            NodeOutcome::Fail { code, .. } => {
                assert_eq!(code, "bpm_integration.connector_failed");
            }
            other => panic!("expected fail, got {other:?}"),
        }
    }

    #[test]
    fn the_kind_is_connector() {
        let handler = ConnectorNodeHandler::new(Arc::new(FakeConnector::new()));
        assert_eq!(handler.kind(), IntegrationKind::Connector);
    }
}
