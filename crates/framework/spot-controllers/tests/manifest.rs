//! Loading controllers from an app manifest and a [`NativeControllers`] table.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::{Probe, manifest, step};
use spot_app_api::{HookPoint, Operation, Phase, Target};
use spot_controllers::{
    ControllerFactory, ControllerRef, ControllerRegistry, ControllerSource, NativeControllers,
};

const SHOP: &str = r#"
[app]
name = "shop"
version = "0.1.0"

[[events]]
doctype = "note"
event = "before_create"
step = "shop:fill"
priority = 50
budget_ms = 200
"#;

fn before_create() -> HookPoint {
    HookPoint::of(Phase::Before, Target::Op(Operation::Create))
}

#[test]
fn a_native_manifest_loads_its_controller_by_app_name() {
    let mut table = NativeControllers::new();
    table.insert(
        "shop",
        Arc::new(Probe::new("note").with_step(step("shop:fill", before_create(), 0))),
    );

    let mut registry = ControllerRegistry::new();
    registry
        .register_from_manifest(&manifest(SHOP), &table)
        .expect("the controller is built in");

    let registered = registry.controllers_for("note");
    assert_eq!(registered.len(), 1);
    assert_eq!(registered[0].app, "shop");
    assert_eq!(registered[0].source, ControllerSource::Native);
}

#[test]
fn the_manifest_priority_wins_over_the_controllers_own() {
    let mut table = NativeControllers::new();
    table.insert(
        "shop",
        Arc::new(Probe::new("note").with_step(step("shop:fill", before_create(), 0))),
    );

    let mut registry = ControllerRegistry::new();
    registry
        .register_from_manifest(&manifest(SHOP), &table)
        .expect("the controller is built in");

    let seams = registry.install().expect("no conflicts");
    let plan = seams.plan("note", &before_create());
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0].step.as_str(), "shop:fill");
    assert_eq!(plan[0].priority, 50, "priority comes from the manifest");
}

#[test]
fn a_typescript_reference_names_the_missing_adapter() {
    let mut registry = ControllerRegistry::new();
    let reference = ControllerRef::parse("ts:controllers/invoice.ts").expect("a valid reference");
    let error = registry
        .register_from_manifest_ref(&manifest(SHOP), &reference, &NativeControllers::new())
        .err()
        .expect("no TypeScript adapter yet");

    assert_eq!(error.code, "controller.host_unavailable");
    assert!(error.message.contains("TypeScript"), "{}", error.message);
}

#[test]
fn a_wasm_reference_names_the_missing_adapter() {
    let error = NativeControllers::new()
        .create(&ControllerRef::parse("wasm:invoice").expect("valid"))
        .err()
        .expect("no WASM adapter yet");
    assert_eq!(error.code, "controller.host_unavailable");
    assert!(error.message.contains("WASM"), "{}", error.message);
}

#[test]
fn an_unknown_native_controller_is_reported() {
    let error = NativeControllers::new()
        .create(&ControllerRef::native("nobody"))
        .err()
        .expect("not built in");
    assert_eq!(error.code, "controller.unknown_native");
    assert!(error.message.contains("nobody"));
}

#[test]
fn a_bad_reference_is_reported() {
    assert_eq!(
        ControllerRef::parse("porter:invoice")
            .expect_err("bad prefix")
            .code,
        "controller.invalid_reference"
    );
    assert_eq!(
        ControllerRef::parse("native:")
            .expect_err("empty value")
            .code,
        "controller.invalid_reference"
    );
    assert_eq!(
        ControllerRef::parse("nativesomething")
            .expect_err("no prefix")
            .code,
        "controller.invalid_reference"
    );
}

#[test]
fn from_controllers_keys_the_table_by_doctype() {
    let table = NativeControllers::from_controllers(vec![
        Arc::new(Probe::new("note")) as Arc<dyn spot_app_api::Controller>,
        Arc::new(Probe::new("other")) as Arc<dyn spot_app_api::Controller>,
    ]);
    assert_eq!(table.len(), 2);
    assert!(table.get("note").is_some());
    assert!(table.get("other").is_some());
    assert!(table.get("missing").is_none());
}

const TWO_DOCTYPES: &str = r#"
[app]
name = "shop"
version = "0.1.0"

[[events]]
doctype = "note"
event = "before_create"
step = "shop:fill"
priority = 50

[[events]]
doctype = "memo"
event = "before_create"
step = "shop:stamp"
priority = 70
"#;

fn two_controller_table() -> NativeControllers {
    NativeControllers::new()
        .with(
            "shop",
            Arc::new(Probe::new("note").with_step(step("shop:fill", before_create(), 0))),
        )
        .with(
            "shop.memo",
            Arc::new(Probe::new("memo").with_step(step("shop:stamp", before_create(), 0))),
        )
}

#[test]
fn a_manifest_with_events_for_several_doctypes_installs_each_controller() {
    let mut registry = ControllerRegistry::new();
    registry
        .register_from_manifest(&manifest(TWO_DOCTYPES), &two_controller_table())
        .expect("both controllers install");

    assert_eq!(registry.controllers_for("note").len(), 1);
    assert_eq!(registry.controllers_for("memo").len(), 1);
    let seams = registry.install().expect("no conflicts");
    let note = seams.plan("note", &before_create());
    let memo = seams.plan("memo", &before_create());
    assert_eq!(note.len(), 1, "note runs only its own step");
    assert_eq!(note[0].step.as_str(), "shop:fill");
    assert_eq!(note[0].priority, 50);
    assert_eq!(memo.len(), 1, "memo runs only its own step");
    assert_eq!(memo[0].step.as_str(), "shop:stamp");
    assert_eq!(memo[0].priority, 70);
}

#[test]
fn an_event_for_an_unserved_doctype_is_refused_at_install() {
    let mut registry = ControllerRegistry::new();
    // Only the `note` controller is built in; the `memo` event has nowhere to attach.
    let table = NativeControllers::new().with(
        "shop",
        Arc::new(Probe::new("note").with_step(step("shop:fill", before_create(), 0))),
    );
    let error = registry
        .register_from_manifest(&manifest(TWO_DOCTYPES), &table)
        .err()
        .expect("the memo event is unserved");

    assert_eq!(error.code, "controller.event_doctype_unserved");
    assert!(error.message.contains("memo"), "{}", error.message);
    assert!(
        registry.controllers_for("note").is_empty(),
        "a refused install registers nothing"
    );
}

#[test]
fn two_controllers_for_one_doctype_in_one_app_are_refused() {
    let table = NativeControllers::new()
        .with("shop", Arc::new(Probe::new("note")))
        .with("shop.again", Arc::new(Probe::new("note")));
    let error = ControllerRegistry::new()
        .register_from_manifest(&manifest(SHOP), &table)
        .err()
        .expect("one controller per DocType");
    assert_eq!(error.code, "controller.doctype_duplicate");
}
