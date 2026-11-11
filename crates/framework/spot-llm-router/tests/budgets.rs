//! The router's budget and accounting face end to end, on scripted [`MockProvider`]s:
//! budget stops with the provider call count asserted zero, the warn callback, exact
//! settlement of the real cost, stream settlement and release, and concurrent
//! reservations under `tokio::spawn`.

mod common;

use std::sync::Arc;
use std::sync::Mutex;

use common::Build;
use common::build_router;
use common::request;
use common::request_for;
use common::response;
use common::table_of;
use common::usage_at;
use spot_llm::LlmError;
use spot_llm::MockAction;
use spot_llm::MockProvider;
use spot_llm::MockStep;
use spot_llm::Provider;
use spot_llm::StopReason;
use spot_llm::StreamEvent;
use spot_llm::stream::drain;
use spot_llm_router::Budget;
use spot_llm_router::BudgetBook;
use spot_llm_router::BudgetScope;
use spot_llm_router::BudgetWarning;
use spot_llm_router::Price;
use spot_llm_router::PriceTable;
use spot_llm_router::SumQuery;
use spot_llm_router::UsageLedger;

/// A budget on the tenant `acme`, cap `limit_micro`, warn at `warn_at_percent`.
fn tenant_budget(limit_micro: u64, warn_at_percent: u64) -> BudgetBook {
    let budgets = BudgetBook::new();
    budgets.set_budget(Budget::new(
        BudgetScope::Tenant {
            tenant: "acme".into(),
        },
        limit_micro,
        warn_at_percent,
    ));
    budgets
}

#[tokio::test]
async fn budget_stop_answers_before_the_provider_is_called() {
    let counted = Arc::new(
        MockProvider::titled("counted").responds_to("acme:chat:r1", response(usage_at(0, 0))),
    );
    let budgets = tenant_budget(3, 0);
    let build = Build {
        table: table_of(&[("chat", &[("counted", "m1")])]),
        providers: vec![("counted", Arc::clone(&counted))],
        budgets,
        ..Build::default()
    };
    let (router, _clock, ledger) = build_router(build);
    // The unpriced model reserves tokens one-for-one: one "hello" line is ~1 input token,
    // and the requested output cap of 8192 pushes the estimate far over the cap of 3.
    let fat_request = request("chat").with_max_output_tokens(8_192);

    let error = match router.complete_detail(fat_request).await {
        Ok(reply) => panic!("the budget must stop the call, but answered {reply:?}"),
        Err(error) => error,
    };
    assert_eq!(error.code(), "router.budget_exceeded");
    assert_eq!(
        counted.recorded_count(),
        0,
        "an over-budget call never reaches the provider"
    );
    assert_eq!(ledger.records_count(), 0, "nothing was recorded");
    let (settled, reserved) = router
        .budgets()
        .view(&BudgetScope::Tenant {
            tenant: "acme".into(),
        })
        .unwrap_or_else(|| panic!("the budget exists"));
    assert_eq!((settled, reserved), (0, 0), "nothing was reserved either");
}

#[tokio::test]
async fn a_priced_route_is_stopped_by_its_estimated_cost() {
    let counted = Arc::new(
        MockProvider::titled("counted").responds_to("acme:chat:r1", response(usage_at(0, 0))),
    );
    let budgets = tenant_budget(10, 0);
    // 4 micro per million input tokens, 10 per million output: the worst-case estimate
    // of ~(1 input, 2 output requested) tokens is (4M + 20M + 500k)/1M = 25050 micro,
    // far over the cap of 10 — refused with zero provider calls.
    let prices = PriceTable::new().with_price("counted", "m1", Price::new(4_000_000, 10_000_000));
    let build = Build {
        table: table_of(&[("chat", &[("counted", "m1")])]),
        providers: vec![("counted", Arc::clone(&counted))],
        prices,
        budgets,
        ..Build::default()
    };
    let (router, _clock, _ledger) = build_router(build);
    let small_request = request("chat").with_max_output_tokens(2);

    let error = match router.complete_detail(small_request).await {
        Ok(reply) => {
            panic!("the estimate of 25050 micro is over the cap of 10, but {reply:?}")
        }
        Err(error) => error,
    };
    assert_eq!(error.code(), "router.budget_exceeded");
    assert_eq!(counted.recorded_count(), 0, "the provider was never called");
}

#[tokio::test]
async fn a_fitting_call_settles_the_real_cost() {
    let counted = Arc::new(
        MockProvider::titled("counted")
            .responds_to("acme:chat:r1", response(usage_at(1_000_000, 2_000_000))),
    );
    let budgets = tenant_budget(1_000_000, 0);
    let prices = PriceTable::new().with_price("counted", "m1", Price::new(4, 10));
    let build = Build {
        table: table_of(&[("chat", &[("counted", "m1")])]),
        providers: vec![("counted", Arc::clone(&counted))],
        prices,
        budgets,
        ..Build::default()
    };
    let (router, _clock, ledger) = build_router(build);

    let _ = match router.complete(request("chat")).await {
        Ok(reply) => reply,
        Err(error) => panic!("the estimate fits the 1M cap: {error}"),
    };
    let (settled, reserved) = router
        .budgets()
        .view(&BudgetScope::Tenant {
            tenant: "acme".into(),
        })
        .unwrap_or_else(|| panic!("the budget exists"));
    assert_eq!(
        settled, 24,
        "the settled cost is the exact integer cost of the real usage (4 + 2*10)"
    );
    assert_eq!(reserved, 0, "the reserve was replaced by the settlement");
    let sums = router.usage(&SumQuery::tenant("acme"));
    assert_eq!(sums.requests, 1, "the ledger settled exactly once");
    assert_eq!(sums.input, 1_000_000);
    assert_eq!(sums.output, 2_000_000);
    assert_eq!(ledger.records_count(), 1);
}

#[tokio::test]
async fn crossing_the_warn_level_emits_one_warning_per_budget() {
    let counted = Arc::new({
        let mut provider = MockProvider::titled("counted");
        provider.push_step(MockStep::new(
            None,
            MockAction::Respond(response(usage_at(0, 0))),
        ));
        provider
    });
    let budgets = tenant_budget(1_000, 50);
    let warnings: Arc<Mutex<Vec<BudgetWarning>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&warnings);
    let build = Build {
        table: table_of(&[("chat", &[("counted", "m1")])]),
        providers: vec![("counted", Arc::clone(&counted))],
        budgets,
        on_warning: Some(Arc::new(move |warning: &BudgetWarning| {
            sink.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(warning.clone());
        })),
        ..Build::default()
    };
    let (router, _clock, _ledger) = build_router(build);

    // w1: estimate 401 -> 40% of the cap; under 50, no warning.
    let w1 = request_for("chat", "w1").with_max_output_tokens(400);
    let _ = match router.complete(w1).await {
        Ok(reply) => reply,
        Err(error) => panic!("w1 fits: {error}"),
    };
    // w2: estimate 701 -> 70%; the warn level is crossed, once.
    let w2 = request_for("chat", "w2").with_max_output_tokens(700);
    let _ = match router.complete(w2).await {
        Ok(reply) => reply,
        Err(error) => panic!("w2 fits: {error}"),
    };
    // w3 and w4 cross nothing new (the level already fired).
    for caller in ["w3", "w4"] {
        let call = request_for("chat", caller).with_max_output_tokens(400);
        let _ = match router.complete(call).await {
            Ok(reply) => reply,
            Err(error) => panic!("{caller} fits: {error}"),
        };
    }
    // w5: the estimate alone is over the cap; refused with zero provider calls.
    let w5 = request_for("chat", "w5").with_max_output_tokens(4_000_000);
    let result = match router.complete(w5).await {
        Ok(reply) => panic!("w5 must be refused, but answered {reply:?}"),
        Err(error) => error,
    };
    assert_eq!(result.code(), "llm.budget_exceeded");
    assert_eq!(
        counted.recorded_count(),
        4,
        "w1..w4 ran; the over-budget w5 never reached the provider"
    );

    let fired = warnings
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    assert_eq!(fired.len(), 1, "one warning, on the first crossing only");
    assert_eq!(fired[0].percent, 70);
    assert_eq!(fired[0].projected_micro, 701);
    assert_eq!(fired[0].limit_micro, 1_000);
}

#[tokio::test]
async fn accounting_is_idempotent_per_request_id() {
    let counted = Arc::new(
        MockProvider::titled("counted").responds_to("acme:chat:r1", response(usage_at(100, 10))),
    );
    let build = Build {
        table: table_of(&[("chat", &[("counted", "m1")])]),
        providers: vec![("counted", Arc::clone(&counted))],
        ..Build::default()
    };
    let (router, _clock, _ledger) = build_router(build);

    // The same request id sent twice (a caller's retry of a whole settled request): both
    // calls really ran, but the ledger keeps one record for the id.
    let _ = match router.complete(request("chat")).await {
        Ok(reply) => reply,
        Err(error) => panic!("the first call answers: {error}"),
    };
    let _ = match router.complete(request("chat")).await {
        Ok(reply) => reply,
        Err(error) => panic!("the repeated call answers too: {error}"),
    };
    assert_eq!(counted.recorded_count(), 2, "both calls really ran");
    let sums = router.usage(&SumQuery::all());
    assert_eq!(sums.requests, 1, "one record per request id");
    assert_eq!(sums.input, 100, "the first settlement's usage stands");
}

#[tokio::test]
async fn the_routed_stream_settles_when_usage_arrives() {
    let events = vec![
        StreamEvent::TextDelta {
            delta: "all".into(),
        },
        StreamEvent::Usage {
            usage: usage_at(30, 8),
        },
        StreamEvent::Stop {
            reason: StopReason::Stop,
        },
    ];
    let streaming = Arc::new(MockProvider::titled("streaming").streams_to("acme:chat:r1", events));
    let build = Build {
        table: table_of(&[("chat", &[("streaming", "m1")])]),
        providers: vec![("streaming", streaming)],
        ..Build::default()
    };
    let (router, _clock, ledger) = build_router(build);

    let mut stream = match router.stream(request("chat")).await {
        Ok(stream) => stream,
        Err(error) => panic!("the routed stream opens: {error}"),
    };
    let items = drain(&mut stream).await;
    let mut count = 0;
    for item in items {
        match item {
            Ok(_) => count += 1,
            Err(error) => panic!("stream error: {error}"),
        }
    }
    assert_eq!(count, 3, "the consumer saw every event");

    // Settlement happened when the Usage event flowed through the router stream.
    let sums = router.usage(&SumQuery::all());
    assert_eq!(sums.requests, 1);
    assert_eq!(sums.input, 30);
    assert_eq!(sums.output, 8);
    assert_eq!(ledger.records_count(), 1);
}

#[tokio::test]
async fn a_stream_the_consumer_abandons_releases_its_reserve() {
    let events = vec![
        StreamEvent::TextDelta {
            delta: "all".into(),
        },
        StreamEvent::Usage {
            usage: usage_at(1_000, 1_000),
        },
        StreamEvent::Stop {
            reason: StopReason::Stop,
        },
    ];
    let counted = Arc::new(MockProvider::titled("counted").streams_to("acme:chat:r1", events));
    let budgets = tenant_budget(1_100, 0);
    let build = Build {
        table: table_of(&[("chat", &[("counted", "m1")])]),
        providers: vec![("counted", counted)],
        budgets,
        ..Build::default()
    };
    let (router, _clock, ledger) = build_router(build);

    // Open the stream and drop it before any event is consumed: the estimate is
    // released, so nothing hangs over the budget.
    {
        let stream = match router.stream(request("chat")).await {
            Ok(stream) => stream,
            Err(error) => panic!("the stream opens: {error}"),
        };
        drop(stream);
    }
    let (settled, reserved) = router
        .budgets()
        .view(&BudgetScope::Tenant {
            tenant: "acme".into(),
        })
        .unwrap_or_else(|| panic!("the budget exists"));
    assert_eq!(
        (settled, reserved),
        (0, 0),
        "an abandoned stream released its whole reserve"
    );
    assert_eq!(ledger.records_count(), 0, "nothing was settled");

    // The freed budget admits the request again: this time the consumer drains it.
    let mut stream = match router.stream(request("chat")).await {
        Ok(stream) => stream,
        Err(error) => panic!("the stream opens again: {error}"),
    };
    for item in drain(&mut stream).await {
        match item {
            Ok(_) => {}
            Err(error) => panic!("stream error: {error}"),
        }
    }
    let (settled, reserved) = router
        .budgets()
        .view(&BudgetScope::Tenant {
            tenant: "acme".into(),
        })
        .unwrap_or_else(|| panic!("the budget exists"));
    // Settled unpriced: the real token sum 2_000 (1_000 in, 1_000 out) against the 1_100
    // cap — a settled unpriced request may exceed its estimate; the record and the
    // settle stay exact either way.
    assert_eq!(
        reserved, 0,
        "the second stream settled, nothing stays reserved"
    );
    assert_eq!(settled, 2_000);
    assert_eq!(ledger.records_count(), 1);
}

#[tokio::test]
async fn a_stream_that_fails_mid_flight_records_nothing() {
    // The provider's stream errors before any usage: the router falls back to the second
    // route, which answers complete-style, and only its usage settles.
    let failing = Arc::new({
        let mut provider = MockProvider::titled("failing");
        provider.push_step(MockStep::new(
            Some("acme:chat:r1"),
            MockAction::Fail(LlmError::Unavailable),
        ));
        provider
    });
    let fallback = Arc::new(
        MockProvider::titled("fallback").responds_to("acme:chat:r1", response(usage_at(5, 5))),
    );
    let build = Build {
        table: table_of(&[("chat", &[("failing", "m1"), ("fallback", "m2")])]),
        providers: vec![
            ("failing", Arc::clone(&failing)),
            ("fallback", Arc::clone(&fallback)),
        ],
        config: spot_llm_router::RouterConfig::default().with_max_retries(0),
        ..Build::default()
    };
    let (router, _clock, ledger) = build_router(build);

    let mut stream = match router.stream(request("chat")).await {
        Ok(stream) => stream,
        Err(error) => panic!("the stream opens on the fallback: {error}"),
    };
    for item in drain(&mut stream).await {
        match item {
            Ok(_) => {}
            Err(error) => panic!("stream error: {error}"),
        }
    }
    // The failing provider WAS attempted (its stream open failed, so the router fell
    // back); the fallback answered complete-style, and only its usage settled.
    assert_eq!(
        failing.recorded_count(),
        1,
        "the failing provider was tried once, on the stream open"
    );
    assert_eq!(ledger.records_count(), 1, "the fallback's usage settled");
    assert_eq!(
        ledger
            .sum(&SumQuery::tenant("acme").and_provider("failing"))
            .requests,
        0,
        "the failed open never settled usage"
    );
}

#[tokio::test]
async fn concurrent_calls_reserve_and_settle_without_overshooting() {
    // Cap 2100, eight calls each estimating 1001 (one input token plus a 1000-token
    // output request on an unpriced model) and settling 900 real tokens: at most two are
    // admitted under the cap and the settled spend lands at exactly 1800 — the
    // reservations in flight are what kept the third out.
    let counted = Arc::new({
        let mut provider = MockProvider::titled("counted");
        provider.push_step(MockStep::new(
            None,
            MockAction::Respond(response(usage_at(600, 300))),
        ));
        provider
    });
    let budgets = tenant_budget(2_100, 0);
    let build = Build {
        table: table_of(&[("chat", &[("counted", "m1")])]),
        providers: vec![("counted", counted)],
        budgets,
        ..Build::default()
    };
    let (router, _clock, ledger) = build_router(build);
    let router = Arc::new(router);

    let mut handles = Vec::new();
    for round in 0..8 {
        let router = Arc::clone(&router);
        let call = request_for("chat", &format!("rc{round}")).with_max_output_tokens(1_000);
        handles.push(tokio::spawn(
            async move { router.complete(call).await.is_ok() },
        ));
    }
    let mut admitted = 0;
    for handle in handles {
        match handle.await {
            Ok(joined) => {
                if joined {
                    admitted += 1;
                }
            }
            Err(error) => panic!("a concurrent call panicked: {error}"),
        }
    }

    let (settled, reserved) = router
        .budgets()
        .view(&BudgetScope::Tenant {
            tenant: "acme".into(),
        })
        .unwrap_or_else(|| panic!("the budget exists"));
    assert_eq!(
        admitted, 2,
        "the cap of 2100 admits two 1001-token estimates"
    );
    assert_eq!(settled, 1_800, "the two admitted calls settled 900 each");
    assert_eq!(reserved, 0, "every admitted call settled");
    assert_eq!(
        ledger.records_count(),
        2,
        "exactly the admitted calls recorded usage"
    );
}
