#![allow(clippy::unwrap_used, clippy::expect_used)]

use spot_mgmt_ops::{AcceptedParam, Invocation, OpId, ParamType, ParamValue};

use super::*;
use crate::catalogue::specs;
use crate::testing::FakeSystem;

fn validated(op: &str, params: Vec<(&str, ParamValue)>) -> Validated {
    let catalogue = crate::catalogue::build_catalogue().unwrap();
    let mut invocation = Invocation::new(OpId::new(op).unwrap());
    for (name, value) in params {
        invocation = invocation.with(name, value);
    }
    catalogue.validate(&invocation).unwrap()
}

fn packages(names: &[&str]) -> ParamValue {
    ParamValue::List(
        names
            .iter()
            .map(|name| ParamValue::Text((*name).to_owned()))
            .collect(),
    )
}

fn env(system: &FakeSystem) -> PackageEnv {
    PackageEnv::new(system.runner(), system.filesystem())
}

#[test]
fn the_backend_is_detected_from_os_release() {
    assert_eq!(
        PackageBackend::detect(&*FakeSystem::fedora().filesystem()).unwrap(),
        PackageBackend::Dnf
    );
    assert_eq!(
        PackageBackend::detect(&*FakeSystem::rhel().filesystem()).unwrap(),
        PackageBackend::Dnf
    );
    assert_eq!(
        PackageBackend::detect(&*FakeSystem::ubuntu().filesystem()).unwrap(),
        PackageBackend::Apt
    );
}

#[test]
fn install_is_idempotent_and_verifies() {
    let system = FakeSystem::fedora();
    let module = PackageInstall::new(env(&system));
    let args = validated(PACKAGE_INSTALL, vec![("packages", packages(&["nginx"]))]);
    let preview = module.plan(&args).unwrap();
    assert!(!preview.is_empty());
    assert_eq!(preview.steps[0].command.program, "dnf");
    let outcome = module.apply(&args).unwrap();
    assert_eq!(outcome.applied, 1);
    assert!(system.snapshot().is_installed("nginx"));
    // The second plan is empty: the desired state holds.
    assert!(module.plan(&args).unwrap().is_empty());
}

#[test]
fn install_of_an_installed_package_is_a_no_op() {
    let system = FakeSystem::fedora();
    let module = PackageInstall::new(env(&system));
    let args = validated(PACKAGE_INSTALL, vec![("packages", packages(&["bash"]))]);
    assert!(module.plan(&args).unwrap().is_empty());
}

#[test]
fn install_refuses_while_the_package_manager_is_locked() {
    let system = FakeSystem::fedora();
    system.set_package_locked(true);
    let module = PackageInstall::new(env(&system));
    let args = validated(PACKAGE_INSTALL, vec![("packages", packages(&["nginx"]))]);
    let error = module.plan(&args).unwrap_err();
    assert_eq!(error.code(), crate::error::PACKAGE_LOCKED);
}

#[test]
fn the_schema_refuses_a_leading_dash_package() {
    let catalogue = crate::catalogue::build_catalogue().unwrap();
    let invocation =
        Invocation::new(OpId::new(PACKAGE_INSTALL).unwrap()).with("packages", packages(&["-y"]));
    assert!(catalogue.validate(&invocation).is_err());
}

#[test]
fn the_module_refuses_a_leading_dash_package() {
    // A hand-built invocation can bypass the catalogue's schema; the module still refuses
    // the value, so the second check is real.
    let system = FakeSystem::fedora();
    let module = PackageInstall::new(env(&system));
    let args = Validated {
        op: crate::ids::op_id(PACKAGE_INSTALL),
        params: vec![AcceptedParam {
            name: "packages".to_owned(),
            ty: ParamType::List(Box::new(ParamType::Text {
                max_len: 128,
                pattern: None,
            })),
            value: packages(&["-y"]),
            from_default: false,
            secret: false,
        }],
    };
    let error = module.plan(&args).unwrap_err();
    assert_eq!(error.code(), crate::error::INVALID_ARGUMENT);
}

#[test]
fn remove_is_idempotent() {
    let system = FakeSystem::ubuntu();
    let module = PackageRemove::new(env(&system));
    let args = validated(PACKAGE_REMOVE, vec![("packages", packages(&["bash"]))]);
    assert!(!module.plan(&args).unwrap().is_empty());
    module.apply(&args).unwrap();
    assert!(!system.snapshot().is_installed("bash"));
    assert!(module.plan(&args).unwrap().is_empty());
}

#[test]
fn update_apply_reports_a_reboot_and_is_idempotent() {
    let system = FakeSystem::rhel();
    system.add_update("kernel", false);
    system.add_update("curl", false);
    let module = UpdateApply::new(env(&system));
    let args = validated(UPDATE_APPLY, Vec::new());
    let preview = module.plan(&args).unwrap();
    assert_eq!(preview.steps.len(), 1);
    let outcome = module.apply(&args).unwrap();
    assert!(outcome.notes.iter().any(|note| note.contains("reboot")));
    assert!(module.plan(&args).unwrap().is_empty());
}

#[test]
fn security_only_leaves_other_updates() {
    let system = FakeSystem::fedora();
    system.add_update("openssl", true);
    system.add_update("vim", false);
    let module = UpdateApply::new(env(&system));
    let args = validated(
        UPDATE_APPLY,
        vec![("security_only", ParamValue::Bool(true))],
    );
    module.apply(&args).unwrap();
    let snapshot = system.snapshot();
    assert_eq!(snapshot.updates.len(), 1);
    assert_eq!(snapshot.updates[0].name, "vim");
    // The security-only plan is empty afterwards; the all-updates plan is not.
    assert!(module.plan(&args).unwrap().is_empty());
    let all = validated(UPDATE_APPLY, Vec::new());
    assert!(!module.plan(&all).unwrap().is_empty());
}

#[test]
fn update_check_reports_available_updates() {
    let system = FakeSystem::ubuntu();
    system.add_update("curl", false);
    let module = UpdateCheck::new(env(&system));
    let args = validated(UPDATE_CHECK, Vec::new());
    let outcome = module.apply(&args).unwrap();
    assert!(outcome.notes[0].contains("1 update"));
}

#[test]
fn the_catalogue_registers_every_package_operation() {
    let specs = specs();
    let ids: Vec<&str> = specs.iter().map(|spec| spec.id.as_str()).collect();
    for op in [
        PACKAGE_LIST,
        PACKAGE_INSTALL,
        PACKAGE_REMOVE,
        PACKAGE_REPOSITORIES,
    ] {
        assert!(ids.contains(&op), "{op} is registered");
    }
}
