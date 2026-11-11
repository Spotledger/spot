//! Costs, as tests with loose ceilings. The numbers (printed with `--nocapture`) are in the README;
//! the ceilings only catch an order-of-magnitude regression, so a slow shared runner does not flake.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use serde_json::json;
use spot_ts::*;

fn mean(runs: u32, mut f: impl FnMut()) -> Duration {
    let started = Instant::now();
    for _ in 0..runs {
        f();
    }
    started.elapsed() / runs
}

#[test]
fn cold_start_a_trivial_call_and_ten_host_calls() {
    for keep in [true, false] {
        let engine = ScriptEngine::new(EngineConfig {
            surface: surface(),
            keep_bytecode: keep,
            ..EngineConfig::default()
        });
        let trivial = engine
            .compile(
                "export function f(x) { return x + 1; }",
                ScriptId::new("app", "trivial", 1),
                None,
            )
            .unwrap();
        let ten = engine
            .compile("export function f() { let s = 0; for (let i = 0; i < 10; i++) { s += host.echo(i); } return s; }", ScriptId::new("app", "ten", 1), None)
            .unwrap();
        let bridge: Arc<Recorder> = Arc::new(Recorder::new());

        let started = Instant::now();
        engine
            .run_blocking(&trivial, "f", &json!(1), bridge.clone(), Limits::default())
            .unwrap();
        let cold = started.elapsed();
        let call = mean(300, || {
            engine
                .run_blocking(&trivial, "f", &json!(1), bridge.clone(), Limits::default())
                .unwrap();
        });
        let ten_calls = mean(300, || {
            engine
                .run_blocking(&ten, "f", &json!(null), bridge.clone(), Limits::default())
                .unwrap();
        });
        let compile = mean(50, || {
            engine
                .compile(
                    "export function f(x) { return x * 2; }",
                    ScriptId::new("app", "fresh", 1),
                    None,
                )
                .unwrap();
        });
        let label = if keep { "bytecode" } else { "source  " };
        println!(
            "[{label}] first (cold) call: {cold:?}; trivial call, mean of 300: {call:?}; ten host calls: {ten_calls:?}; compile: {compile:?}"
        );
        assert!(cold < Duration::from_millis(50), "cold start {cold:?}");
        assert!(call < Duration::from_millis(20), "trivial call {call:?}");
        assert!(
            ten_calls < Duration::from_millis(30),
            "ten host calls {ten_calls:?}"
        );
    }
}
