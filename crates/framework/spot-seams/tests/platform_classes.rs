//! The one shared platform effect class list (WP-EXT-02.T5).
#![allow(missing_docs, clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use spot_app_api::{
    AppResult, BoxFuture, Controller, EffectClassName, EffectDecl, EffectKind, HookContext,
    HookPoint, Operation, Phase, Reversal, StepDecl, StepEffects, StepName, StoreName, Target,
};
use spot_seams::{PLATFORM_EFFECT_CLASSES, Registry, RegistryBuilder, platform_effect_classes};

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

fn build_with_sender(class: &str) -> Result<Registry, Vec<spot_seams::Conflict>> {
    let mut builder = RegistryBuilder::for_platform();
    builder.known_effect_classes(&platform_effect_classes());
    builder.add_controller(
        "host",
        Arc::new(Sender {
            class: class.to_owned(),
        }),
    );
    builder.build()
}

#[test]
fn every_name_is_a_valid_effect_class_name() {
    for class in PLATFORM_EFFECT_CLASSES {
        assert!(EffectClassName::new(class).is_ok(), "{class}");
    }
    assert_eq!(
        platform_effect_classes().len(),
        PLATFORM_EFFECT_CLASSES.len()
    );
}

#[test]
fn the_list_has_no_duplicates() {
    let mut sorted = PLATFORM_EFFECT_CLASSES.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), PLATFORM_EFFECT_CLASSES.len());
}

#[test]
fn the_list_holds_the_eight_platform_classes() {
    assert_eq!(
        PLATFORM_EFFECT_CLASSES,
        [
            "mail",
            "bank_api",
            "webhook",
            "connector",
            "sms",
            "push",
            "edge_sync",
            "fiscal_transmission"
        ]
    );
}

#[test]
fn a_platform_registry_accepts_each_class() {
    for class in PLATFORM_EFFECT_CLASSES {
        assert!(build_with_sender(class).is_ok(), "{class}");
    }
}

#[test]
fn a_platform_registry_refuses_a_misspelt_class() {
    let conflicts = build_with_sender("mial").err().unwrap();
    let codes: Vec<&str> = conflicts.iter().map(|c| c.code).collect();
    assert_eq!(codes, ["effects.unknown_class"]);
}
