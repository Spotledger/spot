//! The outgoing-webhook node: it signs a JSON payload with `spot-webhooks` and posts it through a
//! [`WebhookPort`], which is the webhooks crate's egress connector.
//!
//! The signing credential is a reference; the handler opens it at call time, signs the exact body
//! bytes, and never puts the value in the request, the output or the history. The signature
//! header is `X-Spot-Signature: t=<unix>,v1=<hex>`, the same scheme the webhooks crate's
//! receivers verify.

use std::sync::Arc;

use async_trait::async_trait;
use http::{HeaderName, HeaderValue};
use serde_json::Value as Json;
use spot_bpm_engine::NodeOutcome;
use spot_webhooks::SIGNATURE_HEADER;
use spot_webhooks::signing::{SigningKey, build_signature_header};

use crate::config::{StatusClass, StatusRules, WebhookNodeConfig};
use crate::error::IntegrationError;
use crate::handler::{IntegrationCtx, IntegrationHandler, IntegrationKind};
use crate::mapping::render_template;
use crate::ports::WebhookPort;

/// The header a retry carries the idempotency key in.
pub const IDEMPOTENCY_HEADER: &str = "Idempotency-Key";

/// Runs outgoing-webhook nodes through a webhook port.
pub struct WebhookNodeHandler {
    webhooks: Arc<dyn WebhookPort>,
}

impl WebhookNodeHandler {
    /// A handler that posts through `webhooks`.
    #[must_use]
    pub fn new(webhooks: Arc<dyn WebhookPort>) -> Self {
        Self { webhooks }
    }
}

#[async_trait]
impl IntegrationHandler for WebhookNodeHandler {
    fn kind(&self) -> IntegrationKind {
        IntegrationKind::Webhook
    }

    async fn run(&self, ctx: &mut IntegrationCtx<'_>) -> NodeOutcome {
        let config = match WebhookNodeConfig::from_json(&ctx.node.to_string(), ctx.config) {
            Ok(config) => config,
            Err(error) => {
                return ctx.classify_failure(&error, &crate::config::RetryPolicy::default());
            }
        };
        match self.call(ctx, &config).await {
            Ok(outcome) => outcome,
            Err(error) => ctx.classify_failure(&error, &config.retry),
        }
    }
}

impl WebhookNodeHandler {
    /// Signs and posts one payload.
    async fn call(
        &self,
        ctx: &mut IntegrationCtx<'_>,
        config: &WebhookNodeConfig,
    ) -> Result<NodeOutcome, IntegrationError> {
        let context = ctx.mapping_context();
        let url = render_template(&config.url, &context)?;

        let secret = ctx.resolve(&config.credential_ref).await?;
        let key = SigningKey::new(secret.expose().as_bytes());

        let payload = match &config.payload {
            Some(mapping) => mapping.apply(&context, ctx.now)?,
            None => context.clone(),
        };
        let body = serde_json::to_vec(&payload).map_err(|e| IntegrationError::ConfigInvalid {
            node: ctx.node.to_string(),
            detail: format!("the webhook payload did not serialise: {e}"),
        })?;

        let timestamp = ctx.now.unix_timestamp();
        let signature = key.sign(timestamp, &body);
        let signature_header = build_signature_header(timestamp, &signature);

        let mut headers = http::HeaderMap::new();
        headers.insert(
            http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        headers.insert(
            HeaderName::from_static("user-agent"),
            HeaderValue::from_static(spot_webhooks::USER_AGENT),
        );
        headers.insert(
            HeaderName::from_bytes(SIGNATURE_HEADER.as_bytes()).map_err(|e| {
                IntegrationError::ConfigInvalid {
                    node: ctx.node.to_string(),
                    detail: format!("the signature header name is invalid: {e}"),
                }
            })?,
            HeaderValue::from_str(&signature_header).map_err(|e| {
                IntegrationError::ConfigInvalid {
                    node: ctx.node.to_string(),
                    detail: format!("the signature header value is invalid: {e}"),
                }
            })?,
        );
        headers.insert(
            HeaderName::from_static("idempotency-key"),
            HeaderValue::from_str(&ctx.key.to_string()).map_err(|e| {
                IntegrationError::ConfigInvalid {
                    node: ctx.node.to_string(),
                    detail: format!("the idempotency key is not a header value: {e}"),
                }
            })?,
        );
        for (name, template) in &config.headers {
            let value = render_template(template, &context)?;
            let name = HeaderName::from_bytes(name.as_bytes()).map_err(|e| {
                IntegrationError::ConfigInvalid {
                    node: ctx.node.to_string(),
                    detail: format!("`{name}` is not a valid header name: {e}"),
                }
            })?;
            let value =
                HeaderValue::from_str(&value).map_err(|e| IntegrationError::ConfigInvalid {
                    node: ctx.node.to_string(),
                    detail: format!("a header value is not valid: {e}"),
                })?;
            headers.insert(name, value);
        }

        tracing::debug!(target: "bpm_integration", node = %ctx.node, attempt = ctx.attempt, "webhook post");
        let response = self.webhooks.post(&url, headers, body).await?;
        match StatusRules::default().classify(response.status) {
            StatusClass::Success => {
                let mut output = response
                    .body_json()
                    .unwrap_or_else(|| Json::Object(serde_json::Map::new()));
                ctx.redact_json(&mut output);
                Ok(NodeOutcome::Done {
                    output: Some(output),
                    branches: Vec::new(),
                })
            }
            StatusClass::Retry => Ok(ctx.retry_or_fail(
                "bpm_integration.http_status",
                &ctx.redact_reason(&format!("HTTP {} may succeed on retry", response.status)),
                &config.retry,
            )),
            StatusClass::Error => Ok(NodeOutcome::Fail {
                code: String::from("bpm_integration.http_status"),
                reason: ctx.redact_reason(&format!("HTTP {}", response.status)),
            }),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::credential::InMemorySecretResolver;
    use crate::handler::{IntegrationCtx, IntegrationKind};
    use crate::ports::{FakeWebhook, HttpResponse};
    use crate::registry::SecretRegistry;
    use http::HeaderMap;
    use serde_json::json;
    use spot_app_api::prelude::OffsetDateTime;
    use spot_bpm_engine::{CaseId, IdempotencyKey};
    use spot_dag::NodeId;

    const CANARY: &str = "canary-signing-secret-0123456789";

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
    }

    async fn run(
        handler: &WebhookNodeHandler,
        config: &Json,
        input: &Json,
        secrets: &InMemorySecretResolver,
    ) -> NodeOutcome {
        let case = CaseId::new("case1").unwrap();
        let node = NodeId::new("n1").unwrap();
        let outputs = json!({});
        let mut variables = json!({});
        let mut registry = SecretRegistry::new().unwrap();
        let mut ctx = IntegrationCtx {
            case: &case,
            node: &node,
            attempt: 1,
            key: IdempotencyKey::new(case.clone(), node.clone(), 1),
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

    fn ok() -> HttpResponse {
        HttpResponse {
            status: 200,
            headers: HeaderMap::new(),
            body: br#"{"delivered":true}"#.to_vec(),
        }
    }

    #[tokio::test]
    async fn a_payload_is_signed_and_posted() {
        let fake = Arc::new(FakeWebhook::new());
        fake.push_response(ok());
        let handler = WebhookNodeHandler::new(fake.clone());
        let mut secrets = InMemorySecretResolver::new();
        secrets.insert("hook_key", CANARY);
        let config = json!({
            "url": "https://peer.test/hooks/{input.topic}",
            "credential_ref": "hook_key",
            "payload": { "order": "doc.input.order_id" },
        });
        let input = json!({ "topic": "orders", "order_id": "SO-1" });
        let outcome = run(&handler, &config, &input, &secrets).await;
        assert!(matches!(outcome, NodeOutcome::Done { .. }), "{outcome:?}");
        let posts = fake.posts();
        assert_eq!(posts[0].url, "https://peer.test/hooks/orders");
        assert_eq!(
            serde_json::from_slice::<Json>(&posts[0].body).unwrap(),
            json!({ "order": "SO-1" })
        );
        let signature = posts[0]
            .headers
            .get(SIGNATURE_HEADER)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(signature.starts_with("t=1700000000,v1="), "{signature}");
        // The signature is over the exact body and the resolved secret, and the secret itself is
        // never on the wire.
        assert!(!format!("{:?}", posts[0].headers).contains(CANARY));
    }

    #[tokio::test]
    async fn a_missing_signing_credential_fails_the_node() {
        let fake = Arc::new(FakeWebhook::new());
        let handler = WebhookNodeHandler::new(fake);
        let secrets = InMemorySecretResolver::new();
        let config = json!({ "url": "https://peer.test/hook", "credential_ref": "absent" });
        let outcome = run(&handler, &config, &json!({}), &secrets).await;
        match outcome {
            NodeOutcome::Fail { code, .. } => {
                assert_eq!(code, "bpm_integration.credential_missing");
            }
            other => panic!("expected fail, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_server_error_retries_and_a_client_error_fails() {
        let fake = Arc::new(FakeWebhook::new());
        fake.push_response(HttpResponse {
            status: 500,
            headers: HeaderMap::new(),
            body: Vec::new(),
        });
        let handler = WebhookNodeHandler::new(fake);
        let mut secrets = InMemorySecretResolver::new();
        secrets.insert("k", CANARY);
        let config = json!({ "url": "https://peer.test/hook", "credential_ref": "k" });
        let outcome = run(&handler, &config, &json!({}), &secrets).await;
        assert!(matches!(outcome, NodeOutcome::Retry { .. }), "{outcome:?}");

        let fake = Arc::new(FakeWebhook::new());
        fake.push_response(HttpResponse {
            status: 400,
            headers: HeaderMap::new(),
            body: Vec::new(),
        });
        let handler = WebhookNodeHandler::new(fake);
        let outcome = run(&handler, &config, &json!({}), &secrets).await;
        assert!(matches!(outcome, NodeOutcome::Fail { .. }), "{outcome:?}");
    }

    #[test]
    fn the_kind_is_webhook() {
        let handler = WebhookNodeHandler::new(Arc::new(FakeWebhook::new()));
        assert_eq!(handler.kind(), IntegrationKind::Webhook);
    }
}
