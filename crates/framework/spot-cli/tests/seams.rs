//! The hosts build their step registry with the strict platform constructor: a
//! misspelt effect class fails the build, and the sample app's real controllers still install.
#![allow(missing_docs, clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use spot_app_api::{
    AppResult, BoxFuture, Controller, EffectClassName, EffectDecl, EffectKind, HookContext,
    HookPoint, Operation, Phase, Reversal, StepDecl, StepEffects, StepName, StoreName, Target,
};
use spot_cli::{platform_registry, platform_registry_builder};
use spot_controllers::{ControllerRegistry, ControllerSource};

struct Sender {
    class: String,
}

impl Controller for Sender {
    fn doctype(&self) -> &str {
        "Generic Doc"
    }

    fn steps(&self) -> Vec<StepDecl> {
        vec![StepDecl {
            name: StepName::new("host:send").unwrap(),
            point: HookPoint::of(Phase::Committed, Target::Op(Operation::Post)),
            priority: 0,
            budget: Duration::from_millis(10),
        }]
    }

    fn step_effects(&self) -> Vec<StepEffects> {
        vec![StepEffects::new(
            StepName::new("host:send").unwrap(),
            vec![EffectDecl::new(
                EffectKind::External,
                EffectClassName::new(&self.class).unwrap(),
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

fn codes<T>(built: Result<T, Vec<spot_seams::Conflict>>) -> Vec<String> {
    match built {
        Ok(_) => Vec::new(),
        Err(conflicts) => conflicts.iter().map(|c| c.code.to_owned()).collect(),
    }
}

fn with_sender(class: &str) -> Result<spot_seams::Registry, Vec<spot_seams::Conflict>> {
    let mut builder = platform_registry_builder().unwrap();
    builder.add_controller(
        "host",
        Arc::new(Sender {
            class: class.to_owned(),
        }),
    );
    builder.build()
}

#[test]
fn the_host_registry_refuses_a_misspelt_effect_class() {
    assert_eq!(codes(with_sender("mial")), ["effects.unknown_class"]);
}

#[test]
fn the_host_registry_accepts_every_platform_class() {
    // The one shared list in `spot-seams` (WP-EXT-02.T5); no restated literal.
    for class in spot_seams::PLATFORM_EFFECT_CLASSES {
        assert!(with_sender(class).is_ok(), "{class}");
    }
    // The host's re-export is that same list.
    assert_eq!(
        spot_cli::PLATFORM_EFFECT_CLASSES,
        spot_seams::PLATFORM_EFFECT_CLASSES
    );
}

#[test]
fn the_engine_registry_is_strict_and_builds_without_controllers() {
    // The seed and bootstrap engines use this one: it builds, and it is the strict kind (an
    // empty strict builder without the class set would fail, so a pass here proves the call).
    assert!(platform_registry().is_ok());
}

#[test]
fn the_sample_apps_real_controllers_install_into_the_host_registry() {
    let mut registry = ControllerRegistry::new();
    for controller in spot_sample_app::controllers() {
        registry.register("sample", controller, ControllerSource::Native);
    }
    let mut builder = platform_registry_builder().unwrap();
    registry.install_into(&mut builder);
    assert!(builder.build().is_ok());
}
