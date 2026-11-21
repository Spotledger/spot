//! The script and component runtimes of the dev server (WP-DX-02.T3): no database needed.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value as Json, json};
use spot_cli::services::SiteCtx;
use spot_cli::services::devserver::{
    ComponentRuntime, DevComponents, DevScripts, ScriptRuntime, provide_runtimes, register,
};
use spot_core::ManualClock;
use spot_ts::{EngineConfig, HostBridge, HostError, Limits, ScriptEngine};

struct NoHost;

impl HostBridge for NoHost {
    fn call(&self, name: &str, _args: Json) -> Result<Json, HostError> {
        Err(HostError::new(
            "unsupported",
            &format!("`{name}` is not available"),
        ))
    }

    fn now_millis(&self) -> i64 {
        0
    }

    fn random_seed(&self) -> u64 {
        0
    }
}

fn ctx_with(settings: BTreeMap<String, toml::Value>) -> SiteCtx<'static> {
    let clock = Arc::new(ManualClock::new(time::OffsetDateTime::UNIX_EPOCH));
    SiteCtx::new("acme", Duration::from_secs(1), clock, settings, true)
}

fn scripts() -> DevScripts {
    DevScripts::new(ScriptEngine::new(EngineConfig::default()))
}

async fn answer(scripts: &DevScripts) -> Json {
    scripts
        .call(
            "greet",
            "answer",
            &Json::Null,
            Arc::new(NoHost),
            Limits::default(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn an_edited_script_runs_its_new_text_on_the_next_call() {
    let scripts = scripts();
    scripts
        .replace("greet", "export function answer() { return 1; }", "")
        .unwrap();
    assert_eq!(answer(&scripts).await, json!(1));
    scripts
        .replace("greet", "export function answer() { return 2; }", "")
        .unwrap();
    assert_eq!(answer(&scripts).await, json!(2));
    assert_eq!(scripts.version("greet"), Some(2));
}

#[tokio::test]
async fn a_script_that_does_not_compile_keeps_the_old_version() {
    let scripts = scripts();
    scripts
        .replace("greet", "export function answer() { return 1; }", "")
        .unwrap();
    let refusal = scripts
        .replace("greet", "export function answer( {{{", "")
        .unwrap_err();
    assert!(
        refusal.contains("previous version keeps serving"),
        "{refusal}"
    );
    assert!(!refusal.contains("{{{"), "no source text in the sentence");
    assert_eq!(answer(&scripts).await, json!(1));
}

#[tokio::test]
async fn a_script_that_was_never_loaded_is_refused_by_name() {
    let scripts = scripts();
    let error = scripts
        .call(
            "ghost",
            "answer",
            &Json::Null,
            Arc::new(NoHost),
            Limits::default(),
        )
        .await
        .unwrap_err();
    assert!(error.contains("ghost"));
}

#[test]
fn script_names_must_be_plain() {
    let scripts = scripts();
    for name in ["", "../x", "/x", "a//b", ".hidden", "a b"] {
        assert!(
            scripts.replace(name, "export function f() {}", "").is_err(),
            "{name:?}"
        );
    }
    assert!(
        scripts
            .replace("dir/ok-1.2", "export function f() {}", "")
            .is_ok()
    );
}

const CORE: &[u8] = &[0, 0x61, 0x73, 0x6d, 1, 0, 0, 0];

fn wasm(tail: &[u8]) -> Vec<u8> {
    let mut bytes = CORE.to_vec();
    bytes.extend_from_slice(tail);
    bytes
}

#[test]
fn an_edited_component_changes_the_bundle_hash() {
    let components = DevComponents::new();
    let first = components.replace("shop", &wasm(&[1, 2, 3])).unwrap();
    let second = components.replace("shop", &wasm(&[1, 2, 4])).unwrap();
    assert_ne!(first, second);
    assert_eq!(first.len(), 64);
    assert_eq!(components.bundle_hash("shop"), Some(second.clone()));
    // The same bytes give the same hash.
    assert_eq!(
        components.replace("shop", &wasm(&[1, 2, 4])).unwrap(),
        second
    );
    assert_eq!(components.bundle("shop").unwrap().len(), 11);
}

#[test]
fn a_component_that_does_not_validate_keeps_the_old_bundle() {
    let components = DevComponents::new();
    let good = components.replace("shop", &wasm(&[9])).unwrap();
    assert!(components.replace("shop", b"not wasm at all").is_err());
    assert!(components.replace("shop", &[]).is_err());
    assert!(components.replace("../shop", &wasm(&[9])).is_err());
    assert_eq!(components.bundle_hash("shop"), Some(good));
}

#[test]
fn a_component_header_is_accepted() {
    let components = DevComponents::new();
    let header = [0, 0x61, 0x73, 0x6d, 0x0d, 0, 1, 0, 7];
    assert!(components.replace("shop", &header).is_ok());
}

#[test]
fn a_dev_host_provides_both_ports() {
    let mut ctx = ctx_with(BTreeMap::new());
    let runtimes = provide_runtimes(&mut ctx, ScriptEngine::new(EngineConfig::default())).unwrap();
    assert!(ctx.port::<dyn ScriptRuntime>().is_some());
    assert!(ctx.port::<dyn ComponentRuntime>().is_some());
    assert!(ctx.port::<DevScripts>().is_some());
    runtimes.components.replace("shop", &wasm(&[1])).unwrap();
    assert!(
        ctx.port::<DevComponents>()
            .unwrap()
            .bundle_hash("shop")
            .is_some()
    );
}

#[test]
fn providing_twice_is_an_error() {
    let mut ctx = ctx_with(BTreeMap::new());
    provide_runtimes(&mut ctx, ScriptEngine::new(EngineConfig::default())).unwrap();
    assert!(provide_runtimes(&mut ctx, ScriptEngine::new(EngineConfig::default())).is_err());
}

#[test]
fn a_non_dev_host_provides_neither_port() {
    // Not enabled: nothing registered, nothing provided.
    let mut ctx = ctx_with(BTreeMap::new());
    register(&mut ctx).unwrap();
    assert!(ctx.port::<dyn ScriptRuntime>().is_none());
    assert!(ctx.port::<dyn ComponentRuntime>().is_none());
}

#[test]
fn an_enabled_dev_server_without_a_site_is_refused_and_provides_nothing() {
    let settings = BTreeMap::from([(
        "devserver".to_owned(),
        toml::Value::Table(toml::from_str("enabled = true").unwrap()),
    )]);
    let mut ctx = ctx_with(settings);
    assert!(register(&mut ctx).is_err());
    assert!(ctx.port::<dyn ScriptRuntime>().is_none());
    assert!(ctx.port::<dyn ComponentRuntime>().is_none());
}
