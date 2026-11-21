//! Helpers the test files share (a module, not a test).
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use serde_json::{Value as Json, json};
use spot_app_api::meta::FieldKind;
use spot_app_api::prelude::RecordId;
use spot_app_testkit::AppHarness;
use spot_scaffold::generate::app::{AppSpec, generate};
use spot_scaffold::names::AppName;
use spot_scaffold::tree::WritePolicy;

/// The sample app of the repository, with the two fixture patches the server's own end-to-end
/// tests make (`spot-cli/tests/common`): the `confidential` protection group of Task's `details`
/// and the `created_by` field the owner rule compares. The patched copy is made once per process.
pub fn sample_dir() -> PathBuf {
    static PATCHED: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    PATCHED
        .get_or_init(|| {
            let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../apps/sample");
            let copy = std::env::temp_dir().join(format!(
                "spot_testkit_sample_{}_{}",
                std::process::id(),
                RecordId::new_v7()
            ));
            copy_dir(&source, &copy);
            let task = copy.join("doctypes/task.toml");
            // A Windows checkout with `core.autocrlf` has `\r\n` line endings, where a multi-line
            // pattern matches nothing: normalise first, and refuse a patch that changes nothing
            // (a silent no-op would drop the protection group and a Viewer would read the field).
            let text = patched(
                &std::fs::read_to_string(&task).unwrap().replace("\r\n", "\n"),
                &[
                    (
                        "title_field = \"subject\"",
                        "title_field = \"subject\"\n\n[[protection_groups]]\nname = \"confidential\"\nlabel = \"Confidential\"\ntrace = true",
                    ),
                    (
                        "name = \"details\"\nlabel = \"Details\"\ntype = \"long_text\"",
                        "name = \"details\"\nlabel = \"Details\"\ntype = \"long_text\"\nprotection_group = \"confidential\"",
                    ),
                ],
            );
            std::fs::write(&task, text).unwrap();
            let entry = copy.join("doctypes/time_entry.toml");
            let text = std::fs::read_to_string(&entry).unwrap()
                + "\n[[fields]]\nname = \"created_by\"\nlabel = \"Created By\"\ntype = \"text\"\nmax_chars = 64\n";
            std::fs::write(&entry, text).unwrap();
            copy
        })
        .clone()
}

/// Applies each `(from, to)` replacement in order and panics, naming the pattern, when one
/// matches nothing: `str::replace` returns the text unchanged without a word, which would turn a
/// fixture patch into a silent no-op.
pub fn patched(text: &str, patches: &[(&str, &str)]) -> String {
    let mut text = text.to_owned();
    for (from, to) in patches {
        assert!(
            text.contains(from),
            "the fixture patch matched nothing: {from:?}"
        );
        text = text.replace(from, to);
    }
    text
}

/// The sample app with its native controllers, loaded.
pub async fn sample_harness() -> AppHarness {
    AppHarness::builder()
        .app(sample_dir())
        .controllers("sample", spot_sample_app::controllers())
        .build()
        .await
        .unwrap()
}

/// A fresh scaffolded app (with example fields) in a temp project; returns the project (keep it
/// alive) and the app directory.
pub fn scaffolded(name: &str) -> (tempfile::TempDir, PathBuf) {
    let project = tempfile::tempdir().unwrap();
    let mut spec = AppSpec::new(AppName::parse(name).unwrap());
    spec.example_fields = true;
    let tree = generate(&spec).unwrap();
    tree.write_to_disk(project.path(), &WritePolicy::new())
        .unwrap();
    let app = project.path().join("app");
    (project, app)
}

/// Copies a directory tree.
pub fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

/// A placeholder value for every required scalar field of `doctype`, from its kind.
pub fn required_values(harness: &AppHarness, doctype: &str) -> Json {
    let meta = harness.meta();
    let def = meta.doctype(doctype).unwrap();
    let mut object = serde_json::Map::new();
    for field in &def.fields {
        if !field.required || field.read_only {
            continue;
        }
        let value = match &field.kind {
            FieldKind::Bool => json!(true),
            FieldKind::Int { .. } => json!(1),
            FieldKind::Decimal { .. } => json!("1.00"),
            FieldKind::Text { .. } | FieldKind::LongText { .. } => json!("a value"),
            FieldKind::Select { values } => json!(values.first().cloned().unwrap_or_default()),
            FieldKind::Date => json!("2026-01-05"),
            FieldKind::DateTime => json!("2026-01-05T10:00:00Z"),
            _ => continue,
        };
        object.insert(field.name.to_string(), value);
    }
    Json::Object(object)
}

/// The DocType names an app directory defines.
pub fn doctypes_of(app: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(app.join("doctypes"))
        .unwrap()
        .filter_map(Result::ok)
        .filter_map(|e| {
            e.path()
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
        })
        .collect();
    names.sort();
    names
}
