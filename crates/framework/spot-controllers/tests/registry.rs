//! The registry: ordering, the default controller, the method index, uninstall and snapshots.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{APP_A, APP_B, APP_C, Probe, method, step};
use spot_app_api::{
    AppResult, BoxFuture, Controller, EffectClassName, EffectDecl, EffectKind, HookContext,
    HookPoint, Operation, Phase, Reversal, StepDecl, StepEffects, StepName, StoreName, Target,
};
use spot_controllers::{ControllerRegistry, ControllerSource, DEFAULT_APP, Right};

fn save_point() -> HookPoint {
    HookPoint::of(Phase::Before, Target::Op(Operation::Create))
}

#[test]
fn controllers_are_ordered_by_priority_then_registration() {
    let mut registry = ControllerRegistry::new();
    // Registered out of priority order on purpose.
    registry.register(
        APP_B,
        Arc::new(Probe::new("note")),
        ControllerSource::Native,
    );
    registry.register_prioritized(
        APP_A,
        Arc::new(Probe::new("note")),
        ControllerSource::Wasm,
        10,
    );
    registry.register_prioritized(
        APP_C,
        Arc::new(Probe::new("note")),
        ControllerSource::TypeScript,
        -5,
    );

    let apps: Vec<String> = registry
        .controllers_for("note")
        .into_iter()
        .map(|r| r.app)
        .collect();
    assert_eq!(apps, vec![APP_C, APP_B, APP_A]);
}

#[test]
fn same_priority_keeps_registration_order() {
    let mut registry = ControllerRegistry::new();
    for app in [APP_C, APP_A, APP_B] {
        registry.register(app, Arc::new(Probe::new("note")), ControllerSource::Native);
    }
    let apps: Vec<String> = registry
        .controllers_for("note")
        .into_iter()
        .map(|r| r.app)
        .collect();
    assert_eq!(apps, vec![APP_C, APP_A, APP_B]);
}

#[test]
fn a_doctype_without_a_controller_resolves_to_the_default() {
    let registry = ControllerRegistry::new();
    let resolved = registry.resolve("brand_new_doctype");
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].app, DEFAULT_APP);
    assert_eq!(resolved[0].controller.doctype(), "brand_new_doctype");
    assert!(resolved[0].controller.steps().is_empty());
    assert!(resolved[0].controller.methods().is_empty());
}

#[test]
fn a_doctype_added_after_the_set_was_built_resolves_to_the_default() {
    let mut registry = ControllerRegistry::new();
    registry.register(
        APP_A,
        Arc::new(Probe::new("note")),
        ControllerSource::Native,
    );
    let set = registry.set();

    // The set predates this DocType entirely.
    let resolved = set.resolve("late_doctype");
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].app, DEFAULT_APP);
}

#[test]
fn source_is_recorded_for_the_listing() {
    let mut registry = ControllerRegistry::new();
    registry.register(APP_A, Arc::new(Probe::new("note")), ControllerSource::Wasm);
    let registered = &registry.controllers_for("note")[0];
    assert_eq!(registered.source, ControllerSource::Wasm);
}

#[test]
fn method_index_lists_the_app_and_required_right() {
    let mut registry = ControllerRegistry::new();
    registry.register(
        APP_A,
        Arc::new(
            Probe::new("note")
                .with_method(method("read_note", false))
                .with_method(method("rename_note", true)),
        ),
        ControllerSource::Native,
    );
    let methods = registry.methods("note").expect("no conflict");
    assert_eq!(methods.len(), 2);
    let read = methods.get("read_note").expect("declared");
    assert_eq!(read.app, APP_A);
    assert_eq!(read.right, Right::Read);
    assert!(!read.mutates);
    let write = methods.get("rename_note").expect("declared");
    assert_eq!(write.right, Right::Write);
    assert!(write.mutates);
}

#[test]
fn unregister_removes_controllers_steps_and_methods_atomically() {
    let mut registry = ControllerRegistry::new();
    registry
        .register(
            APP_A,
            Arc::new(
                Probe::new("note")
                    .with_step(step("app_a:fill", save_point(), 0))
                    .with_method(method("a_only", true)),
            ),
            ControllerSource::Native,
        )
        .register(
            APP_B,
            Arc::new(
                Probe::new("note")
                    .with_step(step("app_b:fill", save_point(), 0))
                    .with_method(method("b_only", true)),
            ),
            ControllerSource::Native,
        );

    assert!(registry.unregister_app(APP_A));
    assert!(!registry.unregister_app(APP_A), "second call is a no-op");

    let apps: Vec<String> = registry
        .controllers_for("note")
        .into_iter()
        .map(|r| r.app)
        .collect();
    assert_eq!(apps, vec![APP_B]);
    let methods = registry.methods("note").expect("no conflict");
    assert!(methods.get("a_only").is_none());
    assert!(methods.get("b_only").is_some());

    // Its steps are gone from the installed registry too.
    let seams = registry.install().expect("no conflicts");
    assert!(
        seams.plan("note", &save_point()).is_empty() || {
            seams
                .plan("note", &save_point())
                .iter()
                .all(|p| p.app == APP_B)
        }
    );
}

#[test]
fn registering_produces_a_new_set_and_leaves_the_old_one_intact() {
    let mut registry = ControllerRegistry::new();
    registry.register(
        APP_A,
        Arc::new(Probe::new("note")),
        ControllerSource::Native,
    );
    let first = registry.set();
    registry.register(
        APP_B,
        Arc::new(Probe::new("note")),
        ControllerSource::Native,
    );
    let second = registry.set();

    assert_eq!(first.controllers_for("note").len(), 1);
    assert_eq!(second.controllers_for("note").len(), 2);
}

/// A controller whose one step declares a well-formed effect class no platform list contains.
struct UnknownClass;

impl Controller for UnknownClass {
    fn doctype(&self) -> &str {
        "note"
    }

    fn steps(&self) -> Vec<StepDecl> {
        vec![StepDecl {
            name: StepName::new("app_a:ship").unwrap(),
            point: HookPoint::of(Phase::Committed, Target::Op(Operation::Post)),
            priority: 0,
            budget: Duration::from_millis(10),
        }]
    }

    fn step_effects(&self) -> Vec<StepEffects> {
        vec![StepEffects::new(
            StepName::new("app_a:ship").unwrap(),
            vec![EffectDecl::new(
                EffectKind::External,
                EffectClassName::new("carrier_pigeon").unwrap(),
                vec![StoreName::new("outbound").unwrap()],
                Reversal::Irreversible {
                    reason: "released".to_owned(),
                },
            )],
        )]
    }

    fn run_step<'a>(
        &'a self,
        _step: &'a StepName,
        _ctx: &'a mut dyn HookContext,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move { Ok(()) })
    }
}

/// `install()` is lenient on purpose (it is for tests and tools, not the platform host): an
/// unknown but well-formed effect class is accepted. A host uses `for_platform()` and
/// `install_into`, which refuse it.
#[test]
fn install_stays_lenient_about_an_unknown_effect_class() {
    let mut registry = ControllerRegistry::new();
    registry.register(APP_A, Arc::new(UnknownClass), ControllerSource::Native);

    let installed = registry.install();
    if let Err(conflicts) = &installed {
        let codes: Vec<&str> = conflicts.iter().map(|c| c.code).collect();
        assert!(!codes.contains(&"effects.unknown_class"), "{codes:?}");
    }
    let seams = installed.expect("the lenient install accepts an unknown class");

    let plan = seams.plan(
        "note",
        &HookPoint::of(Phase::Committed, Target::Op(Operation::Post)),
    );
    assert_eq!(plan.len(), 1);
    let effects = plan[0].controller.step_effects();
    assert_eq!(effects.len(), 1, "the declared effect is visible");
    assert_eq!(effects[0].effects.len(), 1);
}
