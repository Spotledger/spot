//! The seeded corpus: bundles built at test time from WebAssembly text, no committed binaries.
//!
//! The component skeleton mirrors `spot-wasm/tests/common`: it declares the test world's types
//! and the imports a fixture asks for, lowers them with the canonical ABI, and lifts the core
//! function `on-event`, whose body each fixture supplies. A fixture's body receives
//! `($ptr, $len)` of the event string and returns a pointer to the `result<string, error>` it
//! wrote at address 1024 (`$ret_ok` writes one).
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_bundle::{Bundle, BundleBuilder, HostInfo, Limits};
use spot_pack::PackOptions;

/// What a fixture needs on top of the skeleton.
pub struct Fixture {
    /// Which of the world's imports the component declares (`log`, `now`, `get-doc`).
    pub imports: &'static [&'static str],
    /// Extra core functions, placed before `on-event`.
    pub core_funcs: &'static str,
    /// The body of `on-event`: leaves an `i32` on the stack.
    pub body: &'static str,
}

impl Fixture {
    /// A fixture with no imports and no extra functions.
    pub const fn new(body: &'static str) -> Self {
        Self {
            imports: &[],
            core_funcs: "",
            body,
        }
    }

    /// The fixture with the named imports.
    pub const fn with_imports(mut self, imports: &'static [&'static str]) -> Self {
        self.imports = imports;
        self
    }

    /// The fixture with extra core functions.
    pub const fn with_funcs(mut self, funcs: &'static str) -> Self {
        self.core_funcs = funcs;
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
        .replace("__LOWERED__", &lowered)
        .replace("__CORE_IMPORTS__", &core_imports)
        .replace("__CORE_FUNCS__", fixture.core_funcs)
        .replace("__BODY__", fixture.body)
        .replace("__HOST_EXPORTS__", &host_exports);
    wat::parse_str(&text).unwrap_or_else(|e| panic!("the fixture does not assemble: {e}\n{text}"))
}

// --- the fixtures ---------------------------------------------------------------------------

/// Returns its argument.
pub const ECHO: Fixture = Fixture::new("(call $ret_ok (local.get $ptr) (local.get $len))");

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

/// Calls `now` for ever: host calls without end.
pub const CHATTY: Fixture =
    Fixture::new("(loop $l (drop (call $now)) (br $l)) (i32.const 0)").with_imports(&["now"]);

/// Returns an 8 MB string.
pub const HUGE_OUTPUT: Fixture = Fixture::new(
    r#"
    (drop (memory.grow (i32.const 130)))
    (memory.fill (i32.const 140000) (i32.const 97) (i32.const 8000000))
    (call $ret_ok (i32.const 140000) (i32.const 8000000))
    "#,
);

/// Executes `unreachable`.
pub const TRAPS: Fixture = Fixture::new("unreachable");

// --- building --------------------------------------------------------------------------------

/// A core module, not a component.
pub fn core_module() -> Vec<u8> {
    wat::parse_str("(module (func (export \"f\") nop))").expect("the core module assembles")
}

/// The host the corpus bundles are opened for.
pub fn host() -> HostInfo {
    PackOptions::default().host
}

/// Builds a bundle from a manifest and a component.
pub fn bundle_bytes(manifest: &str, wasm: &[u8], assets: &[(&str, &[u8])]) -> Vec<u8> {
    let mut builder = BundleBuilder::new(manifest.to_owned(), wasm.to_vec());
    for (name, bytes) in assets {
        builder = builder.asset((*name).to_owned(), bytes.to_vec());
    }
    builder.build().expect("the corpus bundle builds")
}

/// Builds and opens a bundle; the tests that need the opened value use this.
pub fn open(manifest: &str, wasm: &[u8], assets: &[(&str, &[u8])]) -> Bundle {
    let bytes = bundle_bytes(manifest, wasm, assets);
    Bundle::open(&bytes, &Limits::new(), &host()).expect("the corpus bundle opens")
}
