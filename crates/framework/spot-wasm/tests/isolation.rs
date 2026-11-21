//! Isolation: hostile calls change nothing for anyone else.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use serde_json::json;
use spot_wasm::{AppInstance, GrantedCapabilities, HostConfig, Imports, Limits, WasmHost};

fn app(host: &WasmHost, fixture: &Fixture) -> AppInstance {
    let component = host.compile(&component(fixture)).unwrap();
    host.instantiate(&component, GrantedCapabilities::default())
        .unwrap()
}

fn quiet() -> Arc<dyn Imports> {
    Arc::new(Recorder::default())
}

fn hostile_limits() -> Limits {
    Limits {
        fuel: 5_000_000_000,
        wall_clock: Duration::from_millis(150),
        memory_bytes: 4 * 1024 * 1024,
        host_calls: 200,
        output_bytes: 1024 * 1024,
        ..Limits::default()
    }
}

/// 200 concurrent calls, 20 of them hostile (loops, memory hogs, traps, runaway recursion, floods
/// of host calls, huge results). Every well-behaved call gets its own answer; every hostile one gets
/// its own typed error; the pool ends up whole and keeps working.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_hundred_calls_twenty_of_them_hostile() {
    let host = WasmHost::new(HostConfig::default()).unwrap();
    let echo = app(&host, &ECHO);
    let hostile: Vec<(AppInstance, &str)> = vec![
        (app(&host, &SPIN), "wasm.timeout"),
        (app(&host, &GROW_FOREVER), "wasm.memory"),
        (app(&host, &TRAPS), "wasm.trap"),
        (app(&host, &RECURSES), "wasm.trap"),
        (app(&host, &CHATTY), "wasm.too_many_host_calls"),
        (app(&host, &HUGE_OUTPUT), "wasm.output_too_large"),
    ];
    let started = Instant::now();
    let mut tasks = Vec::new();
    for i in 0..200_usize {
        let echo = echo.clone();
        let hostile_app = (i % 10 == 3).then(|| hostile[(i / 10) % hostile.len()].clone());
        tasks.push(tokio::spawn(async move {
            match hostile_app {
                Some((app, want)) => {
                    let limits = Limits {
                        memory_bytes: if want == "wasm.output_too_large" {
                            16 * 1024 * 1024
                        } else {
                            4 * 1024 * 1024
                        },
                        ..hostile_limits()
                    };
                    let err = app
                        .call("on-event", &json!(["x"]), limits, quiet())
                        .await
                        .unwrap_err();
                    assert_eq!(err.code(), want, "call {i}: {err}");
                    true
                }
                None => {
                    let text = format!("call-{i}");
                    let out = echo
                        .call("on-event", &json!([text]), Limits::default(), quiet())
                        .await
                        .unwrap();
                    assert_eq!(out, json!(text), "call {i} got someone else's answer");
                    false
                }
            }
        }));
    }
    let mut hostile_count = 0;
    for task in tasks {
        hostile_count += usize::from(task.await.unwrap());
    }
    assert_eq!(hostile_count, 20);
    assert!(
        started.elapsed() < Duration::from_secs(60),
        "{:?}",
        started.elapsed()
    );

    // The pool is whole: every slot is back.
    assert_eq!(host.available_slots(), host.slots());
    // And it still serves, at full width.
    let mut later = Vec::new();
    for i in 0..(2 * host.slots()) {
        let echo = echo.clone();
        later.push(tokio::spawn(async move {
            echo.call(
                "on-event",
                &json!([format!("later-{i}")]),
                Limits::default(),
                quiet(),
            )
            .await
        }));
    }
    for (i, task) in later.into_iter().enumerate() {
        assert_eq!(task.await.unwrap().unwrap(), json!(format!("later-{i}")));
    }
    assert_eq!(host.available_slots(), host.slots());
}

/// A guest stuck in a loop yields to the runtime every so often, so on a two-thread runtime with
/// more loops than threads an ordinary call still finishes promptly.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hostile_loops_do_not_starve_other_calls() {
    let host = WasmHost::new(HostConfig::default()).unwrap();
    let echo = app(&host, &ECHO);
    let spin = app(&host, &SPIN);
    let mut spinners = Vec::new();
    for _ in 0..8 {
        let spin = spin.clone();
        spinners.push(tokio::spawn(async move {
            spin.call(
                "on-event",
                &json!(["x"]),
                Limits {
                    fuel: u64::MAX / 2,
                    wall_clock: Duration::from_millis(1_500),
                    ..Limits::default()
                },
                quiet(),
            )
            .await
        }));
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let started = Instant::now();
    for _ in 0..10 {
        echo.call("on-event", &json!(["hi"]), Limits::default(), quiet())
            .await
            .unwrap();
    }
    assert!(
        started.elapsed() < Duration::from_millis(1_000),
        "ten ordinary calls took {:?} while eight loops ran",
        started.elapsed()
    );
    for spinner in spinners {
        assert_eq!(spinner.await.unwrap().unwrap_err().code(), "wasm.timeout");
    }
    assert_eq!(host.available_slots(), host.slots());
}

/// More calls than slots wait their turn instead of failing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn more_calls_than_slots_wait_for_a_slot() {
    let host = WasmHost::new(HostConfig {
        max_concurrent_calls: 4,
        ..HostConfig::default()
    })
    .unwrap();
    let spin = app(&host, &SPIN);
    let echo = app(&host, &ECHO);
    let mut tasks = Vec::new();
    // Four spinners hold every slot for 200 ms; the echoes queue behind them.
    for _ in 0..4 {
        let spin = spin.clone();
        tasks.push(tokio::spawn(async move {
            spin.call(
                "on-event",
                &json!(["x"]),
                Limits {
                    fuel: u64::MAX / 2,
                    wall_clock: Duration::from_millis(200),
                    ..Limits::default()
                },
                quiet(),
            )
            .await
            .is_err()
        }));
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(host.available_slots(), 0);
    let started = Instant::now();
    let out = echo
        .call("on-event", &json!(["queued"]), Limits::default(), quiet())
        .await
        .unwrap();
    assert_eq!(out, json!("queued"));
    assert!(
        started.elapsed() >= Duration::from_millis(100),
        "the echo should have waited for a slot: {:?}",
        started.elapsed()
    );
    for task in tasks {
        assert!(task.await.unwrap());
    }
    assert_eq!(host.available_slots(), 4);
}
