//! The ledger: idempotent recording, every-dimension sums, exact cost, reservation
//! arithmetic and the budget book's concurrent reserve-settle flow.

use std::sync::Arc;

use spot_llm_router::Budget;
use spot_llm_router::BudgetBook;
use spot_llm_router::BudgetContext;
use spot_llm_router::BudgetScope;
use spot_llm_router::DayKey;
use spot_llm_router::MemoryLedger;
use spot_llm_router::Price;
use spot_llm_router::PriceTable;
use spot_llm_router::ReserveOutcome;
use spot_llm_router::SumQuery;
use spot_llm_router::UsageLedger;
use spot_llm_router::UsageRecord;
use spot_llm_router::cost;

/// A record for `request_id` with the given shape; other fields vary by the id.
fn record(at: u64, tenant: &str, task: &str, request_id: &str) -> UsageRecord {
    UsageRecord {
        at,
        tenant: tenant.to_owned(),
        task: task.to_owned(),
        provider: "mock".to_owned(),
        model: "m1".to_owned(),
        input: 100,
        output: 10,
        cache_read: 5,
        cache_write: 1,
        request_id: request_id.to_owned(),
    }
}

#[test]
fn the_same_request_id_records_once() {
    let ledger = MemoryLedger::new();
    ledger.record(record(10, "acme", "chat", "acme:chat:r1"));
    // A settlement retry for the same request id is dropped, not double-counted.
    let mut retry = record(20, "acme", "chat", "acme:chat:r1");
    retry.input = 9_999;
    ledger.record(retry);
    let sums = ledger.sum(&SumQuery::all());
    assert_eq!(sums.requests, 1, "one id, one record");
    assert_eq!(sums.input, 100, "the first record wins");
}

#[test]
fn sums_filter_by_every_dimension() {
    let ledger = MemoryLedger::new();
    ledger.record(record(1_000, "acme", "chat", "acme:chat:r1"));
    ledger.record(record(2_000, "acme", "summarise", "acme:summarise:r2"));
    ledger.record(record(3_000, "other", "chat", "other:chat:r3"));
    let mut tomorrow = record(86_400_000 + 4_000, "acme", "chat", "acme:chat:r4");
    tomorrow.provider = "other-provider".to_owned();
    tomorrow.model = "m2".to_owned();
    ledger.record(tomorrow);

    assert_eq!(ledger.sum(&SumQuery::tenant("acme")).requests, 3);
    assert_eq!(
        ledger
            .sum(&SumQuery::tenant("acme").and_task("chat"))
            .requests,
        2
    );
    assert_eq!(
        ledger
            .sum(&SumQuery::tenant("acme").and_day(DayKey(0)))
            .requests,
        2,
        "only today's records"
    );
    assert_eq!(
        ledger
            .sum(&SumQuery::tenant("acme").and_provider("other-provider"))
            .requests,
        1
    );
    assert_eq!(
        ledger
            .sum(&SumQuery::tenant("acme").and_model("m2"))
            .requests,
        1
    );
    let everything = ledger.sum(&SumQuery::all());
    assert_eq!(everything.requests, 4);
    assert_eq!(everything.input, 400);
    assert_eq!(everything.output, 40);
    assert_eq!(everything.cache_read, 20);
    assert_eq!(everything.cache_write, 4);
}

#[test]
fn daykey_divides_the_integer_grid() {
    assert_eq!(DayKey::from_millis(0), DayKey(0));
    assert_eq!(DayKey::from_millis(86_399_999), DayKey(0));
    assert_eq!(DayKey::from_millis(86_400_000), DayKey(1));
    assert_eq!(DayKey::from_millis(173_459_999_999), DayKey(2007));
}

#[test]
fn cost_is_exact_integer_round_half_up() {
    // Micro-units per MILLION tokens: 3 and 15 here.
    let price = Price::new(3, 15);
    assert_eq!(cost(0, 0, &price), 0);
    assert_eq!(
        cost(1, 1, &price),
        0,
        "single tokens price below half a micro"
    );
    // 1M input at 3 == 3; 1M output at 15 == 15.
    assert_eq!(cost(1_000_000, 1_000_000, &price), 18);
    // Halves and near-halves round up, once, on the combined product:
    assert_eq!(cost(500_000, 0, &price), 2, "1.5 rounds half-up to 2");
    assert_eq!(cost(0, 333_334, &price), 5, "5.00001 rounds up to 5");
    assert_eq!(cost(500_001, 0, &price), 2, "1.500003 rounds up to 2");
    // The rounding is applied once to the combined product, not per side: two 0.7-micro
    // halves sum to 1.4, which rounds to 1 (per-side rounding would double-count up).
    assert_eq!(cost(700_000, 700_000, &Price::new(1, 1)), 1);
}

#[test]
fn the_default_price_serves_unnamed_models() {
    let prices = PriceTable::new()
        .with_price("mock", "m1", Price::new(1, 2))
        .with_default(Price::new(7, 9));
    assert_eq!(
        prices.price_for("mock", "m1"),
        Some(Price::new(1, 2)),
        "the exact row wins"
    );
    assert_eq!(
        prices.price_for("mock", "unknown"),
        Some(Price::new(7, 9)),
        "the default serves the rest"
    );
    assert_eq!(PriceTable::new().price_for("mock", "m1"), None);
}

#[test]
fn records_carry_the_priced_input_total() {
    let one = record(0, "acme", "chat", "acme:chat:r1");
    assert_eq!(one.priced_input(), 106, "input plus both cache sides");
}

#[test]
fn a_reserve_refuses_and_rolls_its_other_scopes_back() {
    let book = BudgetBook::new();
    book.set_budget(Budget::new(
        BudgetScope::Tenant {
            tenant: "acme".into(),
        },
        100,
        0,
    ));
    book.set_budget(Budget::new(
        BudgetScope::Task {
            task: "chat".into(),
        },
        60,
        0,
    ));

    let context = BudgetContext::new("acme", "chat", DayKey(0));
    // 40 fits both; the next 40 would fit the tenant but not the task: the refused
    // reserve touches nothing, so no spend hangs anywhere.
    match book.reserve(&context, 40) {
        ReserveOutcome::Reserved { warnings } => assert!(warnings.is_empty()),
        ReserveOutcome::Exhausted(refusal) => panic!("the first reserve fits, not {refusal:?}"),
    }
    match book.reserve(&context, 40) {
        ReserveOutcome::Reserved { warnings } => {
            panic!("the task budget of 60 cannot take 40 more, but got {warnings:?}")
        }
        ReserveOutcome::Exhausted(refusal) => {
            assert_eq!(
                refusal.scope,
                BudgetScope::Task {
                    task: "chat".to_owned()
                }
            );
            assert_eq!(refusal.limit_micro, 60_u64);
        }
    }
    match book.view(&BudgetScope::Tenant {
        tenant: "acme".into(),
    }) {
        Some((settled, reserved)) => {
            assert_eq!((settled, reserved), (0, 40), "the tenant kept its first 40")
        }
        None => panic!("the tenant budget exists"),
    }
    match book.view(&BudgetScope::Task {
        task: "chat".into(),
    }) {
        Some((settled, reserved)) => assert_eq!(
            (settled, reserved),
            (0, 40),
            "the task kept its first 40 too"
        ),
        None => panic!("the task budget exists"),
    }
}

#[test]
fn a_settle_replaces_the_estimate_with_the_actual_cost() {
    let book = BudgetBook::new();
    book.set_budget(Budget::new(
        BudgetScope::Tenant {
            tenant: "acme".into(),
        },
        200,
        0,
    ));
    let context = BudgetContext::new("acme", "chat", DayKey(0));
    match book.reserve(&context, 100) {
        ReserveOutcome::Reserved { .. } => {}
        ReserveOutcome::Exhausted(refusal) => panic!("100 of 200 fits, not {refusal:?}"),
    }
    book.settle(&context, 100, 30);
    match book.view(&BudgetScope::Tenant {
        tenant: "acme".into(),
    }) {
        Some((settled, reserved)) => assert_eq!(
            (settled, reserved),
            (30, 0),
            "the estimate left, the actual arrived"
        ),
        None => panic!("the budget exists"),
    }
    // The next reserve now plans against the settled 30, not the estimate.
    match book.reserve(&context, 171) {
        ReserveOutcome::Reserved { .. } => panic!("30 + 171 is over the cap of 200"),
        ReserveOutcome::Exhausted(_) => {}
    }
    match book.reserve(&context, 170) {
        ReserveOutcome::Reserved { .. } => {}
        ReserveOutcome::Exhausted(refusal) => panic!("30 + 170 is inside 200, not {refusal:?}"),
    }
}

#[test]
fn a_reserve_over_one_scope_is_released_from_every_scope_on_failure() {
    // Reserve 70 against both scopes (tenant cap 100, task cap 80), then a refused
    // reserve of 30 against the task cap: the tenant must NOT hold the 30 the refused
    // reserve would have added — the reserve's all-or-nothing rollback.
    let book = BudgetBook::new();
    book.set_budget(Budget::new(
        BudgetScope::Tenant {
            tenant: "acme".into(),
        },
        100,
        0,
    ));
    book.set_budget(Budget::new(
        BudgetScope::Task {
            task: "chat".into(),
        },
        80,
        0,
    ));
    let context = BudgetContext::new("acme", "chat", DayKey(0));
    match book.reserve(&context, 70) {
        ReserveOutcome::Reserved { .. } => {}
        ReserveOutcome::Exhausted(refusal) => panic!("70 fits both caps, not {refusal:?}"),
    }
    match book.reserve(&context, 30) {
        ReserveOutcome::Reserved { .. } => panic!("70 + 30 crosses the task cap of 80"),
        ReserveOutcome::Exhausted(_) => {}
    }
    match book.view(&BudgetScope::Tenant {
        tenant: "acme".into(),
    }) {
        Some((_, reserved)) => assert_eq!(reserved, 70, "the refused reserve rolled back"),
        None => panic!("the budget exists"),
    }
    // And after a settle, the same reserve admits again.
    book.settle(&context, 70, 40);
    match book.reserve(&context, 30) {
        ReserveOutcome::Reserved { .. } => {}
        ReserveOutcome::Exhausted(refusal) => panic!("40 + 30 fits both caps, not {refusal:?}"),
    }
}

#[test]
fn crossing_the_warn_level_fires_once_and_reports_the_projection() {
    let book = BudgetBook::new();
    book.set_budget(Budget::new(
        BudgetScope::Tenant {
            tenant: "acme".into(),
        },
        1_000,
        50,
    ));
    let context = BudgetContext::new("acme", "chat", DayKey(0));
    match book.reserve(&context, 400) {
        ReserveOutcome::Reserved { warnings } => {
            assert!(warnings.is_empty(), "40% is under 50%, no warning yet")
        }
        ReserveOutcome::Exhausted(refusal) => panic!("400 of 1000 fits, not {refusal:?}"),
    }
    match book.reserve(&context, 300) {
        ReserveOutcome::Reserved { warnings } => {
            assert_eq!(warnings.len(), 1, "the settle-less projection crosses 50%");
            assert_eq!(warnings[0].percent, 70);
            assert_eq!(warnings[0].projected_micro, 700);
            assert_eq!(warnings[0].limit_micro, 1_000);
        }
        ReserveOutcome::Exhausted(refusal) => panic!("700 of 1000 fits, not {refusal:?}"),
    }
    match book.reserve(&context, 300) {
        ReserveOutcome::Reserved { warnings } => {
            assert!(warnings.is_empty(), "the level does not fire twice")
        }
        ReserveOutcome::Exhausted(refusal) => panic!("100 is left, not {refusal:?}"),
    }
}

#[test]
fn the_book_releases_a_dropped_streams_reserve() {
    let book = BudgetBook::new();
    book.set_budget(Budget::new(
        BudgetScope::Tenant {
            tenant: "acme".into(),
        },
        1_000,
        0,
    ));
    let context = BudgetContext::new("acme", "chat", DayKey(0));
    match book.reserve(&context, 900) {
        ReserveOutcome::Reserved { .. } => {}
        ReserveOutcome::Exhausted(refusal) => panic!("900 of 1000 fits, not {refusal:?}"),
    }
    book.release(&context, 900);
    match book.view(&BudgetScope::Tenant {
        tenant: "acme".into(),
    }) {
        Some((settled, reserved)) => assert_eq!((settled, reserved), (0, 0), "fully released"),
        None => panic!("the budget exists"),
    }
}

#[test]
fn a_zero_estimate_reserve_is_admitted_and_warns_by_the_projection() {
    // A zero-cost reserve (an empty request on a free model) still runs the warning
    // arithmetic: a settled spend over the level warns on the next zero-estimate ride.
    let book = BudgetBook::new();
    book.set_budget(Budget::new(
        BudgetScope::Tenant {
            tenant: "acme".into(),
        },
        100,
        50,
    ));
    let context = BudgetContext::new("acme", "chat", DayKey(0));
    book.settle(&context, 0, 60);
    match book.reserve(&context, 0) {
        ReserveOutcome::Reserved { warnings } => {
            assert_eq!(warnings.len(), 1, "60% of a 100 cap is over the 50% level");
            assert_eq!(warnings[0].percent, 60);
        }
        ReserveOutcome::Exhausted(refusal) => panic!("0 fits, not {refusal:?}"),
    }
}

#[test]
fn parallel_threads_reserve_and_settle_without_overshooting() {
    // Eight OS threads racing one budget of 900 with reserves of 100: every thread that
    // wins reserves 100 first (so the concurrency really collides), and settles 1. The
    // invariant: settled spend + outstanding reserves never exceed 900, whatever the
    // interleaving; with everyone settled, reserves are 0 and spend is the winners' count.
    let book = Arc::new(BudgetBook::new());
    book.set_budget(Budget::new(
        BudgetScope::Tenant {
            tenant: "acme".into(),
        },
        900,
        0,
    ));
    let mut joined: Vec<std::thread::JoinHandle<bool>> = Vec::new();
    for _ in 0..8 {
        let book = Arc::clone(&book);
        joined.push(std::thread::spawn(move || {
            let context = BudgetContext::new("acme", "chat", DayKey(0));
            match book.reserve(&context, 100) {
                ReserveOutcome::Reserved { .. } => {
                    // Hold the reservation a beat so the other threads really collide
                    // against it, then settle to 1 real micro.
                    book.settle(&context, 100, 1);
                    true
                }
                ReserveOutcome::Exhausted(_) => false,
            }
        }));
    }
    let mut admitted = 0;
    for handle in joined {
        match handle.join() {
            Ok(won) => {
                if won {
                    admitted += 1;
                }
            }
            Err(_) => panic!("a thread panicked"),
        }
    }
    assert!(
        admitted >= 1,
        "at least one thread got through: {admitted} of 8"
    );
    let (settled, reserved) = book
        .view(&BudgetScope::Tenant {
            tenant: "acme".into(),
        })
        .unwrap_or_else(|| panic!("the budget exists"));
    assert_eq!(
        reserved, 0,
        "every admitted thread settled, and every refused thread left nothing reserved"
    );
    assert_eq!(
        u64::try_from(settled).unwrap_or(u64::MAX),
        u64::try_from(admitted).unwrap_or(u64::MAX),
        "the settled spend is one per admitted thread, never more"
    );
    assert!(
        u64::try_from(settled).unwrap_or(u64::MAX) <= 900,
        "settled spend stays inside the cap"
    );
}
