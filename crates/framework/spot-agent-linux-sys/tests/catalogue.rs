//! The module's catalogue and handlers: the whole catalogue builds, every operation has a
//! registered handler, and a verified plan runs through the handler.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use spot_agent_core::{HandlerOutcome, HandlerRegistry, OperationHandler};
use spot_agent_linux_sys::catalogue::{LinuxSysModule, operations};
use spot_agent_linux_sys::handler::LinuxSysHandler;
use spot_core::{Clock, ManualClock};
use spot_mgmt_ops::{Catalogue, OpId, OperationModule};

fn clock() -> Arc<dyn Clock> {
    Arc::new(ManualClock::new(time::OffsetDateTime::UNIX_EPOCH))
}

#[test]
fn the_module_registers_a_valid_catalogue() {
    let modules: Vec<Box<dyn OperationModule>> = vec![Box::new(LinuxSysModule)];
    let catalogue = Catalogue::from_modules(&modules).unwrap();
    assert_eq!(catalogue.len(), operations().len());
    assert_eq!(catalogue.len(), 26);
    // A representative read and write from each family.
    for id in [
        "os.storage.facts",
        "os.snapshot.restore",
        "os.network.set",
        "os.cert.install",
        "os.time.step",
        "os.sysctl.set",
        "os.mac.mode",
    ] {
        assert!(catalogue.get(&OpId::new(id).unwrap()).is_some(), "{id}");
    }
}

#[test]
fn every_operation_has_a_registered_handler() {
    let runner = Arc::new(common::runner("fedora"));
    let fs = Arc::new(common::fs("fedora"));
    let handler = Arc::new(LinuxSysHandler::new(runner, fs, clock()));
    let mut registry = HandlerRegistry::new();
    LinuxSysHandler::register(&handler, &mut registry);
    assert_eq!(registry.len(), operations().len());
    for spec in operations() {
        assert!(
            registry.contains(&spec.id),
            "{} has no handler",
            spec.id.as_str()
        );
    }
}

#[test]
fn a_verified_read_plan_runs_through_the_handler() {
    let runner = Arc::new(common::runner("fedora"));
    let fs = Arc::new(common::fs("fedora"));
    let handler = Arc::new(LinuxSysHandler::new(runner, fs, clock()));
    let mut registry = HandlerRegistry::new();
    LinuxSysHandler::register(&handler, &mut registry);
    let registered: &Arc<dyn OperationHandler> = registry
        .get(&OpId::new("os.mac.status").unwrap())
        .expect("registered");
    let plan = plan("os.mac.status", Vec::new());
    assert_eq!(registered.handle(&plan), HandlerOutcome::Succeeded);
}

/// Builds a plan for a read operation, mirroring what the executor hands a handler.
fn plan(
    op: &str,
    params: Vec<(&str, spot_mgmt_ops::ParamValue)>,
) -> spot_agent_core::ExecutionPlan {
    let op = OpId::new(op).unwrap();
    let validated = spot_mgmt_ops::Validated {
        op: op.clone(),
        params: params
            .into_iter()
            .map(|(name, value)| spot_mgmt_ops::AcceptedParam {
                name: name.to_owned(),
                ty: spot_mgmt_ops::ParamType::Text {
                    max_len: 4096,
                    pattern: None,
                },
                value,
                from_default: false,
                secret: false,
            })
            .collect(),
    };
    spot_agent_core::ExecutionPlan::new(spot_agent_core::PlanParts {
        agent_id: "agent-1".to_owned(),
        command_id: "c1".to_owned(),
        command_digest: [1u8; 32],
        op,
        args: validated,
        risk: spot_mgmt_ops::Risk::Read,
        signers: Vec::new(),
        issued_at: 1_000,
    })
}
