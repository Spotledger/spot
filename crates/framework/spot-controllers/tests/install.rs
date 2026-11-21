//! Installing controllers into `spot-seams`: the plan, manifest overrides, and conflicts.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{APP_A, APP_B, Probe, manifest, method, step};
use spot_app_api::{
    AppResult, BoxFuture, Controller, EffectClassName, EffectDecl, EffectKind, HookContext,
    HookPoint, Operation, OverrideKind, Phase, Reversal, StepDecl, StepEffects, StepName,
    StepOverride, StoreName, Target,
};
use spot_controllers::{ControllerRegistry, ControllerSource, NativeControllers};

fn before_create() -> HookPoint {
    HookPoint::of(Phase::Before, Target::Op(Operation::Create))
}

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

[[overrides]]
target = "shop:fill"
kind = "replace"
by = "shop:swap"
reason = "use the swapped step"
"#;

#[test]
fn install_turns_steps_into_a_plan() {
    let mut registry = ControllerRegistry::new();
    registry.register(
        APP_A,
        Arc::new(Probe::new("note").with_step(step("app_a:fill", before_create(), 0))),
        ControllerSource::Native,
    );
    let seams = registry.install().expect("no conflicts");
    let plan = seams.plan("note", &before_create());
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0].step.as_str(), "app_a:fill");
    assert_eq!(plan[0].app, APP_A);
}

#[test]
fn the_planned_controller_delegates_to_the_registered_one() {
    let probe = Arc::new(
        Probe::new("note")
            .with_step(step("app_a:fill", before_create(), 7))
            .with_method(method("ping", false)),
    );
    let mut registry = ControllerRegistry::new();
    registry.register(APP_A, probe, ControllerSource::Native);

    let seams = registry.install().expect("no conflicts");
    let plan = seams.plan("note", &before_create());
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0].priority, 7, "the step's own priority is kept");
    // The wrapper is the planned controller and delegates everything else.
    assert_eq!(plan[0].controller.doctype(), "note");
    assert_eq!(plan[0].controller.methods().len(), 1);
}

#[test]
fn a_manifest_override_replaces_a_step_at_install() {
    let mut table = NativeControllers::new();
    table.insert(
        "shop",
        Arc::new(
            Probe::new("note")
                .with_step(step("shop:fill", before_create(), 0))
                .with_step(step("shop:swap", before_create(), 0)),
        ),
    );
    let mut registry = ControllerRegistry::new();
    registry
        .register_from_manifest(&manifest(SHOP), &table)
        .expect("native controller");

    let seams = registry.install().expect("the override installs cleanly");
    let plan = seams.plan("note", &before_create());
    let steps: Vec<&str> = plan.iter().map(|p| p.step.as_str()).collect();
    assert_eq!(steps, vec!["shop:swap"], "fill is replaced by swap");
}

#[test]
fn install_reports_every_conflict_at_once() {
    let mut registry = ControllerRegistry::new();
    // The same app declares the same step twice.
    registry.register(
        APP_A,
        Arc::new(Probe::new("note").with_step(step("app_a:fill", before_create(), 0))),
        ControllerSource::Native,
    );
    registry.register(
        APP_A,
        Arc::new(Probe::new("note").with_step(step("app_a:fill", before_create(), 0))),
        ControllerSource::Native,
    );
    // An override targets a step nobody declares.
    registry.add_override(
        APP_B,
        StepOverride {
            target: StepName::new("app_a:missing").expect("valid"),
            kind: OverrideKind::Disable,
            by: None,
            reason: "gone".to_owned(),
        },
    );

    let conflicts = registry.install().err().expect("both conflicts");
    let codes: Vec<&str> = conflicts.iter().map(|c| c.code).collect();
    assert!(codes.contains(&"step.duplicate"), "{codes:?}");
    assert!(codes.contains(&"override.unknown_target"), "{codes:?}");
}

fn committed_post() -> HookPoint {
    HookPoint::of(Phase::Committed, Target::Op(Operation::Post))
}

/// A controller that declares one effect class on its one step.
struct Declaring;

impl Controller for Declaring {
    fn doctype(&self) -> &str {
        "note"
    }

    fn steps(&self) -> Vec<StepDecl> {
        vec![StepDecl {
            name: StepName::new("app_a:send").unwrap(),
            point: committed_post(),
            priority: 0,
            budget: Duration::from_millis(10),
        }]
    }

    fn step_effects(&self) -> Vec<StepEffects> {
        vec![StepEffects::new(
            StepName::new("app_a:send").unwrap(),
            vec![EffectDecl::new(
                EffectKind::External,
                EffectClassName::new("mail").unwrap(),
                vec![StoreName::new("outbound").unwrap()],
                Reversal::Irreversible {
                    reason: "sent".to_owned(),
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

#[test]
fn the_installed_controller_keeps_its_declared_step_effects() {
    let mut registry = ControllerRegistry::new();
    registry.register(APP_A, Arc::new(Declaring), ControllerSource::Native);
    let seams = registry.install().expect("no conflicts");
    let plan = seams.plan("note", &committed_post());
    assert_eq!(plan.len(), 1);
    let effects = plan[0].controller.step_effects();
    assert_eq!(effects.len(), 1, "the declaration survives the wrapper");
    assert_eq!(effects[0].step.as_str(), "app_a:send");
    assert_eq!(effects[0].effects.len(), 1);
}
