//! The bridge from a [`LinuxOperation`] to the host agent's [`OperationHandler`].
//!
//! The executor runs only registered handlers, keyed by catalogue operation id; there is no
//! default handler. [`register_handlers`] builds one [`LinuxHandler`] per Linux operation and
//! registers it, so the agent can run exactly the `os.*` operations the catalogue declares and
//! nothing else.
//!
//! A handler receives a verified [`ExecutionPlan`](spot_agent_core::ExecutionPlan) carrying the
//! validated arguments, calls the module's `apply` (which re-validates and then verifies), and
//! maps the result onto a [`HandlerOutcome`]. The failure detail is the error's code and its
//! short, secret-free message.

use std::sync::Arc;

use spot_agent_core::{ExecutionPlan, HandlerOutcome, HandlerRegistry, OperationHandler};
use spot_mgmt_ops::OpId;

use crate::context::LinuxContext;
use crate::error::LinuxError;
use crate::firewall::{FirewallList, FirewallRule, FirewallStatus};
use crate::packages::{
    PackageEnv, PackageInstall, PackageList, PackageRemove, PackageRepositories, UpdateApply,
    UpdateCheck,
};
use crate::preview::LinuxOperation;
use crate::services::{ServiceControl, ServiceEffect, ServiceFailed, ServiceStatus};
use crate::users::{UserCreate, UserDelete, UserGroupMembership, UserGroups, UserList, UserLock};

/// One Linux operation, as the executor sees it.
pub struct LinuxHandler {
    module: Arc<dyn LinuxOperation>,
}

impl LinuxHandler {
    /// A handler over `module`.
    #[must_use]
    pub fn new(module: Arc<dyn LinuxOperation>) -> Self {
        Self { module }
    }

    /// The module this handler runs.
    #[must_use]
    pub fn module(&self) -> &Arc<dyn LinuxOperation> {
        &self.module
    }
}

impl OperationHandler for LinuxHandler {
    fn handle(&self, plan: &ExecutionPlan) -> HandlerOutcome {
        match self.module.apply(plan.args()) {
            Ok(_) => HandlerOutcome::Succeeded,
            Err(error) => HandlerOutcome::Failed {
                detail: describe(&error),
            },
        }
    }
}

impl std::fmt::Debug for LinuxHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinuxHandler")
            .field("op", &self.module.op().as_str())
            .finish()
    }
}

/// The failure detail: the stable code and the error's own message.
fn describe(error: &LinuxError) -> String {
    format!("{}: {error}", error.code())
}

/// Every Linux operation module, in a stable order, over `context`.
#[must_use]
pub fn modules(context: &Arc<LinuxContext>) -> Vec<Arc<dyn LinuxOperation>> {
    let env = PackageEnv::new(context.runner.clone(), context.fs.clone());
    vec![
        Arc::new(PackageList::new(env.clone())),
        Arc::new(PackageInstall::new(env.clone())),
        Arc::new(PackageRemove::new(env.clone())),
        Arc::new(PackageRepositories::new(env.clone())),
        Arc::new(UpdateCheck::new(env.clone())),
        Arc::new(UpdateApply::new(env)),
        Arc::new(ServiceStatus::new(context.clone())),
        Arc::new(ServiceControl::new(context.clone(), ServiceEffect::Start)),
        Arc::new(ServiceControl::new(context.clone(), ServiceEffect::Stop)),
        Arc::new(ServiceControl::new(context.clone(), ServiceEffect::Restart)),
        Arc::new(ServiceControl::new(context.clone(), ServiceEffect::Enable)),
        Arc::new(ServiceControl::new(context.clone(), ServiceEffect::Disable)),
        Arc::new(ServiceControl::new(context.clone(), ServiceEffect::Mask)),
        Arc::new(ServiceFailed::new(context.clone())),
        Arc::new(UserList::new(context.clone())),
        Arc::new(UserCreate::new(context.clone())),
        Arc::new(UserLock::new(context.clone(), true)),
        Arc::new(UserLock::new(context.clone(), false)),
        Arc::new(UserDelete::new(context.clone())),
        Arc::new(UserGroups::new(context.clone())),
        Arc::new(UserGroupMembership::new(context.clone(), true)),
        Arc::new(UserGroupMembership::new(context.clone(), false)),
        Arc::new(FirewallStatus::new(context.clone())),
        Arc::new(FirewallList::new(context.clone())),
        Arc::new(FirewallRule::new(context.clone(), true)),
        Arc::new(FirewallRule::new(context.clone(), false)),
    ]
}

/// Registers one handler for every Linux operation.
pub fn register_handlers(registry: &mut HandlerRegistry, context: &Arc<LinuxContext>) {
    for module in modules(context) {
        let op: OpId = module.op().clone();
        registry.register(op, Arc::new(LinuxHandler::new(module)));
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::testing::FakeSystem;

    #[test]
    fn every_catalogue_operation_has_a_handler() {
        let system = FakeSystem::fedora();
        let context = Arc::new(LinuxContext::new(system.runner(), system.filesystem()));
        let mut registry = HandlerRegistry::new();
        register_handlers(&mut registry, &context);
        let catalogue = crate::catalogue::build_catalogue().unwrap();
        assert_eq!(registry.len(), catalogue.len());
        for spec in catalogue.iter() {
            assert!(
                registry.contains(&spec.id),
                "{} has a handler",
                spec.id.as_str()
            );
        }
    }

    #[test]
    fn describe_carries_the_code() {
        let error = LinuxError::PackageLocked {
            backend: "dnf".to_owned(),
        };
        assert!(describe(&error).starts_with("agent_linux.package_locked:"));
    }

    use spot_agent_core::{ExecutionPlan, PlanParts};
    use spot_mgmt_ops::{Invocation, ParamValue};

    /// A verified plan for `op`, as the executor would hand a handler.
    fn plan_for(op: &str, params: Vec<(&str, ParamValue)>) -> ExecutionPlan {
        let catalogue = crate::catalogue::build_catalogue().unwrap();
        let mut invocation = Invocation::new(OpId::new(op).unwrap());
        for (name, value) in params {
            invocation = invocation.with(name, value);
        }
        let args = catalogue.validate(&invocation).unwrap();
        let risk = catalogue.get(&args.op).unwrap().risk;
        ExecutionPlan::new(PlanParts {
            agent_id: "agent-1".to_owned(),
            command_id: "c1".to_owned(),
            command_digest: [0u8; 32],
            op: args.op.clone(),
            args,
            risk,
            signers: Vec::new(),
            issued_at: 0,
        })
    }

    fn fedora_registry() -> (Arc<LinuxContext>, HandlerRegistry, FakeSystem) {
        let system = FakeSystem::fedora();
        let context = Arc::new(LinuxContext::new(system.runner(), system.filesystem()));
        let mut registry = HandlerRegistry::new();
        register_handlers(&mut registry, &context);
        (context, registry, system)
    }

    #[test]
    fn a_handler_runs_the_operation_through_a_plan() {
        let (_context, registry, system) = fedora_registry();
        let plan = plan_for(
            "os.package.install",
            vec![(
                "packages",
                ParamValue::List(vec![ParamValue::Text("nginx".to_owned())]),
            )],
        );
        let handler = registry.get(plan.op()).unwrap();
        assert_eq!(handler.handle(&plan), HandlerOutcome::Succeeded);
        assert!(system.snapshot().is_installed("nginx"));
    }

    #[test]
    fn a_protected_stop_fails_with_the_stable_code() {
        let (_context, registry, _system) = fedora_registry();
        let plan = plan_for(
            "os.service.stop",
            vec![("unit", ParamValue::Text("spot-agent.service".to_owned()))],
        );
        let handler = registry.get(plan.op()).unwrap();
        match handler.handle(&plan) {
            HandlerOutcome::Failed { detail } => {
                assert!(detail.contains("agent_linux.protected_unit"), "{detail}");
            }
            HandlerOutcome::Succeeded => panic!("a protected stop must fail without the flag"),
        }
    }
}
