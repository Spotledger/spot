#![allow(clippy::unwrap_used)]

//! The optional model driver through `spot-llm-router`, scripted with the mock provider: the same
//! simulator, the same tools and the same metrics as the CI driver. No real model, no network.

use std::collections::BTreeMap;
use std::sync::Arc;

use spot_llm::{
    Message, MockAction, MockProvider, MockStep, Part, Provider, Response, StopReason, Usage,
};
use spot_llm_router::{BudgetBook, MemoryLedger, PriceTable, Route, Router, RoutingTable};
use spot_support_eval::agent::{Agent, LlmAgent};
use spot_support_eval::metrics::{IncidentOutcome, collect};
use spot_support_eval::runner::{Suite, corpus_root, load, load_support_cases};
use spot_support_eval::simulator::{Simulator, evaluation_agent};

fn router_with(steps: Vec<MockStep>) -> Arc<dyn Provider> {
    let mut mock = MockProvider::new();
    for step in steps {
        mock.push_step(step);
    }
    let mock: Arc<dyn Provider> = Arc::new(mock);
    let providers: BTreeMap<String, Arc<dyn Provider>> =
        BTreeMap::from([("mock".to_owned(), mock)]);
    let table = RoutingTable::new(BTreeMap::new())
        .with_chain("support_eval", vec![Route::new("mock", "mock-model")]);
    Arc::new(Router::new(
        providers,
        table,
        Arc::new(MemoryLedger::new()),
        PriceTable::new(),
        BudgetBook::new(),
    ))
}

#[test]
fn the_model_driver_uses_the_same_tool_surface_and_pipeline() {
    let corpus = load(&corpus_root(), Suite::Incidents).unwrap();
    let support = load_support_cases(&corpus_root()).unwrap();
    let incident = corpus.get("job-failed-payment-sync").unwrap();
    let now = 1_760_000_000;

    let steps = vec![
        MockStep::new(
            Some("tenant-atlas:support_eval:job-failed-payment-sync-t0"),
            MockAction::Respond(Response::new(
                Message {
                    role: spot_llm::Role::Assistant,
                    parts: vec![Part::ToolUse {
                        id: "call-1".to_owned(),
                        name: "diagnose".to_owned(),
                        arguments: serde_json::json!({}),
                    }],
                },
                StopReason::ToolUse,
                Usage::ZERO,
            )),
        ),
        MockStep::new(
            Some("tenant-atlas:support_eval:job-failed-payment-sync-t1"),
            MockAction::Respond(Response::new(
                Message {
                    role: spot_llm::Role::Assistant,
                    parts: vec![Part::ToolUse {
                        id: "call-2".to_owned(),
                        name: "apply_remedy".to_owned(),
                        arguments: serde_json::json!({
                            "remedy": "job.retry_failed",
                            "params": {"job_id": "job-1041"}
                        }),
                    }],
                },
                StopReason::ToolUse,
                Usage::ZERO,
            )),
        ),
        MockStep::new(
            Some("tenant-atlas:support_eval:job-failed-payment-sync-t2"),
            MockAction::Respond(Response::new(
                Message::assistant("the job is queued"),
                StopReason::Stop,
                Usage::ZERO,
            )),
        ),
    ];
    let provider = router_with(steps);

    let agent_identity = evaluation_agent("support-eval-llm");
    let mut simulator = Simulator::new(incident, support, &agent_identity, now).unwrap();
    let mut driver = LlmAgent::new(
        provider,
        "mock-model",
        "support-eval-llm",
        simulator.site(),
        &incident.id,
    );
    driver.run(&mut simulator).unwrap();
    let result = collect(incident, &simulator);
    assert_eq!(
        result.outcome,
        IncidentOutcome::Resolved,
        "{:?}",
        result.harm
    );
    assert_eq!(
        simulator.applied().map(|(remedy, _)| remedy.as_str()),
        Some("job.retry_failed")
    );
    assert!(result.harm.is_empty());
    assert!(
        simulator
            .trace()
            .iter()
            .any(|entry| entry.tool == "diagnose" && entry.status.as_str() == "ok")
    );
}

#[test]
fn the_llm_driver_reports_its_kind() {
    let provider = router_with(Vec::new());
    let driver = LlmAgent::new(provider, "mock-model", "eval", "tenant-atlas", "x");
    assert_eq!(driver.kind(), "llm");
    assert_eq!(driver.id(), "eval");
}
