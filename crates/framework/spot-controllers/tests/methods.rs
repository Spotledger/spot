//! Method declarations: conflict detection and explicit overrides.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::{APP_A, APP_B, APP_C, Probe, method};
use spot_controllers::{ControllerRegistry, ControllerSource};

fn note(method_name: &str, mutates: bool) -> Arc<Probe> {
    Arc::new(Probe::new("note").with_method(method(method_name, mutates)))
}

#[test]
fn two_apps_declaring_one_method_conflict() {
    let mut registry = ControllerRegistry::new();
    registry.register(APP_A, note("ping", false), ControllerSource::Native);
    registry.register(APP_B, note("ping", false), ControllerSource::Native);

    let error = registry.methods("note").expect_err("must conflict");
    assert_eq!(error.code, "controller.method_conflict");
    assert!(error.message.contains("ping"));
    assert!(error.message.contains(APP_A) && error.message.contains(APP_B));
}

#[test]
fn an_override_resolves_the_conflict() {
    let mut registry = ControllerRegistry::new();
    registry.register(APP_A, note("ping", false), ControllerSource::Native);
    registry.register(APP_B, note("ping", true), ControllerSource::Native);
    registry.declare_method_override(APP_B, APP_A, "note", "ping");

    let methods = registry.methods("note").expect("the override resolves it");
    let ping = methods.get("ping").expect("declared");
    assert_eq!(ping.app, APP_B, "the overriding app wins");
    assert!(ping.mutates);
}

#[test]
fn an_override_that_does_not_cover_every_app_still_conflicts() {
    let mut registry = ControllerRegistry::new();
    registry.register(APP_A, note("ping", false), ControllerSource::Native);
    registry.register(APP_B, note("ping", false), ControllerSource::Native);
    registry.register(APP_C, note("ping", false), ControllerSource::Native);
    // B overrides A, but nobody covers C.
    registry.declare_method_override(APP_B, APP_A, "note", "ping");

    assert!(registry.methods("note").is_err());
}

#[test]
fn methods_on_another_doctype_do_not_conflict() {
    let mut registry = ControllerRegistry::new();
    registry.register(
        APP_A,
        Arc::new(Probe::new("note").with_method(method("ping", false))),
        ControllerSource::Native,
    );
    registry.register(
        APP_B,
        Arc::new(Probe::new("other").with_method(method("ping", false))),
        ControllerSource::Native,
    );
    assert!(registry.methods("note").is_ok());
    assert!(registry.methods("other").is_ok());
}
