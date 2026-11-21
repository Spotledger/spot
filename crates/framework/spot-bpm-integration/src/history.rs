//! The execution history: what one node attempt read and produced, after redaction.
//!
//! The engine persists an attempt's outcome; the designer (WP-BPM-07) shows each node's inputs
//! and outputs. This type is what a host records for that view, and it stores **redacted** copies
//! only: [`ExecutionHistory::record`] runs the node's input, its output and its failure reason
//! through [`crate::registry::redact`] before they are kept, so a credential that ever reached an
//! input or a response body is scrubbed here even if a handler missed it.
//!
//! The record carries the node id, the attempt, the node kind and the outcome state; never a
//! resolved secret.

use serde_json::Value as Json;
use spot_bpm_engine::NodeOutcome;

use crate::handler::{IntegrationCtx, IntegrationKind};
use crate::registry::redact;

/// The state of one recorded attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordState {
    /// The attempt succeeded.
    Done,
    /// The attempt asked to be retried.
    Retry,
    /// The attempt failed and took the error branch.
    Fail,
    /// The attempt paused.
    Wait,
}

impl RecordState {
    /// The state's token.
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Retry => "retry",
            Self::Fail => "fail",
            Self::Wait => "wait",
        }
    }

    /// The state of an outcome.
    #[must_use]
    pub fn of(outcome: &NodeOutcome) -> Self {
        match outcome {
            NodeOutcome::Done { .. } => Self::Done,
            NodeOutcome::Retry { .. } => Self::Retry,
            NodeOutcome::Fail { .. } => Self::Fail,
            NodeOutcome::Wait(_) => Self::Wait,
        }
    }
}

/// One node attempt as the history keeps it: redacted input and output, and a state.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeRecord {
    /// The node id.
    pub node: String,
    /// The engine attempt number.
    pub attempt: u32,
    /// The node kind.
    pub kind: IntegrationKind,
    /// The outcome state.
    pub state: RecordState,
    /// The node's input, redacted.
    pub input: Json,
    /// The node's output, redacted (`null` when there is none).
    pub output: Json,
    /// The failure reason, redacted, when the attempt failed.
    pub error: Option<String>,
}

/// The history of one Case's integration attempts.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ExecutionHistory {
    records: Vec<NodeRecord>,
}

impl ExecutionHistory {
    /// An empty history.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one attempt, redacting its input, output and error first.
    pub fn record(
        &mut self,
        ctx: &IntegrationCtx<'_>,
        kind: IntegrationKind,
        outcome: &NodeOutcome,
    ) {
        let mut input = ctx.input.clone();
        redact(&mut input, ctx.registry);

        let (output, error) = match outcome {
            NodeOutcome::Done { output, .. } => {
                let mut value = output.clone().unwrap_or(Json::Null);
                redact(&mut value, ctx.registry);
                (value, None)
            }
            NodeOutcome::Fail { reason, .. } => {
                (Json::Null, Some(ctx.registry.redact_text(reason)))
            }
            NodeOutcome::Retry { .. } | NodeOutcome::Wait(_) => (Json::Null, None),
        };

        self.records.push(NodeRecord {
            node: ctx.node.to_string(),
            attempt: ctx.attempt,
            kind,
            state: RecordState::of(outcome),
            input,
            output,
            error,
        });
    }

    /// The records, oldest first.
    #[must_use]
    pub fn records(&self) -> &[NodeRecord] {
        &self.records
    }

    /// How many attempts are recorded.
    #[must_use]
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Whether nothing is recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// The history as JSON, for a designer's execution view.
    #[must_use]
    pub fn as_json(&self) -> Json {
        Json::Array(
            self.records
                .iter()
                .map(|record| {
                    Json::Object(serde_json::Map::from_iter([
                        (String::from("node"), Json::String(record.node.clone())),
                        (String::from("attempt"), Json::from(record.attempt)),
                        (
                            String::from("kind"),
                            Json::String(record.kind.token().to_owned()),
                        ),
                        (
                            String::from("state"),
                            Json::String(record.state.token().to_owned()),
                        ),
                        (String::from("input"), record.input.clone()),
                        (String::from("output"), record.output.clone()),
                        (
                            String::from("error"),
                            record.error.clone().map_or(Json::Null, Json::String),
                        ),
                    ]))
                })
                .collect(),
        )
    }
}

/// Redacts a reason string, keeping it printable when the registry could not match a value.
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::credential::InMemorySecretResolver;
    use crate::registry::SecretRegistry;
    use serde_json::json;
    use spot_app_api::prelude::OffsetDateTime;
    use spot_bpm_engine::{CaseId, IdempotencyKey};
    use spot_dag::NodeId;

    const CANARY: &str = "canary-history-secret-0123456789";

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
    }

    #[test]
    fn recorded_input_and_output_are_redacted() {
        let case = CaseId::new("case1").unwrap();
        let node = NodeId::new("n1").unwrap();
        let input = json!({ "token": CANARY });
        let outputs = json!({});
        let mut variables = json!({});
        let secrets = InMemorySecretResolver::new();
        let mut registry = SecretRegistry::new().unwrap();
        let reference = crate::credential::CredentialRef::new("api");
        registry.register(&reference, &spot_secrets::SecretString::new(CANARY));
        let ctx = IntegrationCtx {
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
            config: &json!({}),
            secrets: &secrets,
            registry: &mut registry,
        };
        let outcome = NodeOutcome::Done {
            output: Some(json!({ "echo": CANARY })),
            branches: Vec::new(),
        };
        let mut history = ExecutionHistory::new();
        history.record(&ctx, crate::handler::IntegrationKind::Http, &outcome);
        let text = history.as_json().to_string();
        assert!(
            !text.contains(CANARY),
            "the history leaked a secret: {text}"
        );
        assert_eq!(history.len(), 1);
        assert_eq!(history.records()[0].state, RecordState::Done);
    }

    #[test]
    fn a_failure_reason_is_redacted() {
        let case = CaseId::new("case1").unwrap();
        let node = NodeId::new("n1").unwrap();
        let input = json!({});
        let outputs = json!({});
        let mut variables = json!({});
        let secrets = InMemorySecretResolver::new();
        let mut registry = SecretRegistry::new().unwrap();
        let reference = crate::credential::CredentialRef::new("api");
        registry.register(&reference, &spot_secrets::SecretString::new(CANARY));
        let ctx = IntegrationCtx {
            case: &case,
            node: &node,
            attempt: 2,
            key: IdempotencyKey::new(case.clone(), node.clone(), 2),
            is_retry: true,
            now: now(),
            principal: None,
            input: &input,
            node_outputs: &outputs,
            variables: &mut variables,
            config: &json!({}),
            secrets: &secrets,
            registry: &mut registry,
        };
        let outcome = NodeOutcome::Fail {
            code: String::from("bpm_integration.http_status"),
            reason: format!("HTTP 500 for {CANARY}"),
        };
        let mut history = ExecutionHistory::new();
        history.record(&ctx, crate::handler::IntegrationKind::Http, &outcome);
        let record = &history.records()[0];
        assert_eq!(record.state, RecordState::Fail);
        assert!(!record.error.as_deref().unwrap().contains(CANARY));
    }
}
