//! The integration-node contract: the context a handler reads, the handler trait, and the
//! shared decision of retry-versus-error-branch.
//!
//! An integration handler is asynchronous — it calls the outside world through its ports — and
//! reports the engine's [`NodeOutcome`]: `Done` with the node's output, `Retry` with the instant
//! to try again, or `Fail` for the error branch. It keys every side effect by
//! [`IntegrationCtx::key`], the `(case, node, attempt)` idempotency key, so a replay after a
//! crash is harmless and a retry reuses the same key (the HTTP handler sends it as
//! `Idempotency-Key`).
//!
//! Time is an argument ([`IntegrationCtx::now`]); no handler reads a clock. A handler never
//! stores or prints a credential: it asks [`IntegrationCtx::resolve`] for the value, which
//! registers it for redaction before returning it.

use async_trait::async_trait;
use serde_json::Value as Json;
use spot_app_api::prelude::OffsetDateTime;
use spot_bpm_engine::{CaseId, IdempotencyKey, NodeOutcome};
use spot_dag::NodeId;
use spot_secrets::SecretString;

use crate::config::{RetryDecision, RetryPolicy};
use crate::credential::{CredentialRef, SecretResolver};
use crate::error::{IntegrationError, RetryClass};
use crate::mapping::{Mapping, mapping_context};
use crate::registry::SecretRegistry;

/// Which integration node a handler runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IntegrationKind {
    /// An HTTP call.
    Http,
    /// An e-mail.
    Mail,
    /// An outgoing webhook.
    Webhook,
    /// A TypeScript code node.
    Code,
    /// A WASM connector call.
    Connector,
}

impl IntegrationKind {
    /// The token for logs and records.
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Mail => "mail",
            Self::Webhook => "webhook",
            Self::Code => "code",
            Self::Connector => "connector",
        }
    }
}

/// Everything a handler reads and may change for one attempt.
///
/// The engine owns the Case's variables and hands them in mutably: a handler that maps data
/// writes into `variables`. The [`IntegrationCtx::key`] is the idempotency key a side-effecting
/// handler must use; the [`IntegrationCtx::registry`] is where it registers every credential it
/// opens so nothing observable carries a value.
pub struct IntegrationCtx<'a> {
    /// The Case being run.
    pub case: &'a CaseId,
    /// The node being run.
    pub node: &'a NodeId,
    /// The engine attempt number (1 for the first try).
    pub attempt: u32,
    /// The `(case, node, attempt)` idempotency key.
    pub key: IdempotencyKey,
    /// Whether this run replays a started-but-unfinished attempt after a crash.
    pub is_retry: bool,
    /// The instant the engine was stepped at; the only clock a handler sees.
    pub now: OffsetDateTime,
    /// The principal the node runs as, when the Case carries one.
    pub principal: Option<&'a str>,
    /// The node's input, as the engine resolved it.
    pub input: &'a Json,
    /// The previous nodes' outputs, keyed by node id.
    pub node_outputs: &'a Json,
    /// The Case's variables; a handler may write mapped values here.
    pub variables: &'a mut Json,
    /// The node's config, as compiled from the Process model.
    pub config: &'a Json,
    /// The vault that opens a credential reference at call time.
    pub secrets: &'a dyn SecretResolver,
    /// The registry that scrubs every opened credential from anything observable.
    pub registry: &'a mut SecretRegistry,
}

impl IntegrationCtx<'_> {
    /// Opens a credential and registers its value for redaction.
    ///
    /// # Errors
    ///
    /// Whatever the resolver reports: [`IntegrationError::CredentialMissing`] or
    /// [`IntegrationError::SecretResolveFailed`].
    pub async fn resolve(
        &mut self,
        reference: &CredentialRef,
    ) -> Result<SecretString, IntegrationError> {
        let secret = self.secrets.resolve(reference).await?;
        self.registry.register(reference, &secret);
        Ok(secret)
    }

    /// The mapping context: the node's `input`, the Case's `case` variables and the previous
    /// nodes' outputs under `nodes`.
    #[must_use]
    pub fn mapping_context(&self) -> Json {
        mapping_context(self.input, self.variables, self.node_outputs)
    }

    /// Redacts a reason string so it can be logged or stored in the execution history.
    #[must_use]
    pub fn redact_reason(&self, reason: &str) -> String {
        self.registry.redact_text(reason)
    }

    /// Redacts a JSON value in place, so a credential an endpoint echoed back never flows into
    /// the node's output, the Case's variables or the history.
    pub fn redact_json(&self, value: &mut Json) {
        self.registry.redact_json(value);
    }

    /// The decision for a failure: retry a transient class within the policy, otherwise take the
    /// error branch. The reason is redacted.
    #[must_use]
    pub fn classify_failure(&self, error: &IntegrationError, retry: &RetryPolicy) -> NodeOutcome {
        let reason = self.redact_reason(&error.to_string());
        match error.retry_class() {
            RetryClass::Transient => self.retry_or_fail(error.code(), &reason, retry),
            RetryClass::Permanent => NodeOutcome::Fail {
                code: error.code().to_owned(),
                reason,
            },
        }
    }

    /// Retries `code` within the policy, or fails when the attempts are exhausted.
    #[must_use]
    pub fn retry_or_fail(&self, code: &str, reason: &str, retry: &RetryPolicy) -> NodeOutcome {
        match retry.decide(self.attempt) {
            RetryDecision::Retry { backoff_secs } => {
                let backoff = i64::try_from(backoff_secs).unwrap_or(i64::MAX);
                NodeOutcome::Retry {
                    after: self.now + time::Duration::seconds(backoff),
                }
            }
            RetryDecision::GiveUp => NodeOutcome::Fail {
                code: code.to_owned(),
                reason: reason.to_owned(),
            },
        }
    }

    /// Applies an optional mapping to the context, returning an empty object when there is none.
    ///
    /// # Errors
    ///
    /// [`IntegrationError::MappingEvaluate`] when an expression does not evaluate.
    pub fn apply_mapping(&self, mapping: Option<&Mapping>) -> Result<Json, IntegrationError> {
        match mapping {
            Some(mapping) => mapping.apply(&self.mapping_context(), self.now),
            None => Ok(Json::Object(serde_json::Map::new())),
        }
    }

    /// A deterministic seed for a code node's `Math.random()`, from the idempotency key: the same
    /// attempt gets the same sequence, so a replay is identical.
    #[must_use]
    pub fn deterministic_seed(&self) -> u64 {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in self.key.to_string().bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash
    }
}

/// One integration node handler: asynchronous, side-effecting and keyed by the idempotency key.
#[async_trait]
pub trait IntegrationHandler: Send + Sync {
    /// The node kind this handler runs.
    fn kind(&self) -> IntegrationKind;

    /// Runs one attempt and reports its outcome.
    async fn run(&self, ctx: &mut IntegrationCtx<'_>) -> NodeOutcome;
}

/// A handler registry keyed by [`IntegrationKind`], mirroring the engine's handler registry.
#[derive(Default)]
pub struct IntegrationRegistry {
    handlers: std::collections::BTreeMap<IntegrationKind, std::sync::Arc<dyn IntegrationHandler>>,
}

impl IntegrationRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a handler, replacing any handler already registered for its kind.
    pub fn register(&mut self, handler: impl IntegrationHandler + 'static) -> &mut Self {
        self.handlers
            .insert(handler.kind(), std::sync::Arc::new(handler));
        self
    }

    /// Registers a boxed handler.
    pub fn register_arc(&mut self, handler: std::sync::Arc<dyn IntegrationHandler>) -> &mut Self {
        self.handlers.insert(handler.kind(), handler);
        self
    }

    /// The handler for a kind, when one is registered.
    #[must_use]
    pub fn get(&self, kind: IntegrationKind) -> Option<std::sync::Arc<dyn IntegrationHandler>> {
        self.handlers.get(&kind).cloned()
    }

    /// The kinds that have a handler.
    #[must_use]
    pub fn kinds(&self) -> Vec<IntegrationKind> {
        self.handlers.keys().copied().collect()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::credential::InMemorySecretResolver;
    use crate::registry::SecretRegistry;
    use serde_json::json;
    use spot_bpm_engine::CaseId;
    use spot_dag::NodeId;

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
    }

    fn with_ctx<R>(run: impl FnOnce(&mut IntegrationCtx<'_>) -> R) -> R {
        let case = CaseId::new("case1").unwrap();
        let node = NodeId::new("n1").unwrap();
        let mut variables = json!({});
        let input = json!({ "a": 1 });
        let outputs = json!({});
        let config = json!({});
        let secrets = InMemorySecretResolver::new();
        let mut registry = SecretRegistry::new().unwrap();
        let mut ctx = IntegrationCtx {
            case: &case,
            node: &node,
            attempt: 1,
            key: IdempotencyKey::new(case.clone(), node.clone(), 1),
            is_retry: false,
            now: now(),
            principal: None,
            input: &input,
            node_outputs: &outputs,
            variables: &mut variables,
            config: &config,
            secrets: &secrets,
            registry: &mut registry,
        };
        run(&mut ctx)
    }

    #[test]
    fn a_transient_failure_retries_within_the_policy() {
        let retry = RetryPolicy {
            max_attempts: 3,
            backoff_secs: 10,
        };
        let outcome = with_ctx(|ctx| {
            ctx.classify_failure(
                &IntegrationError::TransportFailed {
                    detail: "reset".to_owned(),
                },
                &retry,
            )
        });
        match outcome {
            NodeOutcome::Retry { after } => {
                assert_eq!((after - now()).whole_seconds(), 10);
            }
            other => panic!("expected a retry, got {other:?}"),
        }
    }

    #[test]
    fn an_exhausted_transient_failure_takes_the_error_branch() {
        let retry = RetryPolicy {
            max_attempts: 1,
            backoff_secs: 10,
        };
        let outcome = with_ctx(|ctx| {
            ctx.classify_failure(
                &IntegrationError::TransportFailed {
                    detail: "reset".to_owned(),
                },
                &retry,
            )
        });
        assert!(matches!(outcome, NodeOutcome::Fail { .. }));
    }

    #[test]
    fn a_permanent_failure_takes_the_error_branch_at_once() {
        let retry = RetryPolicy {
            max_attempts: 9,
            backoff_secs: 1,
        };
        let outcome = with_ctx(|ctx| {
            ctx.classify_failure(
                &IntegrationError::EgressDenied {
                    code: "egress.denied_by_policy".to_owned(),
                },
                &retry,
            )
        });
        assert!(matches!(outcome, NodeOutcome::Fail { .. }));
    }

    #[test]
    fn the_seed_is_deterministic() {
        let first = with_ctx(|ctx| ctx.deterministic_seed());
        let second = with_ctx(|ctx| ctx.deterministic_seed());
        assert_eq!(first, second);
    }
}
