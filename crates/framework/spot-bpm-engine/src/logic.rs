//! The Logic handlers this crate ships, the Trigger pass-through and the Wait handler.
//!
//! Logic nodes branch, split, join, loop and map data with the shared expression language
//! (ADR-28). A branch reads its outgoing edge conditions and takes the true ones; a parallel
//! split takes every branch; a join is a pass-through whose waiting is the DAG layer's join
//! rule; a loop is a *bounded* loop over the Case's variables (the model forbids cycles, so a
//! loop step is bounded, never a back-edge); a map writes expression results into the variables.
//!
//! Everything here is pure: no clock, no I/O, no randomness. Time is [`NodeCtx::now`].

use serde_json::Value as Json;

use crate::error::EngineResult;
use crate::expr;
use crate::handler::{
    HandlerRegistry, LogicKind, NodeCtx, NodeHandler, NodeKind, NodeOutcome, WaitSpec,
};

/// The handler-token a branch reports when no outgoing condition is true and there is no
/// unconditional edge to fall back on.
const BRANCH_UNMATCHED: &str = "branch_unmatched";

/// The handler-token a map or loop reports when its config is malformed.
const CONFIG_INVALID: &str = "config_invalid";

/// The handler-token a script node reports: its host is WP-BPM-06.
const SCRIPT_UNSUPPORTED: &str = "script_unsupported";

/// Registers every Logic handler this crate ships.
pub fn register(registry: &mut HandlerRegistry) {
    registry.register(BranchHandler);
    registry.register(ParallelHandler);
    registry.register(JoinHandler);
    registry.register(LoopHandler);
    registry.register(MapHandler);
    registry.register(ScriptHandler);
}

/// A conditional branch: takes every outgoing edge whose condition is true.
#[derive(Debug, Clone, Copy, Default)]
pub struct BranchHandler;

impl NodeHandler for BranchHandler {
    fn kind(&self) -> NodeKind {
        NodeKind::Logic(LogicKind::Branch)
    }

    fn run(&self, ctx: &mut NodeCtx<'_>) -> NodeOutcome {
        if ctx.outgoing.is_empty() {
            return NodeOutcome::done();
        }
        let mut branches = Vec::new();
        let mut had_unconditional = false;
        for edge in ctx.outgoing {
            if edge.error {
                continue;
            }
            match &edge.condition {
                None => had_unconditional = true,
                Some(condition) => match expr::evaluate_bool(condition, ctx.variables, ctx.now) {
                    Ok(true) => branches.push(condition.clone()),
                    Ok(false) => {}
                    Err(detail) => {
                        return NodeOutcome::fail(
                            BRANCH_UNMATCHED,
                            format!("the condition `{condition}` could not be evaluated: {detail}"),
                        );
                    }
                },
            }
        }
        if branches.is_empty() && !had_unconditional {
            return NodeOutcome::fail(
                BRANCH_UNMATCHED,
                "no outgoing condition held and the node has no unconditional edge",
            );
        }
        NodeOutcome::Done {
            output: None,
            branches,
        }
    }
}

/// A parallel split: takes every outgoing branch.
#[derive(Debug, Clone, Copy, Default)]
pub struct ParallelHandler;

impl NodeHandler for ParallelHandler {
    fn kind(&self) -> NodeKind {
        NodeKind::Logic(LogicKind::Parallel)
    }

    fn run(&self, ctx: &mut NodeCtx<'_>) -> NodeOutcome {
        // The config may name the branches explicitly; otherwise every conditional outgoing
        // edge is taken. Unconditional edges are always taken by the DAG layer.
        let branches =
            config_strings(ctx.config, "branches").unwrap_or_else(|| ctx.outgoing_conditions());
        NodeOutcome::Done {
            output: None,
            branches,
        }
    }
}

/// A join of parallel paths: the DAG layer's join rule does the waiting, so it is a
/// pass-through.
#[derive(Debug, Clone, Copy, Default)]
pub struct JoinHandler;

impl NodeHandler for JoinHandler {
    fn kind(&self) -> NodeKind {
        NodeKind::Logic(LogicKind::Join)
    }

    fn run(&self, _ctx: &mut NodeCtx<'_>) -> NodeOutcome {
        NodeOutcome::done()
    }
}

/// A bounded loop: applies its body mapping while its condition holds, up to a maximum.
#[derive(Debug, Clone, Copy, Default)]
pub struct LoopHandler;

impl NodeHandler for LoopHandler {
    fn kind(&self) -> NodeKind {
        NodeKind::Logic(LogicKind::Loop)
    }

    fn run(&self, ctx: &mut NodeCtx<'_>) -> NodeOutcome {
        let max = config_u64(ctx.config, "max_iterations")
            .unwrap_or(1)
            .min(10_000);
        let condition = config_string(ctx.config, "condition");
        let body = config_object(ctx.config, "body");
        let mut iterations: u64 = 0;
        loop {
            if iterations >= max {
                break;
            }
            if let Some(condition) = &condition {
                match expr::evaluate_bool(condition, ctx.variables, ctx.now) {
                    Ok(true) => {}
                    Ok(false) => break,
                    Err(detail) => {
                        return NodeOutcome::fail(
                            CONFIG_INVALID,
                            format!("the loop condition `{condition}` is invalid: {detail}"),
                        );
                    }
                }
            }
            if let Some(body) = body
                && let Err(detail) = apply_mapping(ctx, body)
            {
                return NodeOutcome::fail(CONFIG_INVALID, detail.to_string());
            }
            iterations = iterations.saturating_add(1);
        }
        NodeOutcome::Done {
            output: Some(Json::from(iterations)),
            branches: config_strings(ctx.config, "branches").unwrap_or_default(),
        }
    }
}

/// Data mapping: evaluates each expression and writes the result into the Case's variables.
#[derive(Debug, Clone, Copy, Default)]
pub struct MapHandler;

impl NodeHandler for MapHandler {
    fn kind(&self) -> NodeKind {
        NodeKind::Logic(LogicKind::Map)
    }

    fn run(&self, ctx: &mut NodeCtx<'_>) -> NodeOutcome {
        let Some(mapping) = config_object(ctx.config, "mapping") else {
            return NodeOutcome::fail(CONFIG_INVALID, "a map node needs a `mapping` object");
        };
        match apply_mapping(ctx, mapping) {
            Ok(mapped) => NodeOutcome::Done {
                output: Some(mapped),
                branches: Vec::new(),
            },
            Err(detail) => NodeOutcome::fail(CONFIG_INVALID, detail.to_string()),
        }
    }
}

/// A TypeScript code node: its host is WP-BPM-06, so this engine refuses it explicitly.
#[derive(Debug, Clone, Copy, Default)]
pub struct ScriptHandler;

impl NodeHandler for ScriptHandler {
    fn kind(&self) -> NodeKind {
        NodeKind::Logic(LogicKind::Script)
    }

    fn run(&self, ctx: &mut NodeCtx<'_>) -> NodeOutcome {
        NodeOutcome::fail(
            SCRIPT_UNSUPPORTED,
            format!("the script node `{}` needs the TypeScript host", ctx.node),
        )
    }
}

/// The Wait handler: turns the node's config into a [`WaitSpec`].
#[derive(Debug, Clone, Copy, Default)]
pub struct WaitHandler;

impl NodeHandler for WaitHandler {
    fn kind(&self) -> NodeKind {
        NodeKind::Wait
    }

    fn run(&self, ctx: &mut NodeCtx<'_>) -> NodeOutcome {
        match config_string(ctx.config, "wait").as_deref() {
            Some("duration") | None => match config_u64(ctx.config, "seconds") {
                Some(seconds) => {
                    let at = ctx.now + time_span_seconds(seconds);
                    NodeOutcome::Wait(WaitSpec::Until { at })
                }
                None => NodeOutcome::fail(CONFIG_INVALID, "a duration wait needs `seconds`"),
            },
            Some("date") => match config_string(ctx.config, "until") {
                Some(text) => match parse_instant(&text) {
                    Some(at) => NodeOutcome::Wait(WaitSpec::Until { at }),
                    None => NodeOutcome::fail(
                        CONFIG_INVALID,
                        format!("the date `{text}` is not an instant"),
                    ),
                },
                None => NodeOutcome::fail(CONFIG_INVALID, "a date wait needs `until`"),
            },
            Some("event") => match config_string(ctx.config, "event") {
                Some(key) => NodeOutcome::Wait(WaitSpec::ForEvent { key, due: None }),
                None => NodeOutcome::fail(CONFIG_INVALID, "an event wait needs `event`"),
            },
            Some(other) => {
                NodeOutcome::fail(CONFIG_INVALID, format!("`{other}` is not a wait type"))
            }
        }
    }
}

/// Applies a mapping of `target path -> expression`, writing each result into the variables and
/// returning the mapped object.
fn apply_mapping(
    ctx: &mut NodeCtx<'_>,
    mapping: &serde_json::Map<String, Json>,
) -> EngineResult<Json> {
    let mut mapped = serde_json::Map::new();
    for (target, expression) in mapping {
        let Some(expression) = expression.as_str() else {
            return Err(crate::error::EngineError::ConfigInvalid {
                node: ctx.node.to_string(),
                detail: format!("the mapping target `{target}` needs a string expression"),
            });
        };
        let value = expr::evaluate(expression, ctx.variables, ctx.now).map_err(|detail| {
            crate::error::EngineError::ExpressionInvalid {
                node: ctx.node.to_string(),
                expression: expression.to_owned(),
                detail,
            }
        })?;
        let json = expr::to_json(&value).unwrap_or(Json::Null);
        ctx.set_variable(target, json.clone());
        mapped.insert(target.clone(), json);
    }
    Ok(Json::Object(mapped))
}

/// A string config field.
fn config_string(config: &Json, key: &str) -> Option<String> {
    config.get(key)?.as_str().map(str::to_owned)
}

/// An unsigned config field.
fn config_u64(config: &Json, key: &str) -> Option<u64> {
    config.get(key)?.as_u64()
}

/// An object config field.
fn config_object<'a>(config: &'a Json, key: &str) -> Option<&'a serde_json::Map<String, Json>> {
    config.get(key)?.as_object()
}

/// A list-of-strings config field.
fn config_strings(config: &Json, key: &str) -> Option<Vec<String>> {
    config
        .get(key)?
        .as_array()?
        .iter()
        .map(|item| item.as_str().map(str::to_owned))
        .collect()
}

/// A `std` duration for a whole number of seconds.
fn time_span_seconds(seconds: u64) -> std::time::Duration {
    std::time::Duration::from_secs(seconds)
}

/// Parses an instant from `YYYY-MM-DD`, `YYYY-MM-DDTHH:MM:SSZ`, or whole Unix seconds.
fn parse_instant(text: &str) -> Option<spot_app_api::prelude::OffsetDateTime> {
    use spot_app_api::prelude::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time};
    if let Ok(seconds) = text.parse::<i64>() {
        return OffsetDateTime::from_unix_timestamp(seconds).ok();
    }
    let (date_text, time_text) = match text.split_once(['T', ' ']) {
        Some((date, time)) => (date, Some(time.trim_end_matches('Z'))),
        None => (text, None),
    };
    let mut parts = date_text.split('-');
    let year: i32 = parts.next()?.parse().ok()?;
    let month: u8 = parts.next()?.parse().ok()?;
    let day: u8 = parts.next()?.parse().ok()?;
    let date = Date::from_calendar_date(year, Month::try_from(month).ok()?, day).ok()?;
    let time = match time_text {
        Some(text) => {
            let mut parts = text.split(':');
            let hour: u8 = parts.next()?.parse().ok()?;
            let minute: u8 = parts.next()?.parse().ok()?;
            let second: u8 = parts.next().unwrap_or("0").parse().ok()?;
            Time::from_hms(hour, minute, second).ok()?
        }
        None => Time::MIDNIGHT,
    };
    Some(PrimitiveDateTime::new(date, time).assume_utc())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use serde_json::json;
    use spot_app_api::prelude::OffsetDateTime;
    use spot_dag::NodeId;

    use super::*;
    use crate::handler::{CaseId, IdempotencyKey, OutgoingEdge};

    fn now() -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
    }

    /// Runs one handler against a small context, returning its outcome and the final variables.
    fn run(
        handler: &dyn NodeHandler,
        config: &Json,
        outgoing: &[OutgoingEdge],
        variables: &mut Json,
    ) -> NodeOutcome {
        let case = CaseId::new("case1").unwrap();
        let node = NodeId::new("n1").unwrap();
        let mut ctx = NodeCtx {
            case: &case,
            node: &node,
            kind: handler.kind(),
            attempt: 1,
            key: IdempotencyKey::new(case.clone(), node.clone(), 1),
            is_retry: false,
            now: now(),
            deadline: None,
            principal: None,
            config,
            outgoing,
            variables,
            cancelled: false,
        };
        handler.run(&mut ctx)
    }

    fn edge(condition: &str) -> OutgoingEdge {
        OutgoingEdge {
            to: NodeId::new("next").unwrap(),
            condition: Some(condition.to_owned()),
            error: false,
        }
    }

    #[test]
    fn a_branch_takes_the_true_conditions() {
        let config = json!({});
        let mut variables = json!({ "ok": true });
        let outcome = run(
            &BranchHandler,
            &config,
            &[edge("doc.ok == true"), edge("doc.ok == false")],
            &mut variables,
        );
        match outcome {
            NodeOutcome::Done { branches, .. } => {
                assert_eq!(branches, vec!["doc.ok == true".to_owned()]);
            }
            other => panic!("expected a done outcome, got {other:?}"),
        }
    }

    #[test]
    fn a_branch_with_no_match_and_no_default_fails() {
        let config = json!({});
        let mut variables = json!({ "ok": true });
        let outcome = run(
            &BranchHandler,
            &config,
            &[edge("doc.ok == false")],
            &mut variables,
        );
        assert!(matches!(outcome, NodeOutcome::Fail { .. }));
    }

    #[test]
    fn a_map_writes_mapped_values() {
        let config = json!({ "mapping": { "total": "doc.qty * 2" } });
        let mut variables = json!({ "qty": 3 });
        let outcome = run(&MapHandler, &config, &[], &mut variables);
        assert!(matches!(outcome, NodeOutcome::Done { .. }));
        assert_eq!(variables["total"], json!(6));
    }

    #[test]
    fn a_loop_is_bounded() {
        let config = json!({
            "max_iterations": 4,
            "condition": "doc.count < 10",
            "body": { "count": "doc.count + 1" }
        });
        let mut variables = json!({ "count": 0 });
        let outcome = run(&LoopHandler, &config, &[], &mut variables);
        assert!(matches!(outcome, NodeOutcome::Done { .. }));
        assert_eq!(variables["count"], json!(4));
    }

    #[test]
    fn a_duration_wait_becomes_a_due_instant() {
        let config = json!({ "wait": "duration", "seconds": 30 });
        let mut variables = json!({});
        let outcome = run(&WaitHandler, &config, &[], &mut variables);
        match outcome {
            NodeOutcome::Wait(WaitSpec::Until { at }) => {
                assert_eq!((at - now()).whole_seconds(), 30);
            }
            other => panic!("expected a wait, got {other:?}"),
        }
    }

    #[test]
    fn an_event_wait_carries_its_key() {
        let config = json!({ "wait": "event", "event": "paid" });
        let mut variables = json!({});
        match run(&WaitHandler, &config, &[], &mut variables) {
            NodeOutcome::Wait(spec) => assert_eq!(spec.key(), Some("paid")),
            other => panic!("expected a wait, got {other:?}"),
        }
    }

    #[test]
    fn instants_parse() {
        assert_eq!(
            parse_instant("2023-11-14T22:13:20Z"),
            OffsetDateTime::from_unix_timestamp(1_700_000_000).ok()
        );
        assert!(parse_instant("2023-11-14").is_some());
        assert!(parse_instant("1700000000").is_some());
        assert!(parse_instant("not-a-date").is_none());
    }

    #[test]
    fn a_script_node_is_refused() {
        let config = json!({ "script": "return 1;" });
        let mut variables = json!({});
        let outcome = run(&ScriptHandler, &config, &[], &mut variables);
        assert!(matches!(outcome, NodeOutcome::Fail { .. }));
    }
}
