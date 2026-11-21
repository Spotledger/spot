//! The sample app installed through the registry: its manifest, its two controllers (WP-DOC-10.T2).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::manifest;
use spot_app_api::{Controller, HookPoint, Operation, Phase, Target};
use spot_controllers::{ControllerRegistry, NativeControllers};
use spot_sample_app::MANIFEST_TEXT;

/// The manifest text without the host-level `controller` key, which `spot-manifest` does not model.
fn sample_manifest() -> spot_manifest::AppManifest {
    let text: String = MANIFEST_TEXT
        .lines()
        .filter(|line| !line.trim_start().starts_with("controller"))
        .collect::<Vec<_>>()
        .join("\n");
    manifest(&text)
}

fn table() -> NativeControllers {
    let mut table = NativeControllers::new();
    for (name, doctype) in spot_sample_app::NATIVE_CONTROLLERS {
        let controller: Arc<dyn Controller> = spot_sample_app::controllers()
            .into_iter()
            .find(|c| c.doctype() == *doctype)
            .expect("the table names a controller the app ships");
        table.insert(name, controller);
    }
    table
}

fn every_point() -> Vec<HookPoint> {
    let mut points = Vec::new();
    for phase in [Phase::Before, Phase::Validate, Phase::After] {
        points.push(HookPoint::of(phase, Target::Save));
        for op in [
            Operation::Create,
            Operation::Update,
            Operation::Delete,
            Operation::Post,
            Operation::Cancel,
        ] {
            points.push(HookPoint::of(phase, Target::Op(op)));
        }
    }
    points
}

#[test]
fn the_sample_manifest_serves_task_and_time_entry_and_nothing_else() {
    let manifest = sample_manifest();
    let mut registry = ControllerRegistry::new();
    registry
        .register_from_manifest(&manifest, &table())
        .expect("the sample installs");
    let seams = registry.install().expect("no conflicts");
    let steps_at = |doctype: &str| -> usize {
        every_point()
            .iter()
            .map(|p| seams.plan(doctype, p).len())
            .sum()
    };
    assert_eq!(steps_at("time_entry"), 3, "Time Entry has its three steps");
    assert_eq!(steps_at("task"), 1, "Task has the owner derivation");
    assert_eq!(registry.controllers_for("task").len(), 1);
    assert_eq!(registry.controllers_for("time_entry").len(), 1);
    // Installing the sample changes nothing for a DocType it has no behaviour for: the
    // resolved controller stays the framework default, which declares no steps or methods.
    for other in ["project", "label"] {
        assert!(registry.controllers_for(other).is_empty());
        let resolved = registry.controller_for(other);
        assert_eq!(resolved.app, spot_controllers::DEFAULT_APP);
        assert!(resolved.controller.steps().is_empty());
    }
    for other in ["project", "label", "label_pick", "checklist_item"] {
        assert_eq!(steps_at(other), 0, "{other} has no behaviour");
    }
}
