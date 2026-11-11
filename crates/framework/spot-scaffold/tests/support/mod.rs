//! Shared proof helpers: generate a project, write it, and run the whole platform over it.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use spot_app_api::{AppResult, BoxFuture, Controller, HookContext, StepName};
use spot_controllers::{ControllerRegistry, NativeControllers};
use spot_pack::{PackOptions, check_app, pack_app};
use spot_permissions::ModelTexts;
use spot_scaffold::error::{Result, ScaffoldError};
use spot_scaffold::tree::{FileTree, WritePolicy};
use spot_storage_contract::schema::Collation;

/// Writes a generated tree into `project` (with `--force` for an edit).
pub fn write(project: &Path, tree: &FileTree, force: bool) {
    let policy = if force {
        WritePolicy::force()
    } else {
        WritePolicy::new()
    };
    tree.write_to_disk(project, &policy).expect("writes");
}

/// A stub native controller for the registry test.
struct Stub {
    doctype: String,
}

impl Controller for Stub {
    fn doctype(&self) -> &str {
        &self.doctype
    }

    fn steps(&self) -> Vec<spot_app_api::StepDecl> {
        Vec::new()
    }

    fn run_step<'a>(
        &'a self,
        _step: &'a StepName,
        _ctx: &'a mut dyn HookContext,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async { Ok(()) })
    }
}

/// Reads one permission file, or the empty text when it is absent.
fn read_text(app: &Path, rel: &str) -> String {
    std::fs::read_to_string(app.join(rel)).unwrap_or_default()
}

/// Runs the whole proof over a written project: pack check and pack, the DocTypes through
/// `spot-meta` into a catalogue, the permissions through `spot-permissions`, a controller
/// through the `spot-controllers` registry, and every script through `spot-ts-transpile`.
pub fn prove(project: &Path) {
    let app = project.join("app");

    // 1. spot-pack check and pack.
    let report = check_app(&app, None);
    assert!(
        report.is_ok(),
        "{}: pack check failed: {:?}",
        project.display(),
        report.problems
    );
    let (_, packed) = pack_app(&app, &PackOptions::default()).expect("the app packs");
    assert!(!packed.bytes.is_empty(), "the bundle has bytes");

    // 2. The DocTypes through spot-meta into a catalogue.
    let definitions = spot_meta::load_dir(&app.join("doctypes")).expect("the DocTypes load");
    assert!(!definitions.is_empty(), "at least one DocType");
    let meta = spot_meta::Registry::build(definitions, &spot_meta::Customisations::new())
        .expect("the DocTypes build");
    let catalog = meta
        .catalog(Collation::new("en").expect("a collation"))
        .expect("the catalog compiles");
    assert!(catalog.entities().count() >= 1);

    // 3. The permissions through spot-permissions.
    let texts = ModelTexts {
        grants: read_text(&app, "permissions/grants.toml"),
        field_grants: read_text(&app, "permissions/field_grants.toml"),
        groups: read_text(&app, "permissions/groups.toml"),
        shares: read_text(&app, "permissions/shares.toml"),
    };
    let model = spot_permissions::load(&texts).expect("the permissions load");
    assert!(!model.grants.is_empty(), "at least one grant");

    // 4. A generated controller registers against a test registry.
    let manifest = report.manifest.as_ref().expect("a parsed manifest");
    if !manifest.events.is_empty() {
        let doctype = manifest.events[0].doctype.clone();
        let mut table = NativeControllers::new();
        table.insert(
            manifest.app.name.as_str(),
            Arc::new(Stub {
                doctype: doctype.clone(),
            }),
        );
        let mut registry = ControllerRegistry::new();
        registry
            .register_from_manifest(manifest, &table)
            .expect("the generated controller registers");
        assert!(
            !registry.controllers_for(&doctype).is_empty(),
            "the controller is attached to {doctype}"
        );
    }

    // 5. Every script transpiles, and its reference reaches the registry factory.
    let scripts = app.join("scripts");
    if scripts.is_dir() {
        let mut found = false;
        for entry in std::fs::read_dir(&scripts).expect("scripts list") {
            let path = entry.expect("entry").path();
            if path.extension().is_some_and(|e| e == "ts") {
                let source = std::fs::read_to_string(&path).expect("script reads");
                let name = path.file_name().unwrap().to_string_lossy().into_owned();
                spot_ts_transpile::transpile(
                    &source,
                    &name,
                    &spot_ts_transpile::Options::default(),
                )
                .unwrap_or_else(|d| panic!("{name} does not transpile: {d:?}"));
                found = true;
            }
        }
        if found {
            let controllers = read_text(&app, "controllers.toml");
            assert!(
                controllers.contains("ts:scripts/"),
                "a ts: reference is mapped"
            );
            let reference = spot_controllers::ControllerRef::parse("ts:scripts/placeholder.ts")
                .expect("a valid reference");
            let error = ControllerRegistry::new()
                .register_from_manifest_ref(manifest, &reference, &NativeControllers::new())
                .err()
                .expect("no TypeScript adapter yet");
            assert_eq!(error.code, "controller.host_unavailable");
        }
    }

    // 6. The generated load test is on disk (its body is steps 2-3 above).
    let tests = project.join("tests");
    let mut saw_load_test = false;
    if tests.is_dir() {
        for entry in std::fs::read_dir(&tests).expect("tests list") {
            let path = entry.expect("entry").path();
            if path.extension().is_some_and(|e| e == "rs") {
                let source = std::fs::read_to_string(&path).expect("test reads");
                assert!(source.contains("spot_meta::load_dir"), "a load test");
                saw_load_test = true;
            }
        }
    }
    assert!(saw_load_test, "a generated load test exists");
}

/// Asserts a generator refused with `code`.
pub fn refused(result: Result<FileTree>, code: &str) {
    match result {
        Ok(_) => panic!("expected a refusal with {code}, but the generator succeeded"),
        Err(ScaffoldError { code: got, .. }) => {
            assert_eq!(got, code, "wrong refusal code");
        }
    }
}

/// A plain `acme` app tree, generated without touching disk.
pub fn plain_tree() -> FileTree {
    let spec = spot_scaffold::generate::app::AppSpec::new(
        spot_scaffold::names::AppName::parse("acme").expect("a valid app name"),
    );
    spot_scaffold::generate::app::generate(&spec).expect("generates")
}

/// A temporary project directory.
pub fn temp_project() -> tempfile::TempDir {
    tempfile::tempdir().expect("a temp dir")
}

/// The app directory of a project.
pub fn app_of(project: &Path) -> PathBuf {
    project.join("app")
}
