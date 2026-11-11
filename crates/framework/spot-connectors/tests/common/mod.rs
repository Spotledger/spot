//! Test connector components: written as WebAssembly text, compiled with `wat`, no toolchain
//! target needed. Every fixture has the shape of `wit/connector.wit`: it exports `describe`,
//! `invoke` and `webhook`, imports the host functions, and lowers/lifts them with the canonical
//! ABI (UTF-8 strings, a bump allocator in a `libc` core module).
//!
//! To change a fixture, edit its `body`: the core function `invoke` receives the flattened
//! parameters (op, input, ctx) and returns a pointer to the `result<string, error>` it wrote at
//! address 1024. The `$ok` and `$err` helpers write one.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

/// The webhook response a fixture returns, at address 800.
const WEBHOOK_RESPONSE: &str = r#"{"status":200,"headers":[],"body":"ok"}"#;

/// One fixture: the `invoke` body, plus the payloads the body reads.
pub struct Fixture {
    /// The body of `invoke`: leaves an `i32` on the stack.
    pub body: &'static str,
    /// The body of `webhook`: leaves an `i32` on the stack.
    pub webhook: &'static str,
    /// The JSON request written at address 600 (read by the HTTP fixture).
    pub request: &'static str,
    /// The credential reference written at address 960 (read by the secret fixture).
    pub secret_ref: &'static str,
}

impl Fixture {
    pub const fn new(body: &'static str) -> Self {
        Self {
            body,
            webhook: "(call $err)",
            request: r#"{"method":"GET","url":"https://api.example.test/ping","headers":[],"body":null}"#,
            secret_ref: "api-token",
        }
    }

    pub const fn with_webhook(mut self, webhook: &'static str) -> Self {
        self.webhook = webhook;
        self
    }

    pub const fn with_request(mut self, request: &'static str) -> Self {
        self.request = request;
        self
    }

    pub const fn with_secret_ref(mut self, secret_ref: &'static str) -> Self {
        self.secret_ref = secret_ref;
        self
    }
}

const SKELETON: &str = r#"
(component
  (type $level_def (enum "debug" "info" "warn" "error"))
  (import "level" (type $level (eq $level_def)))

  (type $auth_kind_def (enum "none" "bearer" "basic" "oauth2"))
  (export $auth_kind "auth-kind" (type $auth_kind_def))
  (type $operation_def (record (field "name" string) (field "input-schema" string) (field "output-schema" string)))
  (export $operation "operation" (type $operation_def))
  (type $connector_info_def (record (field "name" string) (field "version" string) (field "operations" (list $operation)) (field "auth" (list $auth_kind))))
  (export $connector_info "connector-info" (type $connector_info_def))
  (type $call_context_def (record (field "tenant" string) (field "actor" (option string)) (field "now-micros" s64) (field "deadline-micros" s64)))
  (export $call_context "call-context" (type $call_context_def))
  (type $error_def (record (field "code" string) (field "message" string) (field "retryable" bool)))
  (export $error "error" (type $error_def))

  (import "log" (func $log (param "level" $level) (param "message" string)))
  (import "http-request" (func $http_request (param "request" string) (result (result string (error string)))))
  (import "secrets-get" (func $secrets_get (param "reference" string) (result (result string (error string)))))
  (import "kv-get" (func $kv_get (param "key" string) (result (option string))))
  (import "kv-set" (func $kv_set (param "key" string) (param "value" string) (result (result (error string)))))

  (core module $libc
    (memory (export "memory") 2)
    (global $heap (mut i32) (i32.const 8192))
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

  (core func $log_l (canon lower (func $log) (memory (core memory $libc "memory")) (realloc (core func $libc "realloc")) string-encoding=utf8))
  (core func $http_request_l (canon lower (func $http_request) (memory (core memory $libc "memory")) (realloc (core func $libc "realloc")) string-encoding=utf8))
  (core func $secrets_get_l (canon lower (func $secrets_get) (memory (core memory $libc "memory")) (realloc (core func $libc "realloc")) string-encoding=utf8))
  (core func $kv_get_l (canon lower (func $kv_get) (memory (core memory $libc "memory")) (realloc (core func $libc "realloc")) string-encoding=utf8))
  (core func $kv_set_l (canon lower (func $kv_set) (memory (core memory $libc "memory")) (realloc (core func $libc "realloc")) string-encoding=utf8))

  (core module $app
    (import "host" "log" (func $log (param i32 i32 i32)))
    (import "host" "http-request" (func $http_request (param i32 i32 i32)))
    (import "host" "secrets-get" (func $secrets_get (param i32 i32 i32)))
    (import "host" "kv-get" (func $kv_get (param i32 i32 i32)))
    (import "host" "kv-set" (func $kv_set (param i32 i32 i32 i32 i32)))
    (import "libc" "memory" (memory 1))
    (data (i32.const 512) "connector")
    (data (i32.const 522) "0.1.0")
    (data (i32.const 528) "app_failed")
    (data (i32.const 544) "it failed on purpose")
    (data (i32.const 600) "__REQUEST__")
    (data (i32.const 800) "__WEBHOOK_DATA__")
    (data (i32.const 960) "__SECRET_REF__")
    (func $ok (param $ptr i32) (param $len i32) (result i32)
      (i32.store8 (i32.const 1024) (i32.const 0))
      (i32.store (i32.const 1028) (local.get $ptr))
      (i32.store (i32.const 1032) (local.get $len))
      (i32.const 1024))
    (func $err (result i32)
      (i32.store8 (i32.const 1024) (i32.const 1))
      (i32.store (i32.const 1028) (i32.const 528))
      (i32.store (i32.const 1032) (i32.const 10))
      (i32.store (i32.const 1036) (i32.const 544))
      (i32.store (i32.const 1040) (i32.const 20))
      (i32.store8 (i32.const 1044) (i32.const 1))
      (i32.const 1024))
    (func (export "describe") (result i32)
      (i32.store (i32.const 1024) (i32.const 512))
      (i32.store (i32.const 1028) (i32.const 9))
      (i32.store (i32.const 1032) (i32.const 522))
      (i32.store (i32.const 1036) (i32.const 5))
      (i32.store (i32.const 1040) (i32.const 0))
      (i32.store (i32.const 1044) (i32.const 0))
      (i32.store (i32.const 1048) (i32.const 700))
      (i32.store (i32.const 1052) (i32.const 1))
      (i32.store8 (i32.const 700) (i32.const 0))
      (i32.const 1024))
    (func (export "invoke")
      (param $op_ptr i32) (param $op_len i32)
      (param $in_ptr i32) (param $in_len i32)
      (param $t_ptr i32) (param $t_len i32)
      (param $a_disc i32) (param $a_ptr i32) (param $a_len i32)
      (param $now i64) (param $dl i64) (result i32)
      __BODY__)
    (func (export "webhook") (param $req_ptr i32) (param $req_len i32) (result i32)
      __WEBHOOK__))

  (core instance $app (instantiate $app
    (with "host" (instance
      (export "log" (func $log_l))
      (export "http-request" (func $http_request_l))
      (export "secrets-get" (func $secrets_get_l))
      (export "kv-get" (func $kv_get_l))
      (export "kv-set" (func $kv_set_l))))
    (with "libc" (instance $libc))))

  (func $describe (result $connector_info)
    (canon lift (core func $app "describe") (memory (core memory $libc "memory")) (realloc (core func $libc "realloc")) string-encoding=utf8))
  (export "describe" (func $describe))
  (func $invoke (param "op" string) (param "input" string) (param "ctx" $call_context) (result (result string (error $error)))
    (canon lift (core func $app "invoke") (memory (core memory $libc "memory")) (realloc (core func $libc "realloc")) string-encoding=utf8))
  (export "invoke" (func $invoke))
  (func $webhook (param "request" string) (result (result string (error $error)))
    (canon lift (core func $app "webhook") (memory (core memory $libc "memory")) (realloc (core func $libc "realloc")) string-encoding=utf8))
  (export "webhook" (func $webhook)))
"#;

/// Escapes a string for a WAT data segment.
fn wat_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            other => out.push(other),
        }
    }
    out
}

/// The component for a fixture, as binary.
pub fn component(fixture: &Fixture) -> Vec<u8> {
    let text = SKELETON
        .replace("__REQUEST__", &wat_string(fixture.request))
        .replace("__WEBHOOK_DATA__", &wat_string(WEBHOOK_RESPONSE))
        .replace("__SECRET_REF__", &wat_string(fixture.secret_ref))
        .replace(
            "__BODY__",
            &fixture
                .body
                .replace("__REQ_LEN__", &fixture.request.len().to_string())
                .replace("__SECRET_LEN__", &fixture.secret_ref.len().to_string()),
        )
        .replace(
            "__WEBHOOK__",
            &fixture
                .webhook
                .replace("__WEBHOOK_LEN__", &WEBHOOK_RESPONSE.len().to_string()),
        );
    wat::parse_str(&text).unwrap_or_else(|e| panic!("the fixture does not assemble: {e}\n{text}"))
}

// --- the fixtures -----------------------------------------------------------------------------

/// Returns its input unchanged.
pub const ECHO: Fixture = Fixture::new("(call $ok (local.get $in_ptr) (local.get $in_len))");

/// Always returns the connector's own error.
pub const FAILS: Fixture = Fixture::new("(call $err)");

/// Loops for ever.
pub const SPIN: Fixture = Fixture::new("(loop $l (br $l)) (i32.const 0)");

/// Logs the request, makes it through `http-request`, and returns the response body.
pub const HTTP: Fixture = Fixture::new(
    r#"
    (call $log (i32.const 1) (i32.const 600) (i32.const __REQ_LEN__))
    (call $http_request (i32.const 600) (i32.const __REQ_LEN__) (i32.const 2048))
    (if (result i32) (i32.eqz (i32.load8_u (i32.const 2048)))
      (then (call $ok (i32.load (i32.const 2052)) (i32.load (i32.const 2056))))
      (else (call $err)))
    "#,
);

/// Reads a credential reference with `secrets-get` and returns it.
pub const SECRET: Fixture = Fixture::new(
    r#"
    (call $secrets_get (i32.const 960) (i32.const __SECRET_LEN__) (i32.const 2048))
    (if (result i32) (i32.eqz (i32.load8_u (i32.const 2048)))
      (then (call $ok (i32.load (i32.const 2052)) (i32.load (i32.const 2056))))
      (else (call $err)))
    "#,
);

/// Returns a fixed webhook response.
pub const WEBHOOK: Fixture = Fixture::new("(call $ok (local.get $in_ptr) (local.get $in_len))")
    .with_webhook("(call $ok (i32.const 800) (i32.const __WEBHOOK_LEN__))");
