//! Costs, as tests with loose ceilings. The numbers (printed with `--nocapture`) are recorded in the
//! README; the ceilings only catch an order-of-magnitude regression, so they are generous for a
//! debug build on a busy machine.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use serde_json::json;
use spot_wasm::{GrantedCapabilities, HostConfig, Imports, Limits, WasmHost};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cold_instantiate_and_warm_pooled_calls() {
    let started = Instant::now();
    let host = WasmHost::new(HostConfig::default()).unwrap();
    let engine_time = started.elapsed();

    let bytes = component(&USES_IMPORTS);
    let started = Instant::now();
    let compiled = host.compile(&bytes).unwrap();
    let compile_time = started.elapsed();

    let mut caps = GrantedCapabilities::default();
    caps.doctypes.read.push("customer".into());
    let started = Instant::now();
    let app = host.instantiate(&compiled, caps).unwrap();
    let link_time = started.elapsed();

    let imports: Arc<dyn Imports> = Arc::new(Recorder::with_doc("customer", "c-1", "{}"));
    let args = json!(["c-1"]);
    // The first call: a cold slot (the pool maps memory for the first time).
    let started = Instant::now();
    app.call("on-event", &args, Limits::default(), imports.clone())
        .await
        .unwrap();
    let first_call = started.elapsed();
    // Warm: the same slots again.
    const CALLS: u32 = 500;
    let started = Instant::now();
    for _ in 0..CALLS {
        app.call("on-event", &args, Limits::default(), imports.clone())
            .await
            .unwrap();
    }
    let warm = started.elapsed() / CALLS;

    println!("engine and pool build:        {engine_time:?}");
    println!("compile (validate + codegen):  {compile_time:?}");
    println!("link (instantiate_pre):        {link_time:?}");
    println!("first call (cold slot):        {first_call:?}");
    println!("warm pooled call, mean of {CALLS}: {warm:?}");

    assert!(engine_time < Duration::from_secs(5), "{engine_time:?}");
    assert!(compile_time < Duration::from_secs(5), "{compile_time:?}");
    assert!(link_time < Duration::from_secs(1), "{link_time:?}");
    assert!(first_call < Duration::from_secs(1), "{first_call:?}");
    assert!(warm < Duration::from_millis(20), "{warm:?}");
}
