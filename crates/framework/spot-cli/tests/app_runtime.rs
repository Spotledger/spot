//! The app runtime service (WP-EXT-05.T2), driven through its public API with small WebAssembly
//! components written as text: limits and kills, the circuit breaker, capability denial, document
//! reads as the invoking user, start-up verification refusals, unload and the egress gate.
//!
//! These tests need no database: the installed-app registry and the bundle store are the
//! in-memory ones. A signed-bundle round trip with the sample app is the compile bundle's
//! integration run (`apps/sample` packed and installed).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use spot_cli::services::apps::egress::AppEgress;
use spot_cli::services::apps::limits::{AppLimits, LimitTable};
use spot_cli::services::apps::runtime::{
    AppAudit, AppAuditRecord, AppRuntime, AppStatus, BreakerConfig, CallIdentity, DISABLED,
    InvokerDocs, LIMIT_EXCEEDED, NOT_LOADED, REJECTED, RuntimeSettings,
};
use spot_cli::services::apps::store::{MemoryBundleStore, StoredBundle};
use spot_cli::services::apps::verify::Trust;
use spot_core::{ManualClock, SiteId};
use spot_egress::TestResolver;
use spot_installer::{GrantedCapabilities, InstalledApp, SiteState};
use time::macros::datetime;

// ---- components -------------------------------------------------------------------------------

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
    (data (i32.const 512) "customer")
    (data (i32.const 528) "not_found")
    (data (i32.const 544) "no such document")
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

const GET_DOC_IMPORT: &str = r#"(import "get-doc" (func $get-doc (param "doctype" string) (param "id" string) (result (option string))))"#;
const GET_DOC_LOWER: &str = r#"(core func $get_doc_l (canon lower (func $get-doc) (memory (core memory $libc "memory")) (realloc (core func $libc "realloc")) string-encoding=utf8))"#;
const GET_DOC_CORE: &str =
    r#"(import "host" "get-doc" (func $get_doc (param i32 i32 i32 i32 i32)))"#;

fn component(body: &str, uses_get_doc: bool) -> Vec<u8> {
    let (imports, lowered, core, exports) = if uses_get_doc {
        (
            GET_DOC_IMPORT,
            GET_DOC_LOWER,
            GET_DOC_CORE,
            r#"(export "get-doc" (func $get_doc_l))"#,
        )
    } else {
        ("", "", "", "")
    };
    let text = SKELETON
        .replace("__COMPONENT_IMPORTS__", imports)
        .replace("__LOWERED__", lowered)
        .replace("__CORE_IMPORTS__", core)
        .replace("__HOST_EXPORTS__", exports)
        .replace("__BODY__", body);
    wat::parse_str(&text).unwrap_or_else(|e| panic!("the fixture does not assemble: {e}"))
}

const ECHO: &str = "(call $ret_ok (local.get $ptr) (local.get $len))";
const SPIN: &str = "(loop $l (br $l)) (i32.const 0)";
const HOG: &str = "(loop $l (drop (memory.grow (i32.const 8))) (br $l)) (i32.const 0)";
const READS_CUSTOMER: &str = r#"
    (call $get_doc (i32.const 512) (i32.const 8) (local.get $ptr) (local.get $len) (i32.const 2048))
    (if (result i32) (i32.load8_u (i32.const 2048))
      (then (call $ret_ok (i32.load (i32.const 2052)) (i32.load (i32.const 2056))))
      (else (call $ret_err (i32.const 528) (i32.const 9) (i32.const 544) (i32.const 16))))
"#;

// ---- helpers ----------------------------------------------------------------------------------

#[derive(Default)]
struct Audit(Mutex<Vec<AppAuditRecord>>);

impl AppAudit for Audit {
    fn record(&self, record: AppAuditRecord) {
        self.0.lock().unwrap().push(record);
    }
}

struct Docs;

impl InvokerDocs for Docs {
    fn get(&self, _site: &SiteId, user: &str, doctype: &str, id: &str) -> Option<String> {
        (user == "u1" && doctype == "customer").then(|| format!("{{\"id\":\"{id}\"}}"))
    }
}

fn limits_with(app: &str, own: AppLimits) -> LimitTable {
    let mut table = LimitTable::default();
    table.apps.insert(app.to_owned(), own);
    table
}

fn runtime(site: &str, limits: LimitTable, kills: u32, audit: Arc<Audit>) -> AppRuntime {
    let clock = Arc::new(ManualClock::new(datetime!(2026-11-12 10:00 UTC)));
    AppRuntime::new(
        SiteId::new(site.replace('_', "-")).unwrap(),
        RuntimeSettings {
            limits,
            breaker: BreakerConfig {
                kills,
                window: std::time::Duration::from_secs(300),
            },
            cache_entries: 4,
            max_concurrent_calls: 8,
            egress_allow: Vec::new(),
        },
        clock,
        audit,
        Arc::new(Docs),
        Arc::new(TestResolver::new()),
    )
    .unwrap()
}

fn install(rt: &AppRuntime, name: &str, wasm: &[u8], caps: &GrantedCapabilities) {
    // A stand-in digest: distinct per component, which is all the compile cache keys on.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in wasm {
        hash = (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3);
    }
    let digest = format!("{hash:064x}");
    let app = rt
        .link(name, "1.0.0", &digest, wasm, caps, None, None)
        .unwrap();
    rt.insert(app);
}

fn who(user: Option<&str>) -> CallIdentity {
    CallIdentity {
        user: user.map(str::to_owned),
        now_micros: 0,
        external_allowed: true,
    }
}

async fn call(
    rt: &AppRuntime,
    app: &str,
    text: &str,
    user: Option<&str>,
) -> Result<Value, spot_cli::services::apps::CallError> {
    rt.call(app, "on-event", json!([text]), who(user)).await
}

// ---- tests ------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn an_app_serves_a_call() {
    let rt = runtime("s1", LimitTable::default(), 5, Arc::default());
    install(
        &rt,
        "echo",
        &component(ECHO, false),
        &GrantedCapabilities::default(),
    );
    assert_eq!(call(&rt, "echo", "hi", None).await.unwrap(), json!("hi"));
}

#[tokio::test(flavor = "multi_thread")]
async fn fuel_wall_clock_and_memory_kill_only_the_call_and_the_server_keeps_serving() {
    let audit = Arc::new(Audit::default());
    let mut table = LimitTable::default();
    table.apps.insert(
        "spin".into(),
        AppLimits {
            fuel: Some(50_000),
            ..AppLimits::default()
        },
    );
    table.apps.insert(
        "sleepy".into(),
        AppLimits {
            wall_clock_ms: Some(50),
            ..AppLimits::default()
        },
    );
    table.apps.insert(
        "hog".into(),
        AppLimits {
            memory_mib: Some(1),
            ..AppLimits::default()
        },
    );
    let rt = runtime("s1", table, 0, Arc::clone(&audit));
    let none = GrantedCapabilities::default();
    install(&rt, "echo", &component(ECHO, false), &none);
    install(&rt, "spin", &component(SPIN, false), &none);
    install(&rt, "sleepy", &component(SPIN, false), &none);
    install(&rt, "hog", &component(HOG, false), &none);
    for (app, limit) in [
        ("spin", "fuel"),
        ("sleepy", "wall_clock"),
        ("hog", "memory"),
    ] {
        let error = call(&rt, app, "x", None).await.unwrap_err();
        assert_eq!(error.code, LIMIT_EXCEEDED, "{app}");
        assert_eq!(error.limit, Some(limit), "{app}");
        // An unrelated app answers right after.
        assert_eq!(call(&rt, "echo", "ok", None).await.unwrap(), json!("ok"));
    }
    let records = audit.0.lock().unwrap();
    let kills: Vec<_> = records.iter().filter(|r| r.event == "killed").collect();
    assert_eq!(kills.len(), 3);
    // The audit record names the app and the limit, never the payload.
    assert!(
        kills
            .iter()
            .all(|r| !r.app.is_empty() && r.detail.is_some())
    );
    assert!(!format!("{records:?}").contains("\"x\""));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_breaker_disables_an_app_killed_too_often() {
    let table = limits_with(
        "spin",
        AppLimits {
            fuel: Some(10_000),
            ..AppLimits::default()
        },
    );
    let rt = runtime("s1", table, 2, Arc::default());
    install(
        &rt,
        "spin",
        &component(SPIN, false),
        &GrantedCapabilities::default(),
    );
    assert_eq!(
        call(&rt, "spin", "x", None).await.unwrap_err().code,
        LIMIT_EXCEEDED
    );
    assert_eq!(
        call(&rt, "spin", "x", None).await.unwrap_err().code,
        LIMIT_EXCEEDED
    );
    assert_eq!(
        call(&rt, "spin", "x", None).await.unwrap_err().code,
        DISABLED
    );
    let row = rt.list().into_iter().find(|r| r.name == "spin").unwrap();
    assert_eq!(row.status, AppStatus::Disabled);
    assert!(!rt.degradation().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn an_import_that_was_not_approved_fails_at_link_time() {
    let rt = runtime("s1", LimitTable::default(), 5, Arc::default());
    let wasm = component(READS_CUSTOMER, true);
    // No DocType read approved: `get-doc` is not linked.
    assert_eq!(
        rt.link(
            "reader",
            "1.0.0",
            &"1".repeat(64),
            &wasm,
            &GrantedCapabilities::default(),
            None,
            None
        )
        .err(),
        Some("app.capability_denied")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn documents_are_read_as_the_invoking_user() {
    let rt = runtime("s1", LimitTable::default(), 5, Arc::default());
    let mut caps = GrantedCapabilities::default();
    caps.doctypes.read.push("customer".to_owned());
    install(&rt, "reader", &component(READS_CUSTOMER, true), &caps);
    // The user the document layer lets read it.
    assert!(call(&rt, "reader", "c1", Some("u1")).await.is_ok());
    // Another user, and a system actor with no user, see nothing: the app's own error comes back
    // as a sanitised rejection, never as the document.
    for user in [Some("u2"), None] {
        let error = call(&rt, "reader", "c1", user).await.unwrap_err();
        assert_eq!(error.code, REJECTED);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tampered_or_missing_bundle_is_not_loaded_and_the_site_is_otherwise_healthy() {
    let rt = runtime("s1", LimitTable::default(), 5, Arc::default());
    let store = MemoryBundleStore::new();
    let digest = "a".repeat(64);
    // One flipped byte: whatever the bytes are, they are not a bundle with this digest.
    store.put(
        &digest,
        StoredBundle {
            bytes: b"PK\x03\x04 tampered".to_vec(),
            ..StoredBundle::default()
        },
    );
    let mut state = SiteState::new();
    state.apps.push(InstalledApp::new(
        "tampered",
        "1.0.0".parse().unwrap(),
        digest.parse().unwrap(),
    ));
    state.apps.push(InstalledApp::new(
        "absent",
        "1.0.0".parse().unwrap(),
        "b".repeat(64).parse().unwrap(),
    ));
    rt.load_all(&state, &store, &Trust::none(false));
    let rows = rt.list();
    assert_eq!(rows.len(), 2);
    for row in rows {
        assert_eq!(
            row.status,
            AppStatus::Failed("app.bundle_invalid".to_owned())
        );
    }
    // The runtime itself serves: another app can still be installed and called.
    install(
        &rt,
        "echo",
        &component(ECHO, false),
        &GrantedCapabilities::default(),
    );
    assert_eq!(call(&rt, "echo", "ok", None).await.unwrap(), json!("ok"));
}

#[tokio::test(flavor = "multi_thread")]
async fn uninstall_unloads_at_once() {
    let rt = runtime("s1", LimitTable::default(), 5, Arc::default());
    install(
        &rt,
        "echo",
        &component(ECHO, false),
        &GrantedCapabilities::default(),
    );
    rt.unload("echo");
    assert_eq!(
        call(&rt, "echo", "x", None).await.unwrap_err().code,
        NOT_LOADED
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn two_sites_share_no_instances_or_state() {
    let a = runtime("site_a", LimitTable::default(), 5, Arc::default());
    let b = runtime("site_b", LimitTable::default(), 5, Arc::default());
    install(
        &a,
        "echo",
        &component(ECHO, false),
        &GrantedCapabilities::default(),
    );
    assert!(call(&a, "echo", "x", None).await.is_ok());
    assert_eq!(
        call(&b, "echo", "x", None).await.unwrap_err().code,
        NOT_LOADED
    );
}

// ---- egress -----------------------------------------------------------------------------------

fn gate(resolver: Arc<TestResolver>, approved: &[(&str, u16)], site: &[(&str, u16)]) -> AppEgress {
    let own = |v: &[(&str, u16)]| {
        v.iter()
            .map(|(h, p)| ((*h).to_owned(), *p))
            .collect::<Vec<_>>()
    };
    AppEgress::new(
        "s1",
        "app",
        &own(approved),
        &own(site),
        resolver,
        Arc::new(ManualClock::new(datetime!(2026-11-12 10:00 UTC))),
    )
}

fn ip(a: u8, b: u8, c: u8, d: u8) -> Vec<IpAddr> {
    vec![IpAddr::V4(Ipv4Addr::new(a, b, c, d))]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn egress_allows_only_approved_public_hosts() {
    let resolver = Arc::new(TestResolver::new());
    resolver.add_response("api.example.com", vec![ip(93, 184, 216, 34)]);
    let gate = gate(
        resolver,
        &[("api.example.com", 443)],
        &[("api.example.com", 443)],
    );
    assert!(
        gate.authorize_url("https://api.example.com/x", true)
            .await
            .is_ok()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn egress_refuses_everything_else() {
    let resolver = Arc::new(TestResolver::new());
    resolver.add_response("localhost", vec![ip(127, 0, 0, 1)]);
    resolver.add_response("meta.example.com", vec![ip(169, 254, 169, 254)]);
    resolver.add_response("rebind.example.com", vec![ip(10, 0, 0, 5)]);
    resolver.add_response("api.example.com", vec![ip(93, 184, 216, 34)]);
    let approved = [
        ("localhost", 80),
        ("meta.example.com", 80),
        ("rebind.example.com", 443),
        ("api.example.com", 443),
    ];
    let gate = gate(resolver, &approved, &approved);
    // Not on the allow-list at all.
    assert!(
        gate.authorize_url("https://other.example.com/", true)
            .await
            .is_err()
    );
    // Literal loopback and metadata addresses, and names that resolve to them or to a private
    // address, are refused after resolution even when listed.
    for url in [
        "http://127.0.0.1/",
        "http://169.254.169.254/latest/meta-data",
        "http://localhost/",
        "http://meta.example.com/",
        "https://rebind.example.com/",
    ] {
        assert!(gate.authorize_url(url, true).await.is_err(), "{url}");
    }
    // A redirect to the metadata address is a new request, refused the same way.
    assert!(
        gate.authorize_url("http://169.254.169.254/", true)
            .await
            .is_err()
    );
    // An in-transaction hook makes no external call whatever the policy says (ADR-51).
    assert!(
        gate.authorize_url("https://api.example.com/", false)
            .await
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn egress_with_an_empty_site_allow_list_means_no_network() {
    let resolver = Arc::new(TestResolver::new());
    resolver.add_response("api.example.com", vec![ip(93, 184, 216, 34)]);
    let gate = gate(resolver, &[("api.example.com", 443)], &[]);
    assert!(
        gate.authorize_url("https://api.example.com/", true)
            .await
            .is_err()
    );
}
