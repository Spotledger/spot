//! The conformance suite: green on the mock scripted from the same fixture, and able to
//! surface a wrong provider.

use spot_llm::conformance::Fixture;
use spot_llm::{LlmError, MockProvider, Response, StopReason};

#[tokio::test]
async fn runs_green_on_the_mock() {
    let fixture = Fixture::sample();
    let provider = MockProvider::from_fixture(&fixture);
    let failures = spot_llm::conformance::run(&provider, &fixture).await;
    assert!(failures.is_empty(), "conformance failures: {failures:?}");
}

#[tokio::test]
async fn the_suite_flags_a_wrong_reply() {
    let fixture = Fixture::sample();
    let provider = MockProvider::from_fixture(&fixture);
    // The oracle moves; the mock still answers with the original reply — the suite must see it.
    let mut wrong = fixture.clone();
    wrong.text.reply = "6".to_owned();
    let failures = spot_llm::conformance::run(&provider, &wrong).await;
    assert!(
        failures.iter().any(|failure| failure.scenario == "text"),
        "the text scenario must flag the divergence: {failures:?}"
    );
}

#[tokio::test]
async fn the_suite_flags_leaky_errors() {
    let fixture = Fixture::sample();
    // A provider (say a thin HTTP adapter) that fails with the request echo in the detail.
    let leaky = MockProvider::new()
        .responds_to(
            &fixture.text.request_id,
            Response::new(
                spot_llm::Message::assistant(fixture.text.reply.clone()),
                StopReason::Stop,
                fixture.text.usage,
            ),
        )
        .responds_to(
            &fixture.tool.request_id,
            Response::new(
                spot_llm::Message {
                    role: spot_llm::Role::Assistant,
                    parts: vec![spot_llm::Part::ToolUse {
                        id: fixture.tool.call_id.clone(),
                        name: fixture.tool.call_name.clone(),
                        arguments: fixture.tool.call_arguments.clone(),
                    }],
                },
                StopReason::ToolUse,
                fixture.tool.usage,
            ),
        )
        .responds_to(
            &fixture.tool.result_request_id,
            Response::new(
                spot_llm::Message::assistant(fixture.tool.result_reply.clone()),
                StopReason::Stop,
                fixture.tool.usage,
            ),
        )
        .fails_for(
            &fixture.error.request_id,
            LlmError::BadRequest {
                detail: format!("could not process {}", fixture.error.user_text),
            },
        );

    let mut wrong = fixture.clone();
    wrong.error.code = "llm.bad_request".to_owned();
    wrong.error.retryable = false;
    wrong.error.retry_after_secs = None;
    let failures = spot_llm::conformance::run(&leaky, &wrong).await;
    assert!(
        failures
            .iter()
            .any(|failure| failure.scenario == "error" && failure.detail.contains("leaked")),
        "the no-leak rule must flag the echoed request: {failures:?}"
    );
}

#[tokio::test]
async fn the_suite_flags_an_unscripted_provider() {
    let fixture = Fixture::sample();
    // No scripts at all: text, both tool legs and both error paths fail loudly.
    let empty = MockProvider::new();
    let failures = spot_llm::conformance::run(&empty, &fixture).await;
    assert!(
        failures.len() >= 3,
        "an unscripted provider fails loudly: {failures:?}"
    );
    assert!(
        failures.iter().any(|failure| failure.scenario == "error"),
        "the error scenario must report: {failures:?}"
    );
    assert!(
        failures.iter().any(|failure| failure.scenario == "text"),
        "the text scenario must report: {failures:?}"
    );
    assert!(
        failures
            .iter()
            .any(|failure| failure.scenario == "tool_use"),
        "the tool scenarios must report: {failures:?}"
    );
}
