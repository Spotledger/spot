//! The operation module registers a valid catalogue: no duplicate ids, no invalid
//! defaults, and every operation the brief names is present.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use spot_agent_windows_sys::{WindowsSysModule, op_id};
use spot_mgmt_ops::{Catalogue, OpId, OperationModule};

#[test]
fn the_module_builds_a_valid_catalogue() {
    let module = WindowsSysModule::new().unwrap();
    assert_eq!(module.len(), 10);

    let modules: Vec<Box<dyn OperationModule>> = vec![Box::new(module)];
    let catalogue = Catalogue::from_modules(&modules).unwrap();
    assert_eq!(catalogue.len(), 10);

    for id in [
        "vss.shadow.create",
        "vss.shadow.delete",
        "vss.shadow.list",
        "certificate.list",
        "certificate.import",
        "certificate.remove",
        "certificate.bind",
        "backup.mode.start",
        "backup.mode.stop",
        "snapshot.vss.run",
    ] {
        assert!(
            catalogue.get(&OpId::new(id).unwrap()).is_some(),
            "{id} must be registered"
        );
    }
}

#[test]
fn op_ids_are_built_from_segments() {
    let id = op_id(&["vss", "shadow", "create"]).unwrap();
    assert_eq!(id.as_str(), "vss.shadow.create");
    assert!(op_id(&["OnlyOneSegment"]).is_err());
}

#[test]
fn the_module_can_find_its_operations() {
    let module = WindowsSysModule::new().unwrap();
    assert_eq!(
        module.find(&["snapshot", "vss", "run"]).map(OpId::as_str),
        Some("snapshot.vss.run")
    );
    assert!(module.find(&["no", "such", "operation"]).is_none());
}
