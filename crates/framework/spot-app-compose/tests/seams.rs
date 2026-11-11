//! Composition contributes to `spot-seams`' registry: the same conflict detection and the same
//! `explain` output as any other app, including two extensions that conflict.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use spot_app_api::{
    AppResult, BoxFuture, Controller, EffectClassName, EffectDecl, EffectKind, HookContext,
    HookPoint, Operation, OverrideKind, Phase, Reversal, StepDecl, StepEffects, StepName,
    StepOverride, StoreName, Target,
};
use spot_app_compose::seams::{SeamNeed, StepContribution, build_registry};
use spot_seams::Role;

struct OneStep {
    doctype: String,
    step: StepName,
}

impl Controller for OneStep {
    fn doctype(&self) -> &str {
        &self.doctype
    }

    fn steps(&self) -> Vec<StepDecl> {
        vec![StepDecl {
            name: self.step.clone(),
            point: point(),
            priority: 0,
            budget: Duration::from_millis(10),
        }]
    }

    fn run_step<'a>(
        &'a self,
        _step: &'a StepName,
        _ctx: &'a mut dyn HookContext,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move { Ok(()) })
    }
}

fn point() -> HookPoint {
    HookPoint::of(Phase::Validate, Target::Save)
}

fn controller(app: &str, step: &str) -> Arc<dyn Controller> {
    Arc::new(OneStep {
        doctype: "Generic Doc".to_owned(),
        step: StepName::new(&format!("{app}:{step}")).unwrap(),
    })
}

fn wrap(app: &str, step: &str, target: &str) -> StepOverride {
    StepOverride {
        target: StepName::new(target).unwrap(),
        kind: OverrideKind::Wrap,
        by: Some(StepName::new(&format!("{app}:{step}")).unwrap()),
        reason: format!("{app} wraps {target}"),
    }
}

#[test]
fn a_wrapping_extension_is_planned_and_explained() {
    let host = StepContribution::new("host", controller("host", "adjust"));
    let ext = StepContribution::new("tax_extension", controller("tax_extension", "wrap"))
        .override_step(wrap("tax_extension", "wrap", "host:adjust"));
    let registry = build_registry(&[host, ext]).unwrap();

    let plan = registry.plan("Generic Doc", &point());
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0].step.as_str(), "tax_extension:wrap");
    assert_eq!(
        plan[0].wrapped.as_ref().unwrap().step.as_str(),
        "host:adjust"
    );

    let explain = registry.explain("Generic Doc", &point());
    assert_eq!(explain.len(), 1);
    assert!(matches!(explain[0].role, Role::Wrapper { .. }));
}

#[test]
fn two_extensions_wrapping_one_step_conflict() {
    let host = StepContribution::new("host", controller("host", "adjust"));
    let first = StepContribution::new("ext_a", controller("ext_a", "wrap")).override_step(wrap(
        "ext_a",
        "wrap",
        "host:adjust",
    ));
    let second = StepContribution::new("ext_b", controller("ext_b", "wrap")).override_step(wrap(
        "ext_b",
        "wrap",
        "host:adjust",
    ));
    let conflicts = match build_registry(&[host, first, second]) {
        Ok(_) => panic!("two wrappers of one step must conflict"),
        Err(conflicts) => conflicts,
    };
    assert!(
        conflicts
            .iter()
            .any(|c| c.code == "override.duplicate_wrap"),
        "{conflicts:?}"
    );
}

#[test]
fn a_missing_seam_is_reported() {
    let ext = StepContribution::new("tax_extension", controller("tax_extension", "adjust"))
        .require(SeamNeed::new("pricing.adjust", 1));
    let conflicts = match build_registry(&[ext]) {
        Ok(_) => panic!("a required seam with no provider must conflict"),
        Err(conflicts) => conflicts,
    };
    assert!(
        conflicts.iter().any(|c| c.code == "seam.required_missing"),
        "{conflicts:?}"
    );
}

#[test]
fn registration_order_does_not_change_the_result() {
    let host_a = StepContribution::new("host", controller("host", "adjust"));
    let ext_a = StepContribution::new("tax_extension", controller("tax_extension", "wrap"))
        .override_step(wrap("tax_extension", "wrap", "host:adjust"));
    let host_b = StepContribution::new("host", controller("host", "adjust"));
    let ext_b = StepContribution::new("tax_extension", controller("tax_extension", "wrap"))
        .override_step(wrap("tax_extension", "wrap", "host:adjust"));

    let first = build_registry(&[host_a, ext_a]).unwrap();
    let second = build_registry(&[ext_b, host_b]).unwrap();
    let steps = |r: &spot_seams::Registry| {
        r.plan("Generic Doc", &point())
            .iter()
            .map(|p| p.step.as_str().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(steps(&first), steps(&second));
}

/// A controller whose one step declares one external effect of `class`.
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

fn sender(class: &str) -> StepContribution {
    StepContribution::new(
        "host",
        Arc::new(Sender {
            class: class.to_owned(),
        }),
    )
}

#[test]
fn the_composition_registry_refuses_a_misspelt_effect_class() {
    let conflicts = match build_registry(&[sender("mial")]) {
        Ok(_) => panic!("a misspelt class must be refused"),
        Err(conflicts) => conflicts,
    };
    let codes: Vec<&str> = conflicts.iter().map(|c| c.code).collect();
    assert_eq!(codes, ["effects.unknown_class"]);
}

#[test]
fn the_composition_registry_accepts_every_platform_class() {
    for class in spot_app_compose::seams::PLATFORM_EFFECT_CLASSES {
        assert!(build_registry(&[sender(class)]).is_ok(), "{class}");
    }
    assert_eq!(
        spot_app_compose::seams::PLATFORM_EFFECT_CLASSES,
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
