//! The host end to end: a component built from text is compiled, linked and called.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use serde_json::json;
use spot_wasm::{GrantedCapabilities, HostConfig, Imports, Limits, LogLevel, WasmError, WasmHost};

fn host() -> WasmHost {
    WasmHost::new(HostConfig::default()).unwrap()
}

fn read_customers() -> GrantedCapabilities {
    let mut caps = GrantedCapabilities::default();
    caps.doctypes.read.push("customer".into());
    caps
}

fn app(host: &WasmHost, fixture: &Fixture, caps: GrantedCapabilities) -> spot_wasm::AppInstance {
    let component = host.compile(&component(fixture)).unwrap();
    host.instantiate(&component, caps).unwrap()
}

fn quiet() -> Arc<dyn Imports> {
    Arc::new(Recorder::default())
}

#[tokio::test]
async fn a_well_behaved_app_is_called_and_answers() {
    let host = host();
    let echo = app(&host, &ECHO, GrantedCapabilities::default());
    let out = echo
        .call("on-event", &json!(["hello"]), Limits::default(), quiet())
        .await
        .unwrap();
    assert_eq!(out, json!("hello"));
    let out = echo
        .call(
            "on-event",
            &json!(["héllo wörld ✓"]),
            Limits::default(),
            quiet(),
        )
        .await
        .unwrap();
    assert_eq!(out, json!("héllo wörld ✓"));
}

#[tokio::test]
async fn imports_reach_the_host_and_only_through_the_granted_interface() {
    let host = host();
    let uses = app(&host, &USES_IMPORTS, read_customers());
    let recorder = Arc::new(Recorder::with_doc("customer", "c-1", r#"{"name":"Acme"}"#));
    let out = uses
        .call(
            "on-event",
            &json!(["c-1"]),
            Limits::default(),
            recorder.clone(),
        )
        .await
        .unwrap();
    assert_eq!(out, json!(r#"{"name":"Acme"}"#));
    assert_eq!(
        recorder.logs.lock().unwrap().as_slice(),
        [(LogLevel::Info, "c-1".to_owned())]
    );
    assert_eq!(recorder.nows.load(std::sync::atomic::Ordering::SeqCst), 1);
    // A document that does not exist is the app's own error, with its fields.
    let err = uses
        .call("on-event", &json!(["nobody"]), Limits::default(), recorder)
        .await
        .unwrap_err();
    assert_eq!(
        err,
        WasmError::App(json!({"code": "not_found", "message": "no such document"}))
    );
    assert_eq!(err.code(), "wasm.app_error");
}

#[tokio::test]
async fn an_app_error_is_the_apps_not_the_hosts() {
    let host = host();
    let fails = app(&host, &FAILS_ON_PURPOSE, GrantedCapabilities::default());
    let err = fails
        .call("on-event", &json!(["x"]), Limits::default(), quiet())
        .await
        .unwrap_err();
    assert_eq!(
        err,
        WasmError::App(json!({"code": "app_failed", "message": "it failed on purpose"}))
    );
    // And the host is fine: the next call works.
    let echo = app(&host, &ECHO, GrantedCapabilities::default());
    assert!(
        echo.call("on-event", &json!(["x"]), Limits::default(), quiet())
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn nothing_survives_between_calls() {
    let host = host();
    let remembers = app(&host, &REMEMBERS, GrantedCapabilities::default());
    for _ in 0..5 {
        // The second call would fail if it saw the first call's memory.
        let out = remembers
            .call("on-event", &json!(["again"]), Limits::default(), quiet())
            .await
            .unwrap();
        assert_eq!(out, json!("again"));
    }
}

fn limits() -> Limits {
    Limits {
        fuel: 50_000_000,
        wall_clock: Duration::from_millis(500),
        memory_bytes: 4 * 1024 * 1024,
        ..Limits::default()
    }
}

#[tokio::test]
async fn an_endless_loop_runs_out_of_fuel() {
    let host = host();
    let spin = app(&host, &SPIN, GrantedCapabilities::default());
    let started = Instant::now();
    let err = spin
        .call(
            "on-event",
            &json!(["x"]),
            Limits {
                fuel: 1_000_000,
                wall_clock: Duration::from_secs(20),
                ..limits()
            },
            quiet(),
        )
        .await
        .unwrap_err();
    assert_eq!(err, WasmError::OutOfFuel { fuel: 1_000_000 });
    assert_eq!(err.code(), "wasm.out_of_fuel");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn an_endless_loop_is_killed_on_timeout() {
    let host = host();
    let spin = app(&host, &SPIN, GrantedCapabilities::default());
    let wall = Duration::from_millis(150);
    let started = Instant::now();
    let err = spin
        .call(
            "on-event",
            &json!(["x"]),
            Limits {
                fuel: u64::MAX / 2,
                wall_clock: wall,
                ..limits()
            },
            quiet(),
        )
        .await
        .unwrap_err();
    let took = started.elapsed();
    assert_eq!(err, WasmError::Timeout { millis: 150 });
    assert_eq!(err.code(), "wasm.timeout");
    // At least the limit, and stopped within a tick or two of it (plus scheduling slack).
    assert!(took >= wall, "{took:?}");
    assert!(took < wall + Duration::from_millis(400), "{took:?}");
}

#[tokio::test]
async fn memory_past_the_limit_is_refused() {
    let host = host();
    let grow = app(&host, &GROW_FOREVER, GrantedCapabilities::default());
    let err = grow
        .call("on-event", &json!(["x"]), limits(), quiet())
        .await
        .unwrap_err();
    assert_eq!(err.code(), "wasm.memory");
    assert!(err.to_string().contains("the limit is 4194304"), "{err}");
    // A limit above the pool's slab is clamped to it, never exceeded.
    let err = grow
        .call(
            "on-event",
            &json!(["x"]),
            Limits {
                memory_bytes: usize::MAX / 2,
                wall_clock: Duration::from_secs(10),
                ..limits()
            },
            quiet(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), "wasm.memory");
}

#[tokio::test]
async fn traps_and_stack_overflow_are_typed_traps() {
    let host = host();
    let traps = app(&host, &TRAPS, GrantedCapabilities::default());
    let err = traps
        .call("on-event", &json!(["x"]), limits(), quiet())
        .await
        .unwrap_err();
    assert_eq!(err.code(), "wasm.trap");
    assert!(err.to_string().contains("unreachable"), "{err}");
    let recurses = app(&host, &RECURSES, GrantedCapabilities::default());
    let err = recurses
        .call("on-event", &json!(["x"]), limits(), quiet())
        .await
        .unwrap_err();
    assert_eq!(err.code(), "wasm.trap");
    assert!(err.to_string().contains("stack overflowed"), "{err}");
}

#[tokio::test]
async fn host_calls_are_counted() {
    let host = host();
    let chatty = app(&host, &CHATTY, GrantedCapabilities::default());
    let recorder = Arc::new(Recorder::default());
    let err = chatty
        .call(
            "on-event",
            &json!(["x"]),
            Limits {
                host_calls: 50,
                ..limits()
            },
            recorder.clone(),
        )
        .await
        .unwrap_err();
    assert_eq!(err, WasmError::TooManyHostCalls { limit: 50 });
    assert_eq!(err.code(), "wasm.too_many_host_calls");
    // Exactly 50 were served; the 51st was refused before it reached the host.
    assert_eq!(recorder.nows.load(std::sync::atomic::Ordering::SeqCst), 50);
}

#[tokio::test]
async fn a_huge_result_is_refused() {
    let host = host();
    let huge = app(&host, &HUGE_OUTPUT, GrantedCapabilities::default());
    let err = huge
        .call(
            "on-event",
            &json!(["x"]),
            Limits {
                memory_bytes: 16 * 1024 * 1024,
                output_bytes: 1024 * 1024,
                ..limits()
            },
            quiet(),
        )
        .await
        .unwrap_err();
    assert_eq!(err, WasmError::OutputTooLarge { limit: 1024 * 1024 });
    assert_eq!(err.code(), "wasm.output_too_large");
    // The same result is fine under a limit that allows it.
    let out = huge
        .call(
            "on-event",
            &json!(["x"]),
            Limits {
                memory_bytes: 16 * 1024 * 1024,
                output_bytes: 10_000_000,
                wall_clock: Duration::from_secs(5),
                ..limits()
            },
            quiet(),
        )
        .await
        .unwrap();
    assert_eq!(out.as_str().unwrap().len(), 8_000_000);
}

#[tokio::test]
async fn bad_calls_are_typed_errors() {
    let host = host();
    let echo = app(&host, &ECHO, GrantedCapabilities::default());
    let err = echo
        .call("no-such-export", &json!(["x"]), limits(), quiet())
        .await
        .unwrap_err();
    assert_eq!(err, WasmError::MissingExport("no-such-export".into()));
    assert_eq!(err.code(), "wasm.missing_export");
    for (args, needle) in [
        (json!([]), "takes 1 argument(s), found 0"),
        (json!(["a", "b"]), "takes 1 argument(s), found 2"),
        (json!([7]), "expected a string"),
        (json!({"event": "x"}), "must be a JSON array"),
    ] {
        let err = echo
            .call("on-event", &args, limits(), quiet())
            .await
            .unwrap_err();
        assert_eq!(err.code(), "wasm.bad_arguments", "{args}");
        assert!(err.to_string().contains(needle), "{err}");
    }
}

#[test]
fn what_is_not_a_component_is_refused_at_compile_time() {
    let host = host();
    for bytes in [
        b"not wasm at all".to_vec(),
        vec![],
        wat::parse_str("(module)").unwrap(),
    ] {
        let err = host.compile(&bytes).err().expect("must be refused");
        assert_eq!(err.code(), "wasm.bad_component", "{err}");
    }
}

#[test]
fn an_app_cannot_import_what_it_was_not_granted() {
    let host = host();
    // An import the host does not provide at all.
    for (fixture, name) in [
        (&WANTS_MAIL, "send-mail"),
        (&WANTS_WASI, "wasi:cli/environment@0.2.0"),
    ] {
        let component = host.compile(&component(fixture)).unwrap();
        // Not even full capabilities help: there is nothing to link it to.
        let mut everything = read_customers();
        everything.email = true;
        everything.files = spot_wasm::FileAccess::ReadWrite;
        let err = host
            .instantiate(&component, everything)
            .err()
            .expect("must be denied");
        assert_eq!(err.code(), "wasm.capability_denied");
        assert!(err.to_string().contains(name), "{err}");
    }
    // A gated import without the grant, and with it.
    let component = host.compile(&component(&USES_IMPORTS)).unwrap();
    let err = host
        .instantiate(&component, GrantedCapabilities::default())
        .err()
        .expect("must be denied");
    assert_eq!(err.code(), "wasm.capability_denied");
    assert!(
        err.to_string().contains("get-doc") && err.to_string().contains("no DocType read access"),
        "{err}"
    );
    assert!(host.instantiate(&component, read_customers()).is_ok());
    // Imports that carry no authority need no grant.
    let logs = Fixture::new("(call $ret_ok (local.get $ptr) (local.get $len))")
        .with_imports(&["log", "now"]);
    let component = host.compile(&component_of(&logs)).unwrap();
    assert!(
        host.instantiate(&component, GrantedCapabilities::default())
            .is_ok()
    );
}

fn component_of(f: &Fixture) -> Vec<u8> {
    component(f)
}

#[tokio::test]
async fn a_granted_import_is_still_checked_per_document_type() {
    let host = host();
    // `get-doc` is granted (read access to customers), but the app asks for a supplier.
    let reads = app(&host, &READS_SUPPLIER, read_customers());
    let recorder = Arc::new(Recorder::with_doc("supplier", "s-1", "{}"));
    let err = reads
        .call("on-event", &json!(["s-1"]), limits(), recorder)
        .await
        .unwrap_err();
    assert_eq!(err.code(), "wasm.capability_denied");
    assert!(err.to_string().contains("`supplier`"), "{err}");
}

#[test]
fn the_compiled_component_reports_its_hash_imports_and_exports() {
    let host = host();
    let bytes = component(&USES_IMPORTS);
    let compiled = host.compile(&bytes).unwrap();
    assert_eq!(compiled.content_hash().len(), 64);
    assert_eq!(
        compiled.content_hash(),
        host.compile(&bytes).unwrap().content_hash()
    );
    assert_ne!(
        compiled.content_hash(),
        host.compile(&component(&ECHO)).unwrap().content_hash()
    );
    assert_eq!(compiled.imports(), ["get-doc", "log", "now"]);
    assert_eq!(compiled.exports(), ["error", "on-event"]);
    assert_eq!(
        host.instantiate(&compiled, read_customers())
            .unwrap()
            .content_hash(),
        compiled.content_hash()
    );
}

/// The fixtures implement `wit/test-world.wit`: the export and import types are the world's.
#[test]
fn the_fixtures_have_the_shape_of_the_test_world() {
    use wasmtime::component::Type;
    use wasmtime::component::types::ComponentItem;
    let host = host();
    let bytes = component(&USES_IMPORTS);
    let compiled = host.compile(&bytes).unwrap();
    // Reach the engine through a second compile of the same bytes to inspect types.
    let engine = wasmtime::Engine::new(wasmtime::Config::new().wasm_component_model(true)).unwrap();
    let component = wasmtime::component::Component::new(&engine, &bytes).unwrap();
    let ty = component.component_type();
    let exports: Vec<_> = ty.exports(&engine).collect();
    let on_event = exports
        .iter()
        .find(|(name, _)| *name == "on-event")
        .map(|(_, item)| &item.ty)
        .unwrap();
    let ComponentItem::ComponentFunc(func) = on_event else {
        panic!("on-event is not a function")
    };
    let params: Vec<_> = func.params().collect();
    assert_eq!(params.len(), 1);
    assert_eq!((params[0].0, &params[0].1), ("event", &Type::String));
    let results: Vec<_> = func.results().collect();
    let Type::Result(result) = &results[0] else {
        panic!("the result is not a result")
    };
    assert_eq!(result.ok(), Some(Type::String));
    let Some(Type::Record(error)) = result.err() else {
        panic!("the error is not a record")
    };
    assert_eq!(
        error
            .fields()
            .map(|f| f.name.to_owned())
            .collect::<Vec<_>>(),
        ["code", "message"]
    );
    let imports: Vec<_> = ty
        .imports(&engine)
        .map(|(name, _)| name.to_owned())
        .collect();
    assert_eq!(imports, ["log", "now", "get-doc"]);
    assert_eq!(compiled.imports().len(), 3);
}
