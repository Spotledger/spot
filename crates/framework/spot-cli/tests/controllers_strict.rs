//! The running host installs its `ControllerSet` into the strict platform registry:
//! a misspelt effect class refuses the start-up, and the sample app's real controllers install.
#![allow(missing_docs, clippy::unwrap_used)]

use std::time::Duration;

use spot_app_api::{
    AppResult, BoxFuture, Controller, EffectClassName, EffectDecl, EffectKind, HookContext,
    HookPoint, Operation, Phase, Reversal, StepDecl, StepEffects, StepName, StoreName, Target,
};
use spot_cli::services::controllers::install_strict;
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

fn set_with(class: &str) -> std::sync::Arc<spot_controllers::ControllerSet> {
    let mut registry = ControllerRegistry::new();
    registry.register(
        "host",
        std::sync::Arc::new(Sender {
            class: class.to_owned(),
        }),
        ControllerSource::Native,
    );
    registry.set()
}

#[test]
fn the_host_install_path_refuses_a_misspelt_effect_class() {
    let refused = install_strict(&set_with("mial")).err().unwrap();
    assert!(refused.0.contains("effects.unknown_class"), "{}", refused.0);
}

#[test]
fn the_host_install_path_accepts_a_known_effect_class() {
    assert!(install_strict(&set_with("mail")).is_ok());
}

#[test]
fn the_sample_apps_real_controllers_still_build() {
    let mut registry = ControllerRegistry::new();
    for controller in spot_sample_app::controllers() {
        registry.register("sample", controller, ControllerSource::Native);
    }
    assert!(install_strict(&registry.set()).is_ok());
}
