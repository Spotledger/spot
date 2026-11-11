//! The check's broken-app fixtures: an app that breaks one thing in an otherwise complete
//! app must report exactly the expected `pack.*` codes at exactly the expected paths, and
//! nothing else. The happy path first, so a failure in it is never confused with a broken
//! fixture.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

mod common;

/// Builds a complete valid app, replacing the manifest or the controllers file when given and
/// adding `extra`, then returns the directory. One helper so every fixture differs from the
/// good app in exactly the rows the test names.
fn app(
    dir: &Path,
    manifest: Option<&[u8]>,
    controllers: Option<&[u8]>,
    extra: &[(&str, Vec<u8>)],
) -> std::path::PathBuf {
    let mut plain: Vec<(&str, Vec<u8>)> = vec![
        (
            "manifest.toml",
            manifest.map_or_else(
                || common::manifest().into_bytes(),
                std::borrow::ToOwned::to_owned,
            ),
        ),
        ("doctypes/customer.toml", common::CUSTOMER_DOCTYPE.to_vec()),
        ("permissions/grants.toml", common::GRANTS.to_vec()),
        (
            "controllers.toml",
            controllers.map_or_else(
                || common::CONTROLLERS.to_vec(),
                std::borrow::ToOwned::to_owned,
            ),
        ),
        ("assets/logo.svg", common::LOGO_SVG.to_vec()),
        ("app.wasm", common::plain_component()),
    ];
    plain.extend(extra.iter().cloned());
    common::write_app(dir, &plain);
    dir.to_path_buf()
}

/// One fresh, owned temp directory per test.
fn sandbox() -> (tempfile::TempDir, std::path::PathBuf) {
    common::workdir()
}

/// The happy path: a complete valid app checks clean.
#[test]
fn a_complete_app_checks_clean() {
    let (_keep, root) = sandbox();
    let app_dir = app(&root.join("app"), None, None, &[]);
    let report = spot_pack::check_app(&app_dir, None);
    assert!(
        report.is_ok(),
        "the complete app must check clean, saw:\n{}",
        common::codes_of(&report)
    );
    assert!(
        report.manifest.is_some(),
        "the parsed manifest is in the report"
    );
}

/// A manifest-less app: exactly `pack.manifest_missing`, the early exit, nothing else —
/// the rest of the check cannot run without the manifest.
#[test]
fn missing_manifest_is_manifest_missing_only() {
    let (_keep, root) = sandbox();
    let dir = root.join("app");
    common::write_app(
        &dir,
        &[("doctypes/customer.toml", common::CUSTOMER_DOCTYPE.to_vec())],
    );
    let report = spot_pack::check_app(&dir, None);
    common::assert_problems(&report, &[("pack.manifest_missing", None)]);
}

/// A manifest that does not parse: `pack.manifest` at `manifest.toml`, and the report holds
/// no manifest. The rest of the app is fine, so nothing else is reported.
#[test]
fn broken_manifest_is_manifest() {
    let (_keep, root) = sandbox();
    let app_dir = app(
        &root.join("app"),
        Some(b"[app]\nname = \"\"\nversion = \"1.0\"\n"),
        None,
        &[],
    );
    let report = spot_pack::check_app(&app_dir, None);
    common::assert_problems(&report, &[("pack.manifest", Some("manifest.toml"))]);
    assert!(report.manifest.is_none());
    common::messages_about(&report, "manifest.toml", "name");
}

/// A manifest that parses but breaks a cross-row rule: the same `[[events]]` row twice.
/// `pack.manifest` carries the rule's own message.
#[test]
fn duplicate_subscription_is_manifest() {
    let (_keep, root) = sandbox();
    let m: &[u8] = b"[app]\n\
                    name = \"test_app\"\n\
                    version = \"1.0.0\"\n\
                    \n\
                    [[events]]\n\
                    doctype = \"customer\"\n\
                    event = \"before_save\"\n\
                    step = \"test_app:on_save\"\n\
                    \n\
                    [[events]]\n\
                    doctype = \"customer\"\n\
                    event = \"before_save\"\n\
                    step = \"test_app:on_save\"\n";
    let app_dir = app(&root.join("app"), Some(m), None, &[]);
    let report = spot_pack::check_app(&app_dir, None);
    common::assert_problems(&report, &[("pack.manifest", Some("manifest.toml"))]);
    assert!(report.manifest.is_none());
    common::messages_about(&report, "manifest.toml", "twice");
}

/// A DocType carrying a key the model does not know: `pack.doctype` at the file, with
/// spot-meta's own message naming the key.
#[test]
fn unknown_key_doctype_is_doctype_at_its_file() {
    let (_keep, root) = sandbox();
    let app_dir = app(
        &root.join("app"),
        None,
        None,
        &[(
            "doctypes/customer.toml",
            b"name = \"customer\"\nnonsense-key = true\n".to_vec(),
        )],
    );
    let report = spot_pack::check_app(&app_dir, None);
    common::assert_problems(&report, &[("pack.doctype", Some("doctypes/customer.toml"))]);
    common::messages_about(&report, "doctypes/customer.toml", "nonsense-key");
}

/// A DocType whose decimal field breaks the precision rule (scale above precision):
/// `pack.doctype` with spot-meta's own message.
#[test]
fn doctype_bad_decimal_is_doctype() {
    let (_keep, root) = sandbox();
    let app_dir = app(
        &root.join("app"),
        None,
        None,
        &[(
            "doctypes/customer.toml",
            b"name = \"customer\"\n\
             label = \"Customer\"\n\
             app = \"test_app\"\n\
             module = \"sales\"\n\
             \n\
             [[fields]]\n\
             name = \"customer_name\"\n\
             label = \"Customer Name\"\n\
             type = \"text\"\n\
             required = true\n\
             \n\
             [[fields]]\n\
             name = \"weight\"\n\
             label = \"Weight\"\n\
             type = \"decimal\"\n\
             scale = 7\n\
             precision = 4\n"
                .to_vec(),
        )],
    );
    let report = spot_pack::check_app(&app_dir, None);
    common::assert_problems(&report, &[("pack.doctype", Some("doctypes/customer.toml"))]);
    common::messages_about(&report, "doctypes/customer.toml", "precision");
}

/// A grant naming a right the model does not know: `pack.permissions` at the grants file's
/// declared place, with the loader's own message.
#[test]
fn unknown_right_is_permissions_at_the_grant_file() {
    let (_keep, root) = sandbox();
    let app_dir = app(
        &root.join("app"),
        None,
        None,
        &[(
            "permissions/grants.toml",
            b"[[entries]]\n\
             role = \"Reader\"\n\
             doctype = \"customer\"\n\
             rights = [\"steal\"]\n"
                .to_vec(),
        )],
    );
    let report = spot_pack::check_app(&app_dir, None);
    common::assert_problems(
        &report,
        &[("pack.permissions", Some("permissions/grants.toml"))],
    );
    common::messages_about(&report, "permissions/grants.toml", "steal");
}

/// A controllers file missing its `[controllers]` table: `pack.controllers`, and with it the
/// manifest's step unresolved (`pack.controller_missing`), because no row ever loaded.
#[test]
fn badly_formed_controllers_file_is_controllers_and_missing() {
    let (_keep, root) = sandbox();
    let app_dir = app(
        &root.join("app"),
        None,
        Some(b"\"test_app:on_save\" = \"wasm:on-save\"\n"),
        &[],
    );
    let report = spot_pack::check_app(&app_dir, None);
    common::assert_problems(
        &report,
        &[
            // The stable order is by code, then path: `pack.controller_missing` sorts before
            // `pack.controllers`.
            ("pack.controller_missing", Some("controllers.toml")),
            ("pack.controllers", Some("controllers.toml")),
        ],
    );
    let parse_message = &report.problems[1].message;
    assert!(
        parse_message.contains("inside `[controllers]`"),
        "the parse fault names the rule: {parse_message}"
    );
}

/// The manifest names a step no controller provides: `pack.controller_missing` at
/// `controllers.toml`, naming the step. The other row's `wasm:` reference is fine, so the
/// report shows exactly the one gap.
#[test]
fn step_without_controller_is_controller_missing() {
    let (_keep, root) = sandbox();
    let app_dir = app(
        &root.join("app"),
        None,
        Some(b"[controllers]\n\"the-other-step\" = \"wasm:on-save\"\n"),
        &[],
    );
    let report = spot_pack::check_app(&app_dir, None);
    common::assert_problems(
        &report,
        &[("pack.controller_missing", Some("controllers.toml"))],
    );
    common::messages_about(&report, "controllers.toml", "on_save");
}

/// The controllers file names a reference no prefix defines: `pack.controller_unknown`.
#[test]
fn unknown_reference_is_controller_unknown() {
    let (_keep, root) = sandbox();
    let app_dir = app(
        &root.join("app"),
        None,
        Some(b"[controllers]\n\"test_app:on_save\" = \"py:run\"\n"),
        &[],
    );
    let report = spot_pack::check_app(&app_dir, None);
    common::assert_problems(
        &report,
        &[("pack.controller_unknown", Some("controllers.toml"))],
    );
    common::messages_about(&report, "controllers.toml", "py:run");
}

/// A `native:` reference outside the build's controller list: `pack.controller_unknown`,
/// with the list supplied. Without a list the same reference is uncheckable and allowed.
#[test]
fn native_reference_outside_list_is_controller_unknown() {
    let (_keep, root) = sandbox();
    let app_dir = app(
        &root.join("app"),
        None,
        Some(b"[controllers]\n\"test_app:on_save\" = \"native:does_not_exist\"\n"),
        &[],
    );
    let report = spot_pack::check_app(&app_dir, Some(&common::native_names()));
    common::assert_problems(
        &report,
        &[("pack.controller_unknown", Some("controllers.toml"))],
    );
    common::messages_about(&report, "controllers.toml", "does_not_exist");

    let uncheckable = spot_pack::check_app(&app_dir, None);
    assert!(
        uncheckable.is_ok(),
        "without a list a native: reference is uncheckable, saw:\n{}",
        common::codes_of(&uncheckable)
    );
}

/// A `native:` reference inside the build's list checks clean; the unreferenced component is
/// still carried by the pack, which is not a complaint.
#[test]
fn native_reference_inside_list_checks_clean() {
    let (_keep, root) = sandbox();
    let app_dir = app(
        &root.join("app"),
        None,
        Some(b"[controllers]\n\"test_app:on_save\" = \"native:on-save\"\n"),
        &[],
    );
    let report = spot_pack::check_app(&app_dir, Some(&common::native_names()));
    assert!(
        report.is_ok(),
        "a native: name in the build's list must pass, saw:\n{}",
        common::codes_of(&report)
    );
}

/// The controllers name a `wasm:` export but the app carries no component:
/// `pack.component_missing` at `app.wasm`.
#[test]
fn component_absent_is_component_missing() {
    let (_keep, root) = sandbox();
    let app_dir = app(&root.join("app"), None, None, &[]);
    std::fs::remove_file(app_dir.join("app.wasm")).expect("the component is removed");
    let report = spot_pack::check_app(&app_dir, None);
    common::assert_problems(&report, &[("pack.component_missing", Some("app.wasm"))]);
    common::messages_about(&report, "app.wasm", "no app.wasm");
}

/// The component does not export the name a `wasm:` reference asks for:
/// `pack.export_missing` at `app.wasm`.
#[test]
fn export_absent_is_export_missing() {
    let (_keep, root) = sandbox();
    let app_dir = app(
        &root.join("app"),
        None,
        Some(b"[controllers]\n\"test_app:on_save\" = \"wasm:does_not_exist\"\n"),
        &[],
    );
    let report = spot_pack::check_app(&app_dir, None);
    common::assert_problems(&report, &[("pack.export_missing", Some("app.wasm"))]);
    common::messages_about(&report, "app.wasm", "does_not_exist");
}

/// `app.wasm` holds a core module, not a component: `pack.wasm` at `app.wasm`, naming the
/// header the parser rejected. The bytes are the assembled `(module (func (export
/// "on-save") nop))`, fixed here so no `wat` dependency is needed.
#[test]
fn core_module_as_component_is_wasm() {
    let (_keep, root) = sandbox();
    let core_module: Vec<u8> = vec![
        0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, 0x01, 0x04, 0x01, 0x60, 0x00, 0x00, 0x03,
        0x02, 0x01, 0x00, 0x07, 0x0b, 0x01, 0x07, 0x6f, 0x6e, 0x2d, 0x73, 0x61, 0x76, 0x65, 0x00,
        0x00, 0x0a, 0x05, 0x01, 0x03, 0x00, 0x01, 0x0b,
    ];
    let app_dir = app(&root.join("app"), None, None, &[("app.wasm", core_module)]);
    let report = spot_pack::check_app(&app_dir, None);
    common::assert_problems(&report, &[("pack.wasm", Some("app.wasm"))]);
    common::messages_about(&report, "app.wasm", "component header");
}

/// The component imports what the host does not provide: `pack.unknown_import` at
/// `app.wasm`, naming the import.
#[test]
fn unknown_import_is_unknown_import() {
    let (_keep, root) = sandbox();
    let app_dir = app(
        &root.join("app"),
        None,
        None,
        &[("app.wasm", common::stranger_component())],
    );
    let report = spot_pack::check_app(&app_dir, None);
    common::assert_problems(&report, &[("pack.unknown_import", Some("app.wasm"))]);
    common::messages_about(&report, "app.wasm", "make-coffee");
}

/// The component imports `get-doc` and the manifest declares no DocType read access:
/// `pack.import_without_capability` at `manifest.toml` — the installer's check, surfaced at
/// pack time.
#[test]
fn import_without_capability_is_import_without_capability() {
    let (_keep, root) = sandbox();
    let app_dir = app(
        &root.join("app"),
        None,
        Some(b"[controllers]\n\"test_app:on_save\" = \"wasm:on-read\"\n"),
        &[("app.wasm", common::reading_component())],
    );
    let report = spot_pack::check_app(&app_dir, None);
    common::assert_problems(
        &report,
        &[("pack.import_without_capability", Some("manifest.toml"))],
    );
    common::messages_about(&report, "manifest.toml", "get-doc");
}

/// The manifest declares DocType read access the component never exercises:
/// `pack.capability_not_imported` at `manifest.toml`.
#[test]
fn capability_without_import_is_capability_not_imported() {
    let (_keep, root) = sandbox();
    let m: &[u8] = b"[app]\n\
                    name = \"test_app\"\n\
                    version = \"1.0.0\"\n\
                    \n\
                    [capabilities]\n\
                    doctypes = { read = [\"Customer\"] }\n\
                    \n\
                    [[assets]]\n\
                    path = \"assets/logo.svg\"\n";
    let app_dir = app(&root.join("app"), Some(m), None, &[]);
    let report = spot_pack::check_app(&app_dir, None);
    common::assert_problems(
        &report,
        &[("pack.capability_not_imported", Some("manifest.toml"))],
    );
    common::messages_about(&report, "manifest.toml", "get-doc");
}

/// The capability pair in agreement — read access declared for a component that imports
/// `get-doc` — checks clean, pinning that the two checks do not double-report.
#[test]
fn capability_and_import_in_agreement_checks_clean() {
    let (_keep, root) = sandbox();
    let m: &[u8] = b"[app]\n\
                    name = \"test_app\"\n\
                    version = \"1.0.0\"\n\
                    \n\
                    [capabilities]\n\
                    doctypes = { read = [\"Customer\"] }\n\
                    \n\
                    [[events]]\n\
                    doctype = \"customer\"\n\
                    event = \"before_save\"\n\
                    step = \"test_app:on_save\"\n\
                    \n\
                    [[assets]]\n\
                    path = \"assets/logo.svg\"\n";
    let app_dir = app(
        &root.join("app"),
        Some(m),
        Some(b"[controllers]\n\"test_app:on_save\" = \"wasm:on-read\"\n"),
        &[("app.wasm", common::reading_component())],
    );
    let report = spot_pack::check_app(&app_dir, None);
    assert!(
        report.is_ok(),
        "the capability pair must agree, saw:\n{}",
        common::codes_of(&report)
    );
}

/// A `ts:` reference whose script does not exist: `pack.script` naming the file.
#[test]
fn missing_script_is_script() {
    let (_keep, root) = sandbox();
    let app_dir = app(
        &root.join("app"),
        None,
        Some(b"[controllers]\n\"test_app:on_save\" = \"ts:scripts/handler.ts\"\n"),
        &[],
    );
    let report = spot_pack::check_app(&app_dir, None);
    common::assert_problems(&report, &[("pack.script", Some("scripts/handler.ts"))]);
    common::messages_about(&report, "scripts/handler.ts", "does not exist");
}

/// A `ts:` script that exists but breaks a script-tier rule: `pack.script`, the message
/// naming the transpile run's diagnostics. An off-allow-list import (`ts.import_not_allowed`)
/// is the deterministic case the brief's script-tier rules refuse.
#[test]
fn bad_script_is_script_with_diagnostics() {
    let (_keep, root) = sandbox();
    let app_dir = app(
        &root.join("app"),
        None,
        Some(b"[controllers]\n\"test_app:on_save\" = \"ts:scripts/handler.ts\"\n"),
        &[(
            "scripts/handler.ts",
            b"import { save } from \"lodash\";\n\nexport const run = (): void => save({});\n"
                .to_vec(),
        )],
    );
    let report = spot_pack::check_app(&app_dir, None);
    common::assert_problems(&report, &[("pack.script", Some("scripts/handler.ts"))]);
    common::messages_about(&report, "scripts/handler.ts", "transpile");
}

/// A `ts:` script that transpiles cleanly checks clean.
#[test]
fn good_script_checks_clean() {
    let (_keep, root) = sandbox();
    let app_dir = app(
        &root.join("app"),
        None,
        Some(b"[controllers]\n\"test_app:on_save\" = \"ts:scripts/handler.ts\"\n"),
        &[("scripts/handler.ts", b"export const one = 1;\n".to_vec())],
    );
    let report = spot_pack::check_app(&app_dir, None);
    assert!(
        report.is_ok(),
        "a clean script must check clean, saw:\n{}",
        common::codes_of(&report)
    );
}

/// A `ts:` reference outside the app directory: `pack.script` at `controllers.toml` — the
/// reference is what is bad, before any file is looked at.
#[test]
fn escaping_script_path_is_script_at_controllers() {
    let (_keep, root) = sandbox();
    let app_dir = app(
        &root.join("app"),
        None,
        Some(b"[controllers]\n\"test_app:on_save\" = \"ts:../handler.ts\"\n"),
        &[],
    );
    let report = spot_pack::check_app(&app_dir, None);
    common::assert_problems(&report, &[("pack.script", Some("controllers.toml"))]);
    common::messages_about(&report, "controllers.toml", "plain app-relative path");
}

/// A file outside the allowed layout: `pack.layout` at the file's path.
#[test]
fn file_outside_layout_is_layout() {
    let (_keep, root) = sandbox();
    let app_dir = app(
        &root.join("app"),
        None,
        None,
        &[("loose.txt", b"stray\n".to_vec())],
    );
    let report = spot_pack::check_app(&app_dir, None);
    common::assert_problems(&report, &[("pack.layout", Some("loose.txt"))]);
}

/// A directory outside the allowed layout: `pack.layout` at the directory's path — the walk
/// faults the directory itself and does not descend into it.
#[test]
fn directory_outside_layout_is_layout() {
    let (_keep, root) = sandbox();
    let app_dir = app(
        &root.join("app"),
        None,
        None,
        &[("media/photo.jpg", b"\xff\xd8\xff\xe0\n".to_vec())],
    );
    let report = spot_pack::check_app(&app_dir, None);
    common::assert_problems(&report, &[("pack.layout", Some("media"))]);
}

/// A file in `permissions/` that is not one of the four model files: `pack.layout`.
#[test]
fn extra_file_in_permissions_is_layout() {
    let (_keep, root) = sandbox();
    let app_dir = app(
        &root.join("app"),
        None,
        None,
        &[("permissions/extra.toml", b"x = 1\n".to_vec())],
    );
    let report = spot_pack::check_app(&app_dir, None);
    common::assert_problems(&report, &[("pack.layout", Some("permissions/extra.toml"))]);
}

/// A symlink inside the app tree: `pack.path_escape` — the file it names sits wherever the
/// author's machine points, so the walk must refuse it, never follow it. This is the one
/// escape the OS's own path normalisation cannot show (`..` never survives a directory
/// listing); the symlink does.
#[test]
fn symlinked_file_is_path_escape() {
    let (_keep, root) = sandbox();
    let outside = root.join("outside.txt");
    std::fs::write(&outside, b"secret\n").expect("the outside file exists");
    let app_dir = app(&root.join("app"), None, None, &[]);
    std::os::unix::fs::symlink(&outside, app_dir.join("assets/link.svg"))
        .expect("the symlink is created (unix-only fixture)");
    let report = spot_pack::check_app(&app_dir, None);
    common::assert_problems(&report, &[("pack.path_escape", Some("assets/link.svg"))]);
}

/// A symlinked directory must not be walked into either: `pack.path_escape` at the link.
#[test]
fn symlinked_directory_is_path_escape() {
    let (_keep, root) = sandbox();
    let outside = root.join("elsewhere");
    std::fs::create_dir(&outside).expect("the outside directory exists");
    std::fs::write(outside.join("stolen.toml"), b"theft\n").expect("the outside file exists");
    let app_dir = app(&root.join("app"), None, None, &[]);
    std::os::unix::fs::symlink(&outside, app_dir.join("assets/linked"))
        .expect("the symlink is created (unix-only fixture)");
    let report = spot_pack::check_app(&app_dir, None);
    common::assert_problems(&report, &[("pack.path_escape", Some("assets/linked"))]);
}

/// One file over the per-file byte limit: `pack.oversized` naming the file; the message
/// states both sizes.
#[test]
fn oversized_file_is_oversized() {
    let (_keep, root) = sandbox();
    let big = vec![b'x'; 128 * 1024 * 1024 + 1];
    let app_dir = app(&root.join("app"), None, None, &[("assets/big.bin", big)]);
    let report = spot_pack::check_app(&app_dir, None);
    common::assert_problems(&report, &[("pack.oversized", Some("assets/big.bin"))]);
    common::messages_about(&report, "assets/big.bin", "limit");
}

/// More files than the bundle's entry limit: `pack.too_many_files`, one aggregate problem.
#[test]
fn too_many_files_is_too_many_files() {
    let (_keep, root) = sandbox();
    let mut extra: Vec<(String, Vec<u8>)> = Vec::new();
    for i in 0..4097 {
        extra.push((format!("assets/f{i}.bin"), vec![b'x'; 4]));
    }
    let refs: Vec<(&str, Vec<u8>)> = extra.iter().map(|(r, b)| (r.as_str(), b.clone())).collect();
    let app_dir = app(&root.join("app"), None, None, &refs);
    let report = spot_pack::check_app(&app_dir, None);
    common::assert_problems(&report, &[("pack.too_many_files", None)]);
    let message = &report.problems[0].message;
    assert!(
        message.contains("limit") && message.contains("4100"),
        "the message states the count and the limit: {message}"
    );
}

/// Every problem at once: an app broken in six independent places reports all of them in one
/// report, in the stable code-then-path order.
#[test]
fn all_problems_at_once() {
    let (_keep, root) = sandbox();
    let app_dir = app(
        &root.join("app"),
        None,
        Some(
            b"[controllers]\n\
             \"test_app:on_save\" = \"py:run\"\n\
             \"other:step\" = \"native:missing\"\n",
        ),
        &[
            (
                "doctypes/customer.toml",
                b"name = \"customer\"\nnonsense-key = true\n".to_vec(),
            ),
            (
                "permissions/grants.toml",
                b"[[entries]]\n\
                 role = \"Reader\"\n\
                 doctype = \"customer\"\n\
                 rights = [\"steal\"]\n"
                    .to_vec(),
            ),
            ("loose.txt", b"stray\n".to_vec()),
            ("media/photo.jpg", b"\xff\xd8\xff\xe0\n".to_vec()),
        ],
    );
    let report = spot_pack::check_app(&app_dir, Some(&common::native_names()));
    common::assert_problems(
        &report,
        &[
            ("pack.controller_unknown", Some("controllers.toml")),
            ("pack.controller_unknown", Some("controllers.toml")),
            ("pack.doctype", Some("doctypes/customer.toml")),
            ("pack.layout", Some("loose.txt")),
            ("pack.layout", Some("media")),
            ("pack.permissions", Some("permissions/grants.toml")),
        ],
    );
}

/// A step declared twice in the controllers file: `pack.controllers` naming the repeat, and
/// with it the manifest's step unresolved, because no row ever loaded.
#[test]
fn duplicate_step_in_controllers_is_controllers() {
    let (_keep, root) = sandbox();
    let app_dir = app(
        &root.join("app"),
        None,
        Some(
            b"[controllers]\n\
             \"test_app:on_save\" = \"wasm:on-save\"\n\
             \"test_app:on_save\" = \"wasm:on-save\"\n",
        ),
        &[],
    );
    let report = spot_pack::check_app(&app_dir, None);
    common::assert_problems(
        &report,
        &[
            ("pack.controller_missing", Some("controllers.toml")),
            ("pack.controllers", Some("controllers.toml")),
        ],
    );
    let parse_message = &report.problems[1].message;
    assert!(
        parse_message.contains("declared twice"),
        "the parse fault names the repeat: {parse_message}"
    );
}

/// A manifest asset that the app does not carry: `pack.asset_unpacked` at the asset path.
#[test]
fn missing_asset_is_asset_unpacked() {
    let (_keep, root) = sandbox();
    let m: &[u8] = b"[app]\n\
                    name = \"test_app\"\n\
                    version = \"1.0.0\"\n\
                    \n\
                    [[events]]\n\
                    doctype = \"customer\"\n\
                    event = \"before_save\"\n\
                    step = \"test_app:on_save\"\n\
                    \n\
                    [[assets]]\n\
                    path = \"assets/missing.svg\"\n";
    let app_dir = app(&root.join("app"), Some(m), None, &[]);
    let report = spot_pack::check_app(&app_dir, None);
    common::assert_problems(
        &report,
        &[("pack.asset_unpacked", Some("assets/missing.svg"))],
    );
    common::messages_about(&report, "assets/missing.svg", "does not carry");
}
