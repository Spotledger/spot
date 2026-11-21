//! The router's routing face end to end, on scripted [`MockProvider`]s: routing by task,
//! capability filtering, fallback order, retries with an observable backoff schedule, and
//! the circuit breaker open, skip and close. The budget, stream and concurrency faces
//! live in [`crate budgets.rs`](../budgets/index.html) — `tests/budgets.rs`.

mod common;

use std::sync::Arc;

use common::Build;
use common::build_router;
use common::request;
use common::request_for;
use common::response;
use common::table_of;
use common::usage_at;
use spot_llm::Capabilities;
use spot_llm::LlmError;
use spot_llm::Message;
use spot_llm::MockProvider;
use spot_llm::Provider;
use spot_llm::Request;
use spot_llm::StopReason;
use spot_llm_router::RouterConfig;
use spot_llm_router::SumQuery;

#[tokio::test]
async fn routes_by_task_to_the_first_healthy_provider() {
    let chat =
        Arc::new(MockProvider::titled("a").responds_to("acme:chat:r1", response(usage_at(10, 4))));
    let build = Build {
        table: table_of(&[("chat", &[("a", "m1")])]),
        providers: vec![("a", Arc::clone(&chat))],
        ..Build::default()
    };
    let (router, _clock, ledger) = build_router(build);

    let reply = match router.complete(request("chat")).await {
        Ok(reply) => reply,
        Err(error) => panic!("the routed call should answer: {error}"),
    };
    assert_eq!(reply.text(), "done");
    assert_eq!(reply.stop_reason, StopReason::Stop);

    let sums = router.usage(&SumQuery::all());
    assert_eq!(sums.requests, 1);
    assert_eq!(
        sums.input, 10,
        "the answer's usage is what the ledger holds"
    );
    assert_eq!(sums.output, 4);
    assert_eq!(
        router
            .usage(&SumQuery::tenant("acme").and_task("chat"))
            .requests,
        1
    );
    assert_eq!(
        router.usage(&SumQuery::tenant("other-tenant")).requests,
        0,
        "another tenant's sums stay empty"
    );
    assert_eq!(chat.recorded_count(), 1);
    assert_eq!(ledger.records_count(), 1, "one settled request, one record");
}

#[tokio::test]
async fn the_outbound_request_carries_the_routes_model() {
    let provider =
        Arc::new(MockProvider::titled("a").responds_to("acme:chat:r1", response(usage_at(0, 0))));
    let build = Build {
        table: table_of(&[("chat", &[("a", "routed-model")])]),
        providers: vec![("a", Arc::clone(&provider))],
        ..Build::default()
    };
    let (router, _clock, _ledger) = build_router(build);
    let _ = match router.complete(request("chat")).await {
        Ok(reply) => reply,
        Err(error) => panic!("the routed call should answer: {error}"),
    };
    let recorded = provider.requests();
    assert_eq!(recorded.len(), 1);
    assert_eq!(
        recorded[0].model, "routed-model",
        "the provider is called with the route's model, not the caller's echo"
    );
}

#[tokio::test]
async fn a_request_to_an_unknown_task_is_task_unrouted() {
    let unused = Arc::new(MockProvider::titled("unused"));
    let build = Build {
        table: table_of(&[("chat", &[("unused", "m1")])]),
        providers: vec![("unused", unused)],
        ..Build::default()
    };
    let (router, _clock, ledger) = build_router(build);

    let error = match router.complete_detail(request("plan")).await {
        Ok(reply) => panic!("plan is not routed, but answered {reply:?}"),
        Err(error) => error,
    };
    assert_eq!(error.code(), "router.task_unrouted");
    assert_eq!(
        error.with_code(),
        "router.task_unrouted: no route for task `plan`"
    );
    assert_eq!(ledger.records_count(), 0, "nothing was routed or recorded");

    // Through the Provider trait the same failure reads as the trait's bad_request.
    let mapped = match router.complete(request("plan")).await {
        Ok(reply) => panic!("plan is not routed, but answered {reply:?}"),
        Err(error) => error,
    };
    assert_eq!(mapped.code(), "llm.bad_request");
}

#[tokio::test]
async fn a_request_without_the_tenant_task_prefix_is_refused() {
    let unused = Arc::new(MockProvider::titled("unused"));
    let build = Build {
        table: table_of(&[("chat", &[("unused", "m1")])]),
        providers: vec![("unused", unused)],
        ..Build::default()
    };
    let (router, _clock, ledger) = build_router(build);

    let bare = Request::new("unset", "no-convention-here").with_message(Message::user("hello"));
    let error = match router.complete_detail(bare).await {
        Ok(reply) => panic!("a bare id is unroutable, but answered {reply:?}"),
        Err(error) => error,
    };
    assert_eq!(error.code(), "router.task_unrouted");
    assert_eq!(ledger.records_count(), 0);
}

#[tokio::test]
async fn a_tool_request_skips_a_provider_without_tools() {
    let no_tools = Arc::new(
        MockProvider::titled("cheapid")
            .with_capabilities(Capabilities {
                tools: false,
                ..Capabilities::mock()
            })
            .responds_to("acme:chat:r1", response(usage_at(0, 0))),
    );
    let with_tools = Arc::new(
        MockProvider::titled("fullfat").responds_to("acme:chat:r1", response(usage_at(7, 2))),
    );
    let build = Build {
        table: table_of(&[("chat", &[("cheapid", "m1"), ("fullfat", "m2")])]),
        providers: vec![
            ("cheapid", Arc::clone(&no_tools)),
            ("fullfat", Arc::clone(&with_tools)),
        ],
        ..Build::default()
    };
    let (router, _clock, _ledger) = build_router(build);

    let tool_request = request("chat")
        .with_tool(spot_llm::ToolSpec {
            name: "lookup".into(),
            description: "look a thing up".into(),
            input_schema: serde_json::json!({"type": "object"}),
        })
        .with_max_output_tokens(64);
    let reply = match router.complete(tool_request).await {
        Ok(reply) => reply,
        Err(error) => panic!("the capable route should answer: {error}"),
    };
    assert_eq!(reply.text(), "done");
    assert_eq!(
        no_tools.recorded_count(),
        0,
        "the tool-less provider was never called"
    );
    assert_eq!(
        with_tools.recorded_count(),
        1,
        "the tool-capable provider answered"
    );
    assert_eq!(
        router.usage(&SumQuery::all()).output,
        2,
        "the tool answer came from the tool-capable provider"
    );
}

#[tokio::test]
async fn a_request_bigger_than_the_window_skips_the_small_model() {
    let small = Arc::new(
        MockProvider::titled("small").with_capabilities(Capabilities {
            context_window: 10,
            ..Capabilities::mock()
        }),
    );
    let big =
        Arc::new(MockProvider::titled("big").responds_to("acme:chat:r1", response(usage_at(0, 0))));
    let build = Build {
        table: table_of(&[("chat", &[("small", "m1"), ("big", "m2")])]),
        providers: vec![("small", Arc::clone(&small)), ("big", Arc::clone(&big))],
        ..Build::default()
    };
    let (router, _clock, _ledger) = build_router(build);

    let long = request("chat").with_message(Message::user("x".repeat(1_000)));
    let reply = match router.complete(long).await {
        Ok(reply) => reply,
        Err(error) => panic!("the big model should answer: {error}"),
    };
    assert_eq!(reply.text(), "done");
    assert_eq!(
        small.recorded_count(),
        0,
        "the small-window provider was never called"
    );
    assert_eq!(big.recorded_count(), 1);
}

#[tokio::test]
async fn fallback_moves_down_the_chain_in_order() {
    let dead =
        Arc::new(MockProvider::titled("first").fails_for("acme:chat:r1", LlmError::Unavailable));
    let alive = Arc::new(
        MockProvider::titled("second").responds_to("acme:chat:r1", response(usage_at(3, 1))),
    );
    let build = Build {
        table: table_of(&[("chat", &[("first", "m1"), ("second", "m2")])]),
        providers: vec![("first", Arc::clone(&dead)), ("second", Arc::clone(&alive))],
        // No same-route retries here: the fallback order is what this test proves.
        config: RouterConfig::default().with_max_retries(0),
        ..Build::default()
    };
    let (router, _clock, ledger) = build_router(build);

    let reply = match router.complete(request("chat")).await {
        Ok(reply) => reply,
        Err(error) => panic!("the fallback route should answer: {error}"),
    };
    assert_eq!(reply.text(), "done");
    assert_eq!(
        router
            .usage(&SumQuery::tenant("acme").and_provider("second"))
            .requests,
        1,
        "the surviving provider served the task"
    );
    assert_eq!(
        router
            .usage(&SumQuery::tenant("acme").and_provider("first"))
            .requests,
        0,
        "a failed attempt never records usage"
    );
    assert_eq!(dead.recorded_count(), 1, "the dead provider was tried once");
    assert_eq!(ledger.records_count(), 1);
}

#[tokio::test]
async fn auth_and_bad_request_never_retry_the_same_route() {
    let keeper = Arc::new(MockProvider::titled("keeper").fails_for("acme:chat:r1", LlmError::Auth));
    let stubborn = Arc::new(MockProvider::titled("refuser").fails_for(
        "acme:chat:r1",
        LlmError::BadRequest {
            detail: "shape".into(),
        },
    ));
    let build = Build {
        table: table_of(&[("chat", &[("keeper", "m1"), ("refuser", "m2")])]),
        providers: vec![
            ("keeper", Arc::clone(&keeper)),
            ("refuser", Arc::clone(&stubborn)),
        ],
        ..Build::default()
    };
    let (router, clock, _ledger) = build_router(build);

    let error = match router.complete_detail(request("chat")).await {
        Ok(reply) => panic!("every route fails hard, but answered {reply:?}"),
        Err(error) => error,
    };
    assert_eq!(error.code(), "router.routes_exhausted");
    assert!(
        error.with_code().contains("llm.bad_request"),
        "the last failure is named in the detail: {error}"
    );
    assert_eq!(clock.waits(), Vec::<u64>::new(), "no backoff waits fired");
    assert_eq!(keeper.recorded_count(), 1, "auth: one attempt, no retry");
    assert_eq!(
        stubborn.recorded_count(),
        1,
        "bad_request: one attempt, no retry"
    );
}

#[tokio::test]
async fn a_retryable_failure_retries_then_falls_back_with_recorded_backoff() {
    let flaky =
        Arc::new(MockProvider::titled("flaky").fails_for("acme:chat:r1", LlmError::Overloaded));
    let solid = Arc::new(
        MockProvider::titled("solid").responds_to("acme:chat:r1", response(usage_at(0, 0))),
    );
    let build = Build {
        table: table_of(&[("chat", &[("flaky", "m1"), ("solid", "m2")])]),
        providers: vec![("flaky", Arc::clone(&flaky)), ("solid", Arc::clone(&solid))],
        config: RouterConfig::default()
            .with_max_retries(2)
            .with_backoff(250, 4_000),
        ..Build::default()
    };
    let (router, clock, _ledger) = build_router(build);

    let reply = match router.complete(request("chat")).await {
        Ok(reply) => reply,
        Err(error) => panic!("the fallback should answer: {error}"),
    };
    assert_eq!(reply.text(), "done");
    assert_eq!(
        clock.waits(),
        vec![250, 500],
        "capped exponential backoff between the two retries, nothing on fallback"
    );
    assert_eq!(
        flaky.recorded_count(),
        3,
        "the retryable failure was retried twice on the same route"
    );
}

#[tokio::test]
async fn a_provider_retry_after_is_honoured_over_the_schedule() {
    let limited = Arc::new(MockProvider::titled("limited").fails_for(
        "acme:chat:r1",
        LlmError::RateLimited {
            retry_after: Some(9),
        },
    ));
    let solid = Arc::new(
        MockProvider::titled("solid").responds_to("acme:chat:r1", response(usage_at(0, 0))),
    );
    let build = Build {
        table: table_of(&[("chat", &[("limited", "m1"), ("solid", "m2")])]),
        providers: vec![("limited", limited), ("solid", solid)],
        config: RouterConfig::default().with_max_retries(1),
        ..Build::default()
    };
    let (router, clock, _ledger) = build_router(build);

    let _ = match router.complete(request("chat")).await {
        Ok(reply) => reply,
        Err(error) => panic!("the fallback should answer: {error}"),
    };
    assert_eq!(
        clock.waits(),
        vec![9_000],
        "the provider's retry_after (seconds) is the wait"
    );
}

#[tokio::test]
async fn breaker_opens_skips_and_closes_again() {
    // r1-r3 fail on the route (one call each, no retries), the breaker opens on the
    // third. r4 must skip the open route entirely. Past the cool-down the route is tried
    // again (half-open); its scripted success closes it, and r6 uses it normally again.
    let broken = Arc::new(
        MockProvider::titled("broken")
            .fails_for("acme:chat:r1", LlmError::Unavailable)
            .fails_for("acme:chat:r2", LlmError::Unavailable)
            .fails_for("acme:chat:r3", LlmError::Unavailable)
            .responds_to("acme:chat:r5", response(usage_at(1, 1)))
            .responds_to("acme:chat:r6", response(usage_at(2, 2))),
    );
    let healed = Arc::new(
        MockProvider::titled("healed")
            .responds_to("acme:chat:r1", response(usage_at(0, 0)))
            .responds_to("acme:chat:r2", response(usage_at(0, 0)))
            .responds_to("acme:chat:r3", response(usage_at(0, 0)))
            .responds_to("acme:chat:r4", response(usage_at(0, 0))),
    );
    let build = Build {
        table: table_of(&[("chat", &[("broken", "m1"), ("healed", "m2")])]),
        providers: vec![
            ("broken", Arc::clone(&broken)),
            ("healed", Arc::clone(&healed)),
        ],
        config: RouterConfig::default()
            .with_max_retries(0)
            .with_breaker(3, 60_000, 30_000),
        ..Build::default()
    };
    let (router, clock, ledger) = build_router(build);

    for caller in ["r1", "r2", "r3"] {
        let _ = match router.complete(request_for("chat", caller)).await {
            Ok(reply) => reply,
            Err(error) => panic!("caller {caller} falls back and answers: {error}"),
        };
    }
    assert_eq!(
        broken.recorded_count(),
        3,
        "each of the three requests tried the broken route once"
    );

    // Tripped: r4 skips the broken route without a call; healed still answers.
    let _ = match router.complete(request_for("chat", "r4")).await {
        Ok(reply) => reply,
        Err(error) => panic!("the fallback serves while the breaker is open: {error}"),
    };
    assert_eq!(
        broken.recorded_count(),
        3,
        "an open breaker skips its route without a call"
    );
    assert_eq!(
        router
            .usage(&SumQuery::tenant("acme").and_provider("broken"))
            .requests,
        0,
        "no usage was ever settled on the broken route"
    );

    // Half-open: past the cool-down the route is probed; the success closes it.
    clock.advance(31_000);
    let _ = match router.complete(request_for("chat", "r5")).await {
        Ok(reply) => reply,
        Err(error) => panic!("the half-open probe should answer: {error}"),
    };
    assert_eq!(broken.recorded_count(), 4, "the half-open probe reached it");

    // Closed again: the next request uses the route normally.
    let _ = match router.complete(request_for("chat", "r6")).await {
        Ok(reply) => reply,
        Err(error) => panic!("the closed breaker's route should answer: {error}"),
    };
    assert_eq!(broken.recorded_count(), 5, "the route serves again");
    assert_eq!(
        router
            .usage(&SumQuery::tenant("acme").and_provider("broken"))
            .requests,
        2,
        "the settled usages are the two successes"
    );
    assert_eq!(ledger.records_count(), 6, "every settled request recorded");
}

#[tokio::test]
async fn a_success_clears_the_failure_count_before_it_opens() {
    // r2 fails, r4 succeeds on the same route (clearing the count), then r5-r7 fail
    // again: the breaker opens only at the third of those — proven by r7 being tried
    // (count 3, breaker opens afterwards) and r8 being skipped.
    let alt = Arc::new(
        MockProvider::titled("alt")
            .fails_for("acme:chat:r2", LlmError::Timeout)
            .responds_to("acme:chat:r4", response(usage_at(0, 0)))
            .fails_for("acme:chat:r5", LlmError::Timeout)
            .fails_for("acme:chat:r6", LlmError::Timeout)
            .fails_for("acme:chat:r7", LlmError::Timeout),
    );
    let solid = Arc::new(
        MockProvider::titled("solid")
            .responds_to("acme:chat:r2", response(usage_at(0, 0)))
            .responds_to("acme:chat:r5", response(usage_at(0, 0)))
            .responds_to("acme:chat:r6", response(usage_at(0, 0)))
            .responds_to("acme:chat:r7", response(usage_at(0, 0)))
            .responds_to("acme:chat:r8", response(usage_at(0, 0))),
    );
    let build = Build {
        table: table_of(&[("chat", &[("alt", "m1"), ("solid", "m2")])]),
        providers: vec![("alt", Arc::clone(&alt)), ("solid", Arc::clone(&solid))],
        config: RouterConfig::default()
            .with_max_retries(0)
            .with_breaker(3, 60_000, 30_000),
        ..Build::default()
    };
    let (router, _clock, _ledger) = build_router(build);

    for caller in ["r2", "r4", "r5", "r6", "r7", "r8"] {
        let _ = match router.complete(request_for("chat", caller)).await {
            Ok(reply) => reply,
            Err(error) => panic!("caller {caller} should be served somehow: {error}"),
        };
    }
    assert_eq!(
        alt.recorded_count(),
        5,
        "r2, r4, r5, r6 and r7 were tried; r8 was skipped by the freshly opened breaker"
    );
}
