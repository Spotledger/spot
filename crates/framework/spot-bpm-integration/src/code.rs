//! The TypeScript code node: a script runs on `spot-ts`' embedded engine, under its time, memory,
//! instruction and output limits, with no network and no host functions.
//!
//! The script is an ES module whose exported entry point takes one JSON argument and returns one
//! JSON value; the node maps the Case's data into that argument and the result back out with the
//! shared expression language. The engine gives each execution its own runtime and context, and
//! takes away `eval`, the `Function` constructor, timers and every network primitive. `Date.now()`
//! and `Math.random()` answer from the invocation: the engine's time ([`IntegrationCtx::now`]) and
//! a seed derived from the idempotency key, so a replay of one attempt is identical.
//!
//! A limit the engine enforces — a timeout, the instruction budget, memory, the stack, host calls
//! or the output size — is reported as [`IntegrationError::ScriptLimit`]; every other script
//! failure is [`IntegrationError::ScriptFailed`] with the engine's stable `script.*` code.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value as Json;
use spot_bpm_engine::NodeOutcome;
use spot_ts::{HostBridge, HostError, ScriptEngine, ScriptError, ScriptId};

use crate::config::CodeNodeConfig;
use crate::error::IntegrationError;
use crate::handler::{IntegrationCtx, IntegrationHandler, IntegrationKind};

/// The app name a code node's scripts are cached under.
const SCRIPT_APP: &str = "bpm";

/// Runs TypeScript code nodes on a script engine.
pub struct CodeNodeHandler {
    engine: Arc<ScriptEngine>,
}

impl CodeNodeHandler {
    /// A handler over `engine`.
    #[must_use]
    pub fn new(engine: Arc<ScriptEngine>) -> Self {
        Self { engine }
    }
}

#[async_trait]
impl IntegrationHandler for CodeNodeHandler {
    fn kind(&self) -> IntegrationKind {
        IntegrationKind::Code
    }

    async fn run(&self, ctx: &mut IntegrationCtx<'_>) -> NodeOutcome {
        let config = match CodeNodeConfig::from_json(&ctx.node.to_string(), ctx.config) {
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
            Err(error) => ctx.classify_failure(&error, &crate::config::RetryPolicy::default()),
        }
    }
}

impl CodeNodeHandler {
    /// Compiles and runs the script, returning its mapped result.
    async fn call(
        &self,
        ctx: &mut IntegrationCtx<'_>,
        config: &CodeNodeConfig,
    ) -> Result<Json, IntegrationError> {
        let id = ScriptId::new(SCRIPT_APP, &ctx.node.to_string(), 1);
        let compiled = self
            .engine
            .compile(&config.script, id, None)
            .map_err(|e| script_error(&ctx.node.to_string(), &e))?;

        let context = ctx.mapping_context();
        let args = match &config.input {
            Some(mapping) => mapping.apply(&context, ctx.now)?,
            None => ctx.input.clone(),
        };

        let bridge = Arc::new(CodeBridge {
            now_millis: millis(ctx.now),
            seed: ctx.deterministic_seed(),
        });
        let result = self
            .engine
            .run(
                &compiled,
                &config.entry,
                &args,
                bridge,
                config.limits.to_limits(),
            )
            .await
            .map_err(|e| script_error(&ctx.node.to_string(), &e))?;

        tracing::debug!(target: "bpm_integration", node = %ctx.node, attempt = ctx.attempt, "code run");
        let mut output = match &config.output {
            Some(mapping) => {
                let mut extended = context;
                if let Json::Object(map) = &mut extended {
                    map.insert(String::from("result"), result);
                }
                mapping.apply(&extended, ctx.now)?
            }
            None => result,
        };
        ctx.redact_json(&mut output);
        Ok(output)
    }
}

/// The host bridge of a code node: no host functions, and a deterministic time and seed.
struct CodeBridge {
    now_millis: i64,
    seed: u64,
}

impl HostBridge for CodeBridge {
    fn call(&self, name: &str, _args: Json) -> Result<Json, HostError> {
        Err(HostError::new(
            "unavailable",
            &format!("the code node exposes no host function `{name}`"),
        ))
    }

    fn now_millis(&self) -> i64 {
        self.now_millis
    }

    fn random_seed(&self) -> u64 {
        self.seed
    }
}

/// The engine's instant, in milliseconds since the Unix epoch.
fn millis(at: spot_app_api::prelude::OffsetDateTime) -> i64 {
    i64::try_from(at.unix_timestamp_nanos() / 1_000_000).unwrap_or(0)
}

/// Maps a script failure onto a node error: a limit is [`IntegrationError::ScriptLimit`], anything
/// else is [`IntegrationError::ScriptFailed`] with the engine's code.
fn script_error(node: &str, error: &ScriptError) -> IntegrationError {
    let detail = error.to_string();
    match error {
        ScriptError::Timeout { .. }
        | ScriptError::OutOfBudget { .. }
        | ScriptError::MemoryLimit { .. }
        | ScriptError::StackOverflow
        | ScriptError::TooManyHostCalls { .. }
        | ScriptError::OutputTooLarge { .. } => IntegrationError::ScriptLimit {
            node: node.to_owned(),
            detail,
        },
        other => IntegrationError::ScriptFailed {
            node: node.to_owned(),
            code: other.code().to_owned(),
        },
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::config::CodeLimits;
    use crate::credential::InMemorySecretResolver;
    use crate::handler::{IntegrationCtx, IntegrationKind};
    use crate::registry::SecretRegistry;
    use serde_json::json;
    use spot_app_api::prelude::OffsetDateTime;
    use spot_bpm_engine::{CaseId, IdempotencyKey};
    use spot_dag::NodeId;
    use spot_ts::{EngineConfig, ScriptEngine};

    fn engine() -> Arc<ScriptEngine> {
        Arc::new(ScriptEngine::new(EngineConfig::default()))
    }

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
    }

    async fn run(handler: &CodeNodeHandler, config: &Json, input: &Json) -> NodeOutcome {
        let case = CaseId::new("case1").unwrap();
        let node = NodeId::new("n1").unwrap();
        let outputs = json!({});
        let mut variables = json!({});
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
            input,
            node_outputs: &outputs,
            variables: &mut variables,
            config,
            secrets: &secrets,
            registry: &mut registry,
        };
        handler.run(&mut ctx).await
    }

    #[tokio::test]
    async fn a_script_maps_input_to_output() {
        let handler = CodeNodeHandler::new(engine());
        let config = json!({
            "script": "export function main(args) { return { total: args.qty * 2 }; }",
            "input": { "qty": "doc.input.qty" },
            "output": { "doubled": "doc.result.total" },
        });
        let outcome = run(&handler, &config, &json!({ "qty": 21 })).await;
        match outcome {
            NodeOutcome::Done { output, .. } => {
                assert_eq!(output, Some(json!({ "doubled": 42 })));
            }
            other => panic!("expected done, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_timeout_is_a_limit_failure() {
        let handler = CodeNodeHandler::new(engine());
        let config = json!({
            "script": "export function main() { while (true) {} }",
            "limits": { "wall_clock_ms": 100, "instructions": 100000000 },
        });
        let outcome = run(&handler, &config, &json!({})).await;
        match outcome {
            NodeOutcome::Fail { code, .. } => assert_eq!(code, "bpm_integration.script_limit"),
            other => panic!("expected a limit failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_missing_entry_is_a_script_failure() {
        let handler = CodeNodeHandler::new(engine());
        let config = json!({ "script": "export function other() { return 1; }", "entry": "main" });
        let outcome = run(&handler, &config, &json!({})).await;
        match outcome {
            NodeOutcome::Fail { code, .. } => {
                assert_eq!(code, "bpm_integration.script_failed");
            }
            other => panic!("expected fail, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_compile_error_is_a_script_failure() {
        let handler = CodeNodeHandler::new(engine());
        let config = json!({ "script": "export function main( {" });
        let outcome = run(&handler, &config, &json!({})).await;
        match outcome {
            NodeOutcome::Fail { code, .. } => {
                assert_eq!(code, "bpm_integration.script_failed");
            }
            other => panic!("expected fail, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_script_has_no_network_or_host_functions() {
        let handler = CodeNodeHandler::new(engine());
        let config = json!({
            "script": "export function main() { return { fetch: typeof fetch, host: typeof host, eval_: typeof eval }; }",
        });
        let outcome = run(&handler, &config, &json!({})).await;
        match outcome {
            NodeOutcome::Done { output, .. } => {
                assert_eq!(
                    output,
                    Some(json!({ "fetch": "undefined", "host": "object", "eval_": "function" })),
                    "the host object exists but exposes no functions; eval is replaced by a throwing stub"
                );
            }
            other => panic!("expected done, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_small_instruction_budget_is_a_limit_failure() {
        let handler = CodeNodeHandler::new(engine());
        let config = json!({
            "script": "export function main() { let s = 0; for (let i = 0; i < 1000000; i++) { s += i; } return s; }",
            "limits": { "instructions": 10000, "wall_clock_ms": 5000 },
        });
        let outcome = run(&handler, &config, &json!({})).await;
        match outcome {
            NodeOutcome::Fail { code, .. } => assert_eq!(code, "bpm_integration.script_limit"),
            other => panic!("expected a limit failure, got {other:?}"),
        }
    }

    #[test]
    fn the_limits_convert() {
        let limits = CodeLimits::default().to_limits();
        assert!(limits.wall_clock.as_millis() > 0);
    }

    #[test]
    fn the_kind_is_code() {
        let handler = CodeNodeHandler::new(engine());
        assert_eq!(handler.kind(), IntegrationKind::Code);
    }
}
