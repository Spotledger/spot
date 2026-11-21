#![allow(dead_code, clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value as Json;
use spot_app_api::seams::SeamProvider;
use spot_app_api::{
    AppResult, BoxFuture, Controller, HookContext, HookPoint, Operation, OverrideKind, Phase,
    StepDecl, StepName, StepOverride, Target,
};
use spot_seams::{Conflict, PlannedStep, Registry, RegistryBuilder};

pub struct Ctrl {
    pub doctype: String,
    pub steps: Vec<StepDecl>,
}

impl Controller for Ctrl {
    fn doctype(&self) -> &str {
        &self.doctype
    }
    fn steps(&self) -> Vec<StepDecl> {
        self.steps.clone()
    }
    fn run_step<'a>(
        &'a self,
        _step: &'a StepName,
        _ctx: &'a mut dyn HookContext,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async { Ok(()) })
    }
}

pub fn sn(s: &str) -> StepName {
    StepName::new(s).unwrap()
}

pub fn before_save() -> HookPoint {
    HookPoint::of(Phase::Before, Target::Save)
}

pub fn before_create() -> HookPoint {
    HookPoint::of(Phase::Before, Target::Op(Operation::Create))
}

pub fn decl(name: &str, point: HookPoint, priority: i32) -> StepDecl {
    StepDecl {
        name: sn(name),
        point,
        priority,
        budget: Duration::from_millis(100),
    }
}

pub fn ctrl(doctype: &str, steps: Vec<StepDecl>) -> Arc<dyn Controller> {
    Arc::new(Ctrl {
        doctype: doctype.to_owned(),
        steps,
    })
}

pub fn ov(target: &str, kind: OverrideKind, by: Option<&str>) -> StepOverride {
    StepOverride {
        target: sn(target),
        kind,
        by: by.map(sn),
        reason: format!("{kind:?} {target}"),
    }
}

pub fn names(plan: &[PlannedStep]) -> Vec<String> {
    plan.iter().map(|p| p.step.to_string()).collect()
}

pub fn codes(r: Result<Registry, Vec<Conflict>>) -> Vec<&'static str> {
    match r {
        Ok(_) => Vec::new(),
        Err(c) => c.iter().map(|c| c.code).collect(),
    }
}

/// App `core` with steps `core:a` (10), `core:b` (20), `core:c` (30) on `before_save`, plus
/// app `ext` with helper steps `ext:x`, `ext:y`, `ext:z` (priority 50) on `load`.
pub fn base() -> RegistryBuilder {
    let mut b = RegistryBuilder::new();
    b.add_controller(
        "core",
        ctrl(
            "item",
            vec![
                decl("core:a", before_save(), 10),
                decl("core:b", before_save(), 20),
                decl("core:c", before_save(), 30),
            ],
        ),
    );
    b.add_controller(
        "ext",
        ctrl(
            "item",
            vec![
                decl("ext:x", HookPoint::Load, 50),
                decl("ext:y", HookPoint::Load, 50),
                decl("ext:z", HookPoint::Load, 50),
            ],
        ),
    );
    b
}

pub struct Const(pub Json);

impl SeamProvider for Const {
    fn call(&self, _input: Json) -> BoxFuture<'_, AppResult<Json>> {
        let v = self.0.clone();
        Box::pin(async move { Ok(v) })
    }
}
