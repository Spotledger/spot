//! The Windows operation catalogue: it builds, every id has a spec and a handler, and the
//! risk levels match the brief.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use spot_agent_windows::FakeWin;
use spot_agent_windows::catalogue::operations;
use spot_mgmt_ops::{Catalogue, OpId, Risk};

#[test]
fn the_catalogue_builds_with_every_operation() {
    let catalogue: Catalogue = common::catalogue();
    assert_eq!(catalogue.len(), operations().len());
    assert!(
        catalogue.len() >= 20,
        "the Windows module covers the four families"
    );
}

#[test]
fn every_operation_gets_a_handler() {
    let channel = std::sync::Arc::new(common::RecordingChannel::default());
    let registry = common::registry(std::sync::Arc::new(FakeWin::stock()), channel);
    assert_eq!(registry.len(), operations().len());
    for operation in operations() {
        assert!(
            registry.contains(&operation.id),
            "{} has no handler",
            operation.id.as_str()
        );
    }
}

#[test]
fn the_risk_levels_match_the_brief() {
    let catalogue = common::catalogue();
    let risk = |op: &str| {
        catalogue
            .get(&OpId::new(op).unwrap())
            .expect("the operation is registered")
            .risk
    };
    assert_eq!(
        risk(spot_agent_windows::catalogue::SERVICE_LIST),
        Risk::Read
    );
    assert_eq!(
        risk(spot_agent_windows::catalogue::SERVICE_STOP),
        Risk::Medium
    );
    assert_eq!(
        risk(spot_agent_windows::catalogue::UPDATE_INSTALL),
        Risk::High
    );
    assert_eq!(
        risk(spot_agent_windows::catalogue::UPDATE_SCHEDULE_REBOOT),
        Risk::Critical
    );
    assert_eq!(
        risk(spot_agent_windows::catalogue::USER_RESET_PASSWORD),
        Risk::Critical
    );
}

#[test]
fn the_module_name_is_windows() {
    use spot_mgmt_ops::OperationModule;
    assert_eq!(
        spot_agent_windows::AgentWindowsModule.name(),
        spot_agent_windows::MODULE_NAME
    );
    // Registering the module twice on one builder is the catalogue's duplicate-id error,
    // which proves the module owns every id exactly once.
    let mut builder = spot_mgmt_ops::CatalogueBuilder::new();
    spot_agent_windows::AgentWindowsModule.register(&mut builder);
    spot_agent_windows::AgentWindowsModule.register(&mut builder);
    let errors = builder.build().expect_err("duplicate ids are refused");
    assert!(
        errors
            .iter()
            .all(|error| error.code() == "ops.duplicate_id")
    );
}
