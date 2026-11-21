//! The e-mail node: it builds an [`OutgoingMessage`] from the node's config and sends it through
//! a [`MailPort`], which is `spot-mail`'s transport behind the egress guard.
//!
//! Every text field — the sender, the recipients, the subject and the bodies — may carry
//! `{doc.<path>}` placeholders. The message is validated by `spot-mail` before it is sent, so a
//! malformed address is a permanent failure (the error branch), not a retry.

use std::sync::Arc;

use async_trait::async_trait;
use spot_bpm_engine::NodeOutcome;
use spot_mail::{Mailbox, OutgoingMessage};

use crate::config::MailNodeConfig;
use crate::error::IntegrationError;
use crate::handler::{IntegrationCtx, IntegrationHandler, IntegrationKind};
use crate::mapping::render_template;
use crate::ports::MailPort;

/// Runs e-mail nodes through a mail port.
pub struct MailNodeHandler {
    mail: Arc<dyn MailPort>,
}

impl MailNodeHandler {
    /// A handler that sends through `mail`.
    #[must_use]
    pub fn new(mail: Arc<dyn MailPort>) -> Self {
        Self { mail }
    }
}

#[async_trait]
impl IntegrationHandler for MailNodeHandler {
    fn kind(&self) -> IntegrationKind {
        IntegrationKind::Mail
    }

    async fn run(&self, ctx: &mut IntegrationCtx<'_>) -> NodeOutcome {
        let config = match MailNodeConfig::from_json(&ctx.node.to_string(), ctx.config) {
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

impl MailNodeHandler {
    /// Builds and sends one message.
    async fn call(
        &self,
        ctx: &mut IntegrationCtx<'_>,
        config: &MailNodeConfig,
    ) -> Result<NodeOutcome, IntegrationError> {
        let context = ctx.mapping_context();

        // Resolve and register the credential first, so a value that ever reaches a log or the
        // history is scrubbed. The transport owns the actual SMTP credential; this keeps the
        // node's reference resolvable and redacted all the same.
        if let Some(reference) = &config.credential_ref {
            let _secret = ctx.resolve(reference).await?;
        }

        let from = mailbox(None, &render_template(&config.from, &context)?)?;
        let mut to = Vec::with_capacity(config.to.len());
        for address in &config.to {
            to.push(mailbox(None, &render_template(address, &context)?)?);
        }
        let mut cc = Vec::with_capacity(config.cc.len());
        for address in &config.cc {
            cc.push(mailbox(None, &render_template(address, &context)?)?);
        }
        let subject = render_template(&config.subject, &context)?;
        let text = match &config.text {
            Some(body) => Some(render_template(body, &context)?),
            None => None,
        };
        let html = match &config.html {
            Some(body) => Some(render_template(body, &context)?),
            None => None,
        };

        let mut message = OutgoingMessage::new(from, subject);
        message.to = to;
        message.cc = cc;
        message.text = text;
        message.html = html;
        message
            .validate()
            .map_err(|e| IntegrationError::MailPermanent {
                detail: e.to_string(),
            })?;

        tracing::debug!(target: "bpm_integration", node = %ctx.node, attempt = ctx.attempt, "mail send");
        self.mail.send(&message).await?;
        Ok(NodeOutcome::Done {
            output: Some(serde_json::json!({ "sent": true })),
            branches: Vec::new(),
        })
    }
}

/// Builds a validated mailbox.
fn mailbox(name: Option<String>, address: &str) -> Result<Mailbox, IntegrationError> {
    Mailbox::new(name, address).map_err(|e| IntegrationError::MailPermanent {
        detail: e.to_string(),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::credential::InMemorySecretResolver;
    use crate::handler::{IntegrationCtx, IntegrationKind};
    use crate::ports::FakeMail;
    use crate::registry::SecretRegistry;
    use serde_json::json;
    use spot_app_api::prelude::OffsetDateTime;
    use spot_bpm_engine::{CaseId, IdempotencyKey};
    use spot_dag::NodeId;

    const CANARY: &str = "canary-smtp-password-0123456789";

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
    }

    async fn run(
        handler: &MailNodeHandler,
        config: &serde_json::Value,
        input: &serde_json::Value,
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

    fn receipt() -> spot_mail::SendReceipt {
        spot_mail::SendReceipt {
            message_id: String::from("m1"),
            accepted: Vec::new(),
            rejected: Vec::new(),
        }
    }

    #[tokio::test]
    async fn a_message_is_rendered_and_sent() {
        let fake = Arc::new(FakeMail::new());
        fake.push_receipt(receipt());
        let handler = MailNodeHandler::new(fake.clone());
        let secrets = InMemorySecretResolver::new();
        let config = json!({
            "from": "orders@acme.test",
            "to": ["{input.customer}"],
            "subject": "Order {input.order_id}",
            "text": "Thanks for order {input.order_id}.",
        });
        let input = json!({ "customer": "jane@example.test", "order_id": "SO-1" });
        let outcome = run(&handler, &config, &input, &secrets, 1).await;
        assert!(matches!(outcome, NodeOutcome::Done { .. }), "{outcome:?}");
        let sent = fake.sent();
        assert_eq!(sent[0].to[0].address.to_string(), "jane@example.test");
        assert_eq!(sent[0].subject, "Order SO-1");
        assert_eq!(sent[0].text.as_deref(), Some("Thanks for order SO-1."));
    }

    #[tokio::test]
    async fn a_transient_failure_retries_and_a_permanent_one_fails() {
        let fake = Arc::new(FakeMail::new());
        fake.push_error(IntegrationError::MailTransient {
            detail: String::from("connection reset"),
        });
        let handler = MailNodeHandler::new(fake);
        let secrets = InMemorySecretResolver::new();
        let config = json!({
            "from": "a@b.test", "to": ["c@d.test"], "subject": "s", "text": "t",
        });
        let outcome = run(&handler, &config, &json!({}), &secrets, 1).await;
        assert!(matches!(outcome, NodeOutcome::Retry { .. }), "{outcome:?}");

        let fake = Arc::new(FakeMail::new());
        fake.push_error(IntegrationError::MailPermanent {
            detail: String::from("authentication failed"),
        });
        let handler = MailNodeHandler::new(fake);
        let outcome = run(&handler, &config, &json!({}), &secrets, 1).await;
        match outcome {
            NodeOutcome::Fail { code, .. } => assert_eq!(code, "bpm_integration.mail_permanent"),
            other => panic!("expected fail, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_credential_is_resolved_and_never_reaches_the_message() {
        let fake = Arc::new(FakeMail::new());
        fake.push_receipt(receipt());
        let handler = MailNodeHandler::new(fake.clone());
        let mut secrets = InMemorySecretResolver::new();
        secrets.insert("smtp", CANARY);
        let config = json!({
            "from": "a@b.test", "to": ["c@d.test"], "subject": "s", "text": "t",
            "credential_ref": "smtp",
        });
        let outcome = run(&handler, &config, &json!({}), &secrets, 1).await;
        assert!(matches!(outcome, NodeOutcome::Done { .. }));
        let sent = fake.sent();
        assert!(
            !format!("{sent:?}").contains(CANARY),
            "the message leaked the credential"
        );
    }

    #[tokio::test]
    async fn a_bad_address_is_a_permanent_failure() {
        let fake = Arc::new(FakeMail::new());
        let handler = MailNodeHandler::new(fake);
        let secrets = InMemorySecretResolver::new();
        let config = json!({
            "from": "a@b.test", "to": ["not an address"], "subject": "s", "text": "t",
        });
        let outcome = run(&handler, &config, &json!({}), &secrets, 1).await;
        match outcome {
            NodeOutcome::Fail { code, .. } => assert_eq!(code, "bpm_integration.mail_permanent"),
            other => panic!("expected fail, got {other:?}"),
        }
    }

    #[test]
    fn the_kind_is_mail() {
        let handler = MailNodeHandler::new(Arc::new(FakeMail::new()));
        assert_eq!(handler.kind(), IntegrationKind::Mail);
    }
}
