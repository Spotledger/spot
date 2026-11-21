//! Limits: every one of them is enforced, none can be escaped, and a killed execution leaves
//! nothing behind.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use serde_json::{Value, json};
use spot_ts::*;

fn engine(keep_bytecode: bool) -> ScriptEngine {
    ScriptEngine::new(EngineConfig {
        surface: surface(),
        keep_bytecode,
        ..EngineConfig::default()
    })
}

const MODES: [bool; 2] = [true, false];

fn run_with(
    e: &ScriptEngine,
    source: &str,
    args: Value,
    bridge: Arc<Recorder>,
    limits: Limits,
) -> Result<Value> {
    let compiled = e
        .compile(source, ScriptId::new("app", "limits", 1), None)
        .unwrap();
    e.run_blocking(&compiled, "f", &args, bridge, limits)
}

fn quick() -> Limits {
    Limits {
        wall_clock: Duration::from_millis(100),
        ..Limits::default()
    }
}

#[test]
fn an_endless_loop_hits_the_deadline() {
    for keep in MODES {
        let e = engine(keep);
        let started = Instant::now();
        let err = run_with(
            &e,
            "export function f() { for (;;) {} }",
            json!(null),
            Arc::new(Recorder::new()),
            quick(),
        )
        .unwrap_err();
        let took = started.elapsed();
        assert_eq!(err, ScriptError::Timeout { millis: 100 });
        assert_eq!(err.code(), "script.timeout");
        assert!(
            took >= Duration::from_millis(100) && took < Duration::from_millis(1_000),
            "{took:?}"
        );
    }
}

#[test]
fn the_instruction_budget_stops_a_loop_before_the_deadline() {
    for keep in MODES {
        let e = engine(keep);
        let limits = Limits {
            instructions: 5_000_000,
            wall_clock: Duration::from_secs(30),
            ..Limits::default()
        };
        let started = Instant::now();
        let err = run_with(
            &e,
            "export function f() { let i = 0; for (;;) { i++; } }",
            json!(null),
            Arc::new(Recorder::new()),
            limits,
        )
        .unwrap_err();
        assert_eq!(
            err,
            ScriptError::OutOfBudget {
                instructions: 5_000_000
            }
        );
        assert_eq!(err.code(), "script.out_of_budget");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
        // A script within its budget is not touched.
        let ok = run_with(
            &e,
            "export function f() { let s = 0; for (let i = 0; i < 1000; i++) s += i; return s; }",
            json!(null),
            Arc::new(Recorder::new()),
            limits,
        );
        assert_eq!(ok.unwrap(), json!(499_500));
    }
}

#[test]
fn a_script_cannot_catch_the_interrupt() {
    for keep in MODES {
        let e = engine(keep);
        let src = r#"
            export function f() {
              let caught = 0;
              for (;;) {
                try { for (;;) {} } catch (err) { caught++; }
                try { for (;;) {} } finally { caught++; }
              }
            }
        "#;
        let started = Instant::now();
        let err = run_with(&e, src, json!(null), Arc::new(Recorder::new()), quick()).unwrap_err();
        assert_eq!(err.code(), "script.timeout", "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        // Nor by promise handlers, nor by an async function awaiting in the loop.
        let src = "export async function f() { for (;;) { try { await Promise.resolve(); for (let i = 0; i < 1e6; i++) {} } catch (e) {} } }";
        let err = run_with(&e, src, json!(null), Arc::new(Recorder::new()), quick()).unwrap_err();
        assert_eq!(err.code(), "script.timeout", "{err}");
    }
}

#[test]
fn memory_past_the_limit_is_refused() {
    for keep in MODES {
        let e = engine(keep);
        let limits = Limits {
            memory_bytes: 4 * 1024 * 1024,
            wall_clock: Duration::from_secs(10),
            ..Limits::default()
        };
        for body in [
            "const a = []; for (;;) a.push(new Array(10000).fill(1));",
            "const m = new Map(); for (let i = 0; ; i++) m.set(i, { i });",
            "const b = new ArrayBuffer(64 * 1024 * 1024); return b.byteLength;",
        ] {
            let src = format!("export function f() {{ {body} }}");
            let err =
                run_with(&e, &src, json!(null), Arc::new(Recorder::new()), limits).unwrap_err();
            assert_eq!(
                err,
                ScriptError::MemoryLimit {
                    limit: 4 * 1024 * 1024
                },
                "{body}"
            );
            assert_eq!(err.code(), "script.memory_limit");
        }
        // Within the limit it is fine.
        let ok = run_with(
            &e,
            "export function f() { return new Array(1000).fill(7).length; }",
            json!(null),
            Arc::new(Recorder::new()),
            limits,
        );
        assert_eq!(ok.unwrap(), json!(1000));
    }
}

#[test]
fn a_script_that_catches_out_of_memory_stays_inside_the_limit() {
    for keep in MODES {
        let e = engine(keep);
        let limits = Limits {
            memory_bytes: 4 * 1024 * 1024,
            wall_clock: Duration::from_millis(300),
            ..Limits::default()
        };
        let src = "export function f() { const keep = []; for (;;) { try { keep.push(new Array(10000).fill(1)); } catch (e) { keep.length = 0; } } }";
        let err = run_with(&e, src, json!(null), Arc::new(Recorder::new()), limits).unwrap_err();
        // However it ends, it ends: either the memory error got through, or the deadline stopped it.
        assert!(
            matches!(err.code(), "script.memory_limit" | "script.timeout"),
            "{err}"
        );
    }
}

#[test]
fn runaway_recursion_overflows_the_stack() {
    for keep in MODES {
        let e = engine(keep);
        let err = run_with(
            &e,
            "export function f() { const r = (n) => r(n + 1) + 1; return r(0); }",
            json!(null),
            Arc::new(Recorder::new()),
            quick(),
        )
        .unwrap_err();
        assert_eq!(err, ScriptError::StackOverflow);
        assert_eq!(err.code(), "script.stack_overflow");
        // A deeper stack allowance lets more recursion through.
        let src =
            "export function f(n) { const r = (k) => k === 0 ? 0 : 1 + r(k - 1); return r(n); }";
        let small = Limits {
            stack_bytes: 64 * 1024,
            ..Limits::default()
        };
        let large = Limits {
            stack_bytes: 2 * 1024 * 1024,
            ..Limits::default()
        };
        assert_eq!(
            run_with(&e, src, json!(300), Arc::new(Recorder::new()), large).unwrap(),
            json!(300)
        );
        assert_eq!(
            run_with(&e, src, json!(100_000), Arc::new(Recorder::new()), small)
                .unwrap_err()
                .code(),
            "script.stack_overflow"
        );
    }
}

#[test]
fn host_calls_are_counted_and_the_overflow_is_not_served() {
    for keep in MODES {
        let e = engine(keep);
        let bridge = Arc::new(Recorder::new());
        let limits = Limits {
            host_calls: 50,
            ..Limits::default()
        };
        let err = run_with(
            &e,
            "export function f() { for (;;) { try { host.ping(1); } catch (e) {} } }",
            json!(null),
            bridge.clone(),
            limits,
        )
        .unwrap_err();
        assert_eq!(err, ScriptError::TooManyHostCalls { limit: 50 });
        assert_eq!(err.code(), "script.too_many_host_calls");
        // Exactly the limit was served, not one more, and the script cannot hide the fact by catching.
        assert_eq!(bridge.count.load(std::sync::atomic::Ordering::SeqCst), 50);
        // At exactly the limit it succeeds.
        let ok = run_with(
            &e,
            "export function f() { for (let i = 0; i < 50; i++) host.ping(i); return 'done'; }",
            json!(null),
            Arc::new(Recorder::new()),
            limits,
        );
        assert_eq!(ok.unwrap(), json!("done"));
    }
}

#[test]
fn a_huge_result_is_refused() {
    for keep in MODES {
        let e = engine(keep);
        let limits = Limits {
            output_bytes: 1024,
            ..Limits::default()
        };
        let err = run_with(
            &e,
            "export function f() { return 'x'.repeat(5000); }",
            json!(null),
            Arc::new(Recorder::new()),
            limits,
        )
        .unwrap_err();
        assert_eq!(err, ScriptError::OutputTooLarge { limit: 1024 });
        assert_eq!(err.code(), "script.output_too_large");
        let ok = run_with(
            &e,
            "export function f() { return 'x'.repeat(500); }",
            json!(null),
            Arc::new(Recorder::new()),
            limits,
        )
        .unwrap();
        assert_eq!(ok.as_str().map(str::len), Some(500));
    }
}

#[test]
fn a_hostile_compile_is_stopped_too() {
    let e = engine(true);
    // A pathological nesting depth must fail, not take the host down.
    let deep = format!(
        "export function f() {{ return {}1{}; }}",
        "(".repeat(100_000),
        ")".repeat(100_000)
    );
    let err = e
        .compile(&deep, ScriptId::new("app", "deep", 1), None)
        .unwrap_err();
    assert_eq!(err.code(), "script.compile", "{err}");
}

/// Resident memory of this process, in bytes (Linux).
#[cfg(target_os = "linux")]
fn resident_bytes() -> usize {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap();
    let pages: usize = statm.split_whitespace().nth(1).unwrap().parse().unwrap();
    pages * 4096
}

/// A thousand executions, each killed by a different limit, leave no memory behind: resident memory
/// grows by less than 24 MiB across the whole run (a leaked runtime of even 256 KiB would add
/// 250 MiB), and the next run still works.
#[test]
#[cfg(target_os = "linux")]
fn a_thousand_killed_executions_leak_nothing() {
    let e = engine(true);
    let scripts: Vec<(&str, Limits, &str)> = vec![
        (
            "for (;;) {}",
            Limits {
                wall_clock: Duration::from_millis(2),
                ..Limits::default()
            },
            "script.timeout",
        ),
        (
            "let i = 0; for (;;) i++;",
            Limits {
                instructions: 200_000,
                ..Limits::default()
            },
            "script.out_of_budget",
        ),
        (
            "const a = []; for (;;) a.push(new Array(1000).fill(1));",
            Limits {
                memory_bytes: 2 * 1024 * 1024,
                wall_clock: Duration::from_secs(5),
                ..Limits::default()
            },
            "script.memory_limit",
        ),
        (
            "const r = (n) => r(n + 1) + 1; return r(0);",
            Limits::default(),
            "script.stack_overflow",
        ),
        (
            "for (;;) { try { host.ping(0); } catch (e) {} }",
            Limits {
                host_calls: 20,
                ..Limits::default()
            },
            "script.too_many_host_calls",
        ),
        ("throw new Error('x');", Limits::default(), "script.thrown"),
    ];
    let compiled: Vec<_> = scripts
        .iter()
        .enumerate()
        .map(|(i, (body, _, _))| {
            e.compile(
                &format!("export function f() {{ {body} }}"),
                ScriptId::new("app", &format!("k{i}"), 1),
                None,
            )
            .unwrap()
        })
        .collect();
    let run_all = |rounds: usize| {
        for round in 0..rounds {
            for (i, c) in compiled.iter().enumerate() {
                let err = e
                    .run_blocking(
                        c,
                        "f",
                        &json!(null),
                        Arc::new(Recorder::new()),
                        scripts[i].1,
                    )
                    .unwrap_err();
                assert_eq!(err.code(), scripts[i].2, "round {round}, script {i}: {err}");
            }
        }
    };
    run_all(10); // warm up the allocator
    let before = resident_bytes();
    run_all(1_000 / scripts.len() + 1);
    let growth = resident_bytes().saturating_sub(before);
    println!(
        "1,000 killed executions grew resident memory by {} KiB",
        growth / 1024
    );
    assert!(
        growth < 24 * 1024 * 1024,
        "resident memory grew by {} KiB",
        growth / 1024
    );
    let ok = e
        .compile(
            "export function f() { return 'still fine'; }",
            ScriptId::new("app", "after", 1),
            None,
        )
        .unwrap();
    assert_eq!(
        e.run_blocking(
            &ok,
            "f",
            &json!(null),
            Arc::new(Recorder::new()),
            Limits::default()
        )
        .unwrap(),
        json!("still fine")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn running_a_script_does_not_block_the_reactor() {
    let e = engine(true);
    let busy = e
        .compile(
            "export function f() { for (;;) {} }",
            ScriptId::new("app", "busy", 1),
            None,
        )
        .unwrap();
    let ticks = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let counter = Arc::clone(&ticks);
    let ticker = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(5)).await;
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    });
    let limits = Limits {
        wall_clock: Duration::from_millis(300),
        ..Limits::default()
    };
    let err = e
        .run(&busy, "f", &json!(null), Arc::new(Recorder::new()), limits)
        .await
        .unwrap_err();
    ticker.abort();
    assert_eq!(err.code(), "script.timeout");
    // On a single-threaded runtime the ticker only ran because the script was on another thread.
    assert!(
        ticks.load(std::sync::atomic::Ordering::SeqCst) >= 20,
        "the reactor was blocked: {} ticks",
        ticks.load(std::sync::atomic::Ordering::SeqCst)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_executions_at_once_are_independent() {
    let e = engine(true);
    let good = e
        .compile(
            "export function f(n) { return { n, sum: [1, 2, 3].reduce((a, b) => a + b, n) }; }",
            ScriptId::new("app", "good", 1),
            None,
        )
        .unwrap();
    let bad = e
        .compile(
            "export function f() { for (;;) {} }",
            ScriptId::new("app", "bad", 1),
            None,
        )
        .unwrap();
    let mut tasks = Vec::new();
    for i in 0..200_u64 {
        let (e, good, bad) = (e.clone(), Arc::clone(&good), Arc::clone(&bad));
        tasks.push(tokio::spawn(async move {
            if i % 10 == 0 {
                let limits = Limits {
                    wall_clock: Duration::from_millis(50),
                    ..Limits::default()
                };
                let err = e
                    .run(&bad, "f", &json!(null), Arc::new(Recorder::new()), limits)
                    .await
                    .unwrap_err();
                assert_eq!(err.code(), "script.timeout");
            } else {
                let out = e
                    .run(
                        &good,
                        "f",
                        &json!(i),
                        Arc::new(Recorder::new()),
                        Limits::default(),
                    )
                    .await
                    .unwrap();
                assert_eq!(out, json!({"n": i, "sum": i + 6}));
            }
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
}
