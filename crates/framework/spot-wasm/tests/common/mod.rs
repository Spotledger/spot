//! Test components: written as WebAssembly text, compiled with `wat`, no toolchain target needed.
//!
//! Every fixture has the shape of `wit/test-world.wit`. The component skeleton ([`component`])
//! declares the world's types and the imports a fixture asks for, lowers them with the canonical
//! ABI (UTF-8 strings, a bump allocator in a `libc` core module), and lifts the core function
//! `on-event`, whose body each fixture supplies. To change a fixture, edit its `body` below: the
//! core function receives `($ptr, $len)` of the event string and returns a pointer to the
//! `result<string, error>` it wrote at address 1024 (`$ret_ok` and `$ret_err` write one).
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use spot_wasm::{Imports, LogLevel};

/// What a fixture needs on top of the skeleton.
pub struct Fixture {
    /// Which of the world's imports the component declares (`log`, `now`, `get-doc`).
    pub imports: &'static [&'static str],
    /// Extra component-level WAT: imports the host does not provide, for the denial tests.
    pub extra_component: &'static str,
    /// Extra core functions, placed before `on-event`.
    pub core_funcs: &'static str,
    /// The body of `on-event`: leaves an `i32` on the stack.
    pub body: &'static str,
}

impl Fixture {
    pub const fn new(body: &'static str) -> Self {
        Self {
            imports: &[],
            extra_component: "",
            core_funcs: "",
            body,
        }
    }

    pub const fn with_imports(mut self, imports: &'static [&'static str]) -> Self {
        self.imports = imports;
        self
    }

    pub const fn with_funcs(mut self, funcs: &'static str) -> Self {
        self.core_funcs = funcs;
        self
    }

    pub const fn with_extra(mut self, extra: &'static str) -> Self {
        self.extra_component = extra;
        self
    }
}

const SKELETON: &str = r#"
(component
  (type $level_def (enum "debug" "info" "warn" "error"))
  (import "level" (type $level (eq $level_def)))
  (type $error_def (record (field "code" string) (field "message" string)))
  (export $error "error" (type $error_def))
  __COMPONENT_IMPORTS__
  __EXTRA__

  (core module $libc
    (memory (export "memory") 2)
    (global $heap (mut i32) (i32.const 4096))
    (func (export "realloc") (param $old i32) (param $old_size i32) (param $align i32) (param $new_size i32) (result i32)
      (local $ptr i32) (local $end i32) (local $have i32)
      (local.set $ptr
        (i32.and
          (i32.add (global.get $heap) (i32.sub (local.get $align) (i32.const 1)))
          (i32.sub (i32.const 0) (local.get $align))))
      (local.set $end (i32.add (local.get $ptr) (local.get $new_size)))
      (local.set $have (i32.mul (memory.size) (i32.const 65536)))
      (if (i32.gt_u (local.get $end) (local.get $have))
        (then
          (drop (memory.grow
            (i32.add (i32.div_u (i32.sub (local.get $end) (local.get $have)) (i32.const 65536)) (i32.const 1))))))
      (global.set $heap (local.get $end))
      (local.get $ptr)))
  (core instance $libc (instantiate $libc))
  __LOWERED__

  (core module $app
    __CORE_IMPORTS__
    (import "libc" "memory" (memory 1))
    (data (i32.const 512) "customer")
    (data (i32.const 528) "not_found")
    (data (i32.const 544) "no such document")
    (data (i32.const 560) "supplier")
    (data (i32.const 576) "app_failed")
    (data (i32.const 592) "it failed on purpose")
    (func $ret_ok (param $p i32) (param $l i32) (result i32)
      (i32.store8 (i32.const 1024) (i32.const 0))
      (i32.store (i32.const 1028) (local.get $p))
      (i32.store (i32.const 1032) (local.get $l))
      (i32.const 1024))
    (func $ret_err (param $cp i32) (param $cl i32) (param $mp i32) (param $ml i32) (result i32)
      (i32.store8 (i32.const 1024) (i32.const 1))
      (i32.store (i32.const 1028) (local.get $cp))
      (i32.store (i32.const 1032) (local.get $cl))
      (i32.store (i32.const 1036) (local.get $mp))
      (i32.store (i32.const 1040) (local.get $ml))
      (i32.const 1024))
    __CORE_FUNCS__
    (func (export "on-event") (param $ptr i32) (param $len i32) (result i32)
      __BODY__))

  (core instance $app (instantiate $app
    (with "host" (instance __HOST_EXPORTS__))
    (with "libc" (instance $libc))))
  (func $on_event (param "event" string) (result (result string (error $error)))
    (canon lift (core func $app "on-event")
      (memory (core memory $libc "memory")) (realloc (core func $libc "realloc")) string-encoding=utf8))
  (export "on-event" (func $on_event)))
"#;

/// The component for a fixture, as binary.
pub fn component(fixture: &Fixture) -> Vec<u8> {
    let mut component_imports = String::new();
    let mut lowered = String::new();
    let mut core_imports = String::new();
    let mut host_exports = String::new();
    let lower_opts = r#"(memory (core memory $libc "memory")) (realloc (core func $libc "realloc")) string-encoding=utf8"#;
    for import in fixture.imports {
        match *import {
            "log" => {
                component_imports.push_str(
                    r#"(import "log" (func $log (param "level" $level) (param "message" string)))"#,
                );
                lowered.push_str(&format!(
                    "(core func $log_l (canon lower (func $log) {lower_opts}))"
                ));
                core_imports.push_str(r#"(import "host" "log" (func $log (param i32 i32 i32)))"#);
                host_exports.push_str(r#"(export "log" (func $log_l))"#);
            }
            "now" => {
                component_imports.push_str(r#"(import "now" (func $now (result s64)))"#);
                lowered.push_str("(core func $now_l (canon lower (func $now)))");
                core_imports.push_str(r#"(import "host" "now" (func $now (result i64)))"#);
                host_exports.push_str(r#"(export "now" (func $now_l))"#);
            }
            "get-doc" => {
                component_imports.push_str(r#"(import "get-doc" (func $get-doc (param "doctype" string) (param "id" string) (result (option string))))"#);
                lowered.push_str(&format!(
                    "(core func $get_doc_l (canon lower (func $get-doc) {lower_opts}))"
                ));
                core_imports.push_str(
                    r#"(import "host" "get-doc" (func $get_doc (param i32 i32 i32 i32 i32)))"#,
                );
                host_exports.push_str(r#"(export "get-doc" (func $get_doc_l))"#);
            }
            other => panic!("unknown import {other}"),
        }
        component_imports.push('\n');
        lowered.push('\n');
        core_imports.push('\n');
    }
    let text = SKELETON
        .replace("__COMPONENT_IMPORTS__", &component_imports)
        .replace("__EXTRA__", fixture.extra_component)
        .replace("__LOWERED__", &lowered)
        .replace("__CORE_IMPORTS__", &core_imports)
        .replace("__CORE_FUNCS__", fixture.core_funcs)
        .replace("__BODY__", fixture.body)
        .replace("__HOST_EXPORTS__", &host_exports);
    wat::parse_str(&text).unwrap_or_else(|e| panic!("the fixture does not assemble: {e}\n{text}"))
}

// --- the fixtures -----------------------------------------------------------------------------

/// Returns its argument.
pub const ECHO: Fixture = Fixture::new("(call $ret_ok (local.get $ptr) (local.get $len))");

/// Logs the event, reads the clock, fetches the `customer` document named by the event and returns
/// it, or an error if there is none.
pub const USES_IMPORTS: Fixture = Fixture::new(
    r#"
    (call $log (i32.const 1) (local.get $ptr) (local.get $len))
    (drop (call $now))
    (call $get_doc (i32.const 512) (i32.const 8) (local.get $ptr) (local.get $len) (i32.const 2048))
    (if (result i32) (i32.load8_u (i32.const 2048))
      (then (call $ret_ok (i32.load (i32.const 2052)) (i32.load (i32.const 2056))))
      (else (call $ret_err (i32.const 528) (i32.const 9) (i32.const 544) (i32.const 16))))
    "#,
)
.with_imports(&["log", "now", "get-doc"]);

/// Reads a `supplier` document, which a test grants or does not.
pub const READS_SUPPLIER: Fixture = Fixture::new(
    r#"
    (call $get_doc (i32.const 560) (i32.const 8) (local.get $ptr) (local.get $len) (i32.const 2048))
    (call $ret_ok (local.get $ptr) (local.get $len))
    "#,
)
.with_imports(&["get-doc"]);

/// Always returns the app's own error.
pub const FAILS_ON_PURPOSE: Fixture =
    Fixture::new("(call $ret_err (i32.const 576) (i32.const 10) (i32.const 592) (i32.const 20))");

/// Loops for ever, doing nothing.
pub const SPIN: Fixture = Fixture::new("(loop $l (br $l)) (i32.const 0)");

/// Grows its memory a megabyte at a time until something stops it.
pub const GROW_FOREVER: Fixture = Fixture::new(
    r#"
    (loop $again
      (if (i32.eq (memory.grow (i32.const 16)) (i32.const -1))
        (then (return (i32.const 1024))))
      (br $again))
    (i32.const 1024)
    "#,
);

/// Executes `unreachable`.
pub const TRAPS: Fixture = Fixture::new("unreachable");

/// Recurses without end.
pub const RECURSES: Fixture = Fixture::new("(call $rec (i32.const 0))").with_funcs(
    "(func $rec (param i32) (result i32) (call $rec (i32.add (local.get 0) (i32.const 1))))",
);

/// Returns an 8 MB string.
pub const HUGE_OUTPUT: Fixture = Fixture::new(
    r#"
    (drop (memory.grow (i32.const 130)))
    (memory.fill (i32.const 140000) (i32.const 97) (i32.const 8000000))
    (call $ret_ok (i32.const 140000) (i32.const 8000000))
    "#,
);

/// Calls `now` for ever: host calls without end.
pub const CHATTY: Fixture =
    Fixture::new("(loop $l (drop (call $now)) (br $l)) (i32.const 0)").with_imports(&["now"]);

/// Fails if memory left by an earlier call is visible: proves nothing survives between calls.
pub const REMEMBERS: Fixture = Fixture::new(
    r#"
    (if (i32.load8_u (i32.const 700))
      (then (return (call $ret_err (i32.const 576) (i32.const 10) (i32.const 592) (i32.const 20)))))
    (i32.store8 (i32.const 700) (i32.const 1))
    (call $ret_ok (local.get $ptr) (local.get $len))
    "#,
);

/// Imports a function the host does not provide.
pub const WANTS_MAIL: Fixture = Fixture::new("(call $ret_ok (local.get $ptr) (local.get $len))")
    .with_extra(r#"(import "send-mail" (func (param "to" string) (param "body" string)))"#);

/// Imports a WASI-style interface (files, clocks, environment are never provided).
pub const WANTS_WASI: Fixture = Fixture::new("(call $ret_ok (local.get $ptr) (local.get $len))").with_extra(
    r#"(import "wasi:cli/environment@0.2.0" (instance (export "get-environment" (func (result (list (tuple string string)))))))"#,
);

// --- a recording host side -------------------------------------------------------------------

/// Records what the app did and answers `get-doc` for the documents it knows.
#[derive(Default)]
pub struct Recorder {
    pub logs: Mutex<Vec<(LogLevel, String)>>,
    pub nows: AtomicU32,
    pub docs: Vec<(String, String, String)>,
}

impl Recorder {
    pub fn with_doc(doctype: &str, id: &str, json: &str) -> Self {
        Self {
            docs: vec![(doctype.into(), id.into(), json.into())],
            ..Self::default()
        }
    }
}

impl Imports for Recorder {
    fn log(&self, level: LogLevel, message: &str) {
        self.logs.lock().unwrap().push((level, message.to_owned()));
    }

    fn now(&self) -> i64 {
        self.nows.fetch_add(1, Ordering::SeqCst);
        1_790_000_000_000_000
    }

    fn get_doc(&self, doctype: &str, id: &str) -> Option<String> {
        self.docs
            .iter()
            .find(|(t, i, _)| t == doctype && i == id)
            .map(|(_, _, json)| json.clone())
    }
}
