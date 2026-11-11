//! Registration by modules: two toy modules that conflict on an id produce one error
//! naming both; `from_modules` assembles a catalogue.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use spot_mgmt_ops::{Catalogue, CatalogueBuilder, OperationModule};

/// A toy Linux module: registers the restart operation (like `common::service_restart`).
struct LinuxModule;

impl OperationModule for LinuxModule {
    fn name(&self) -> &str {
        "linux"
    }

    fn register(&self, b: &mut CatalogueBuilder) {
        b.add(self.name(), common::service_restart());
    }
}

/// A toy Windows module that registers the SAME id: the build must name both modules.
struct WindowsModule;

impl OperationModule for WindowsModule {
    fn name(&self) -> &str {
        "windows"
    }

    fn register(&self, b: &mut CatalogueBuilder) {
        b.add(self.name(), common::service_restart());
    }
}

/// A toy backup module with a distinct id.
struct BackupModule;

impl OperationModule for BackupModule {
    fn name(&self) -> &str {
        "backup"
    }

    fn register(&self, b: &mut CatalogueBuilder) {
        b.add(self.name(), common::backup_run());
    }
}

#[test]
fn two_modules_conflicting_on_an_id_error_names_both() {
    // Heterogeneous modules: boxed as the trait's objects behind `from_modules`' slice.
    let modules: Vec<Box<dyn OperationModule>> =
        vec![Box::new(LinuxModule), Box::new(WindowsModule)];
    let errors = Catalogue::from_modules(&modules).expect_err("the id collides");
    assert_eq!(errors.len(), 1, "{errors:#?}");
    assert_eq!(errors[0].code(), "ops.duplicate_id");
    let message = errors[0].to_string();
    assert!(message.contains("linux"), "{message}");
    assert!(message.contains("windows"), "{message}");
    assert!(message.contains("server.service.restart"), "{message}");
    // The order is deterministic: the FIRST registrant is named first.
    let linux_pos = message.find("linux").expect("linux named");
    let windows_pos = message.find("windows").expect("windows named");
    assert!(linux_pos < windows_pos, "{message}");
}

#[test]
fn non_conflicting_modules_build_one_catalogue() {
    let modules: Vec<Box<dyn OperationModule>> =
        vec![Box::new(LinuxModule), Box::new(BackupModule)];
    let catalogue = Catalogue::from_modules(&modules).expect("no clash");
    assert_eq!(catalogue.len(), 2);
    assert!(
        catalogue
            .get(&spot_mgmt_ops::OpId::new("server.service.restart").unwrap())
            .is_some()
    );
    assert!(
        catalogue
            .get(&spot_mgmt_ops::OpId::new("tenant.backup.run").unwrap())
            .is_some()
    );
}

#[test]
fn a_builder_direct_flow_matches_from_modules() {
    let mut builder = CatalogueBuilder::new();
    LinuxModule.register(&mut builder);
    BackupModule.register(&mut builder);
    let built = builder.build().expect("valid");
    let modules: Vec<Box<dyn OperationModule>> =
        vec![Box::new(LinuxModule), Box::new(BackupModule)];
    let direct = Catalogue::from_modules(&modules).expect("valid");
    let ids_left: Vec<&str> = built.iter().map(|s| s.id.as_str()).collect();
    let ids_right: Vec<&str> = direct.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids_left, ids_right);
}
