//! The reload pipeline: metadata, views, scripts and components, and the guarantee that a
//! failed change leaves the previous version serving (WP-DX-02.T1 deliverable 2).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use camino::{Utf8Path, Utf8PathBuf};
use spot_core::{SiteId, SystemClock};
use spot_devserver::classify::{Change, ChangeKind};
use spot_devserver::publish::RecordingNotifier;
use spot_devserver::reload::{
    BuildCommand, BuildOutput, BuildRunner, ComponentStep, DeclarationNameChecker, DtsProvider,
    InMemoryComponentHost, InMemoryScriptHost, LiveDefinitions, MetaStep, Reloader, ScriptStep,
    Step, ViewStep,
};
use spot_devserver::watch::Op;
use spot_meta_live::LiveMeta;

fn root() -> (tempfile::TempDir, Utf8PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let path = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
    (temp, path)
}

fn write(path: &Utf8Path, text: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, text).unwrap();
}

fn widget_toml(field_type: &str) -> String {
    format!(
        "name = \"widget\"\nlabel = \"Widget\"\nnaming = \"WID-.#####\"\n\n[[fields]]\nname = \"title\"\nlabel = \"Title\"\ntype = \"{field_type}\"\nrequired = true\n"
    )
}

fn change(relative: &str, kind: ChangeKind) -> Change {
    Change {
        path: Utf8PathBuf::from(relative),
        relative: Utf8PathBuf::from(relative),
        op: Op::Modified,
        kind,
    }
}

#[test]
fn a_good_metadata_change_mints_a_version_and_a_bad_one_leaves_the_old_serving() {
    let (_temp, root) = root();
    let meta_dir = root.join("meta");
    write(&meta_dir.join("widget.toml"), &widget_toml("text"));
    let site = SiteId::new("acme.example").unwrap();
    let meta = MetaStep::open(site, meta_dir.clone()).unwrap();
    let live: Arc<LiveMeta> = meta.live().clone();
    assert_eq!(meta.version(), 0);
    let before_hash = live.current().hash().clone();

    let reloader = Reloader::new(16).with_step(Arc::new(meta) as Arc<dyn Step>);
    let clock = SystemClock::new();
    let summary = reloader.reload(&[change("meta/widget.toml", ChangeKind::Metadata)], &clock);
    assert!(summary.all_ok(), "{summary:?}");
    assert_eq!(live.current().version().get(), 2);

    // A change that does not build: the version and the content hash do not move.
    write(
        &meta_dir.join("widget.toml"),
        &widget_toml("not_a_field_type"),
    );
    let summary = reloader.reload(&[change("meta/widget.toml", ChangeKind::Metadata)], &clock);
    assert_eq!(summary.failures(), 1);
    assert_eq!(summary.records[0].error_code(), Some("devserver.metadata"));
    assert_eq!(live.current().version().get(), 2);
    assert_eq!(live.current().hash(), &before_hash);
}

#[test]
fn a_view_change_is_validated_against_the_live_metadata() {
    let (_temp, root) = root();
    let meta_dir = root.join("meta");
    write(&meta_dir.join("widget.toml"), &widget_toml("text"));
    let meta = MetaStep::open(SiteId::new("acme.example").unwrap(), meta_dir).unwrap();
    let definitions = Arc::new(LiveDefinitions::new(meta.live().clone()));
    let views_dir = root.join("views");
    let step = ViewStep::new(views_dir.clone(), definitions);

    write(
        &views_dir.join("widget.form.toml"),
        "doctype = \"widget\"\nkind = \"form\"\n\n[[sections]]\nid = \"fields\"\n\n[[sections.items]]\ntype = \"field\"\nfield = \"title\"\n",
    );
    let outcome = step.reload(&spot_devserver::reload::ReloadContext {
        changes: &[change("views/widget.form.toml", ChangeKind::View)],
        now: std::time::Duration::ZERO,
    });
    assert!(outcome.ok, "{outcome:?}");

    write(
        &views_dir.join("widget.list.toml"),
        "doctype = \"widget\"\nkind = \"list\"\n\n[[columns]]\nfield = \"missing\"\n",
    );
    let outcome = step.reload(&spot_devserver::reload::ReloadContext {
        changes: &[change("views/widget.list.toml", ChangeKind::View)],
        now: std::time::Duration::ZERO,
    });
    assert!(!outcome.ok);
    assert!(
        outcome.errors.iter().any(|e| e.contains("missing")),
        "{outcome:?}"
    );
}

/// A declaration provider fixed by the test.
struct FixedDts(Option<String>);

impl DtsProvider for FixedDts {
    fn declarations(&self) -> Result<Option<String>, Vec<String>> {
        Ok(self.0.clone())
    }
}

#[test]
fn a_script_is_transpiled_checked_and_swapped_and_a_bad_one_is_not() {
    let (_temp, root) = root();
    let ts_dir = root.join("ts");
    let host = Arc::new(InMemoryScriptHost::new());
    let step = ScriptStep::new(
        ts_dir.clone(),
        Arc::new(FixedDts(Some(
            "export function getDoc(): unknown;\n".to_owned(),
        ))),
        Arc::new(DeclarationNameChecker),
        host.clone(),
    );
    let scalar = |relative: &str| change(relative, ChangeKind::Script);
    let key = |name: &str| ts_dir.join(name).to_string();

    write(
        &ts_dir.join("main.ts"),
        "import { getDoc } from \"spot:meta\";\nexport const value: unknown = getDoc;\n",
    );
    let main_changes = [scalar("ts/main.ts")];
    let outcome = step.reload(&spot_devserver::reload::ReloadContext {
        changes: &main_changes,
        now: std::time::Duration::ZERO,
    });
    assert!(outcome.ok, "{outcome:?}");
    assert!(host.get(&key("main.ts")).is_some());

    // The declaration does not export the imported name: the step fails and nothing is swapped.
    let strict = ScriptStep::new(
        ts_dir.clone(),
        Arc::new(FixedDts(Some(
            "export function somethingElse(): unknown;\n".to_owned(),
        ))),
        Arc::new(DeclarationNameChecker),
        host.clone(),
    );
    write(
        &ts_dir.join("other.ts"),
        "import { getDoc } from \"spot:meta\";\nexport const value = getDoc;\n",
    );
    let other_changes = [scalar("ts/other.ts")];
    let outcome = strict.reload(&spot_devserver::reload::ReloadContext {
        changes: &other_changes,
        now: std::time::Duration::ZERO,
    });
    assert!(!outcome.ok);
    assert!(host.get(&key("other.ts")).is_none());

    // A syntax error is a diagnostic and never reaches the host.
    write(&ts_dir.join("broken.ts"), "export const = ;\n");
    let broken_changes = [scalar("ts/broken.ts")];
    let outcome = step.reload(&spot_devserver::reload::ReloadContext {
        changes: &broken_changes,
        now: std::time::Duration::ZERO,
    });
    assert!(!outcome.ok);
    assert!(!outcome.errors.is_empty());
}

#[derive(Clone, Copy)]
struct FakeRunner(bool);

impl BuildRunner for FakeRunner {
    fn run(&self, _command: &BuildCommand) -> spot_devserver::DevResult<BuildOutput> {
        Ok(BuildOutput {
            ok: self.0,
            stdout_tail: "build output".to_owned(),
            stderr_tail: if self.0 {
                String::new()
            } else {
                "boom".to_owned()
            },
        })
    }
}

#[test]
fn a_component_change_runs_the_build_and_loads_the_new_artefact() {
    let (_temp, root) = root();
    let artifact = root.join("app.wasm");
    write(&artifact, "wasm-bytes");
    let host = Arc::new(InMemoryComponentHost::new());
    let step = ComponentStep::new(
        "app",
        Some(BuildCommand::new("cargo", &root)),
        Arc::new(FakeRunner(true)),
        host.clone(),
        artifact.clone(),
    );
    let ctx = spot_devserver::reload::ReloadContext {
        changes: &[change("src/lib.rs", ChangeKind::Rust)],
        now: std::time::Duration::ZERO,
    };
    let outcome = step.reload(&ctx);
    assert!(outcome.ok, "{outcome:?}");
    assert!(host.get("app").is_some());
    assert_eq!(outcome.artifacts.len(), 1);

    let failing = ComponentStep::new(
        "app",
        Some(BuildCommand::new("cargo", &root)),
        Arc::new(FakeRunner(false)),
        host.clone(),
        artifact,
    );
    let outcome = failing.reload(&ctx);
    assert!(!outcome.ok);
    assert!(outcome.errors[0].contains("devserver.build"));
}

#[test]
fn the_notifier_sees_every_recorded_step() {
    let (_temp, root) = root();
    let meta_dir = root.join("meta");
    write(&meta_dir.join("widget.toml"), &widget_toml("text"));
    let meta = MetaStep::open(SiteId::new("acme.example").unwrap(), meta_dir).unwrap();
    let notifier = Arc::new(RecordingNotifier::new());
    let reloader = Reloader::new(4)
        .with_step(Arc::new(meta) as Arc<dyn Step>)
        .with_notifier(notifier.clone());
    reloader.reload(
        &[change("meta/widget.toml", ChangeKind::Metadata)],
        &SystemClock::new(),
    );
    assert_eq!(notifier.seen().len(), 1);
    assert!(notifier.seen()[0].ok);
}

#[test]
fn a_missing_build_command_is_reported_not_guessed() {
    let (_temp, root) = root();
    let artifact = root.join("app.wasm");
    let step = ComponentStep::new(
        "app",
        None,
        Arc::new(FakeRunner(true)),
        Arc::new(InMemoryComponentHost::new()),
        artifact,
    );
    let ctx = spot_devserver::reload::ReloadContext {
        changes: &[change("app.wasm", ChangeKind::Component)],
        now: std::time::Duration::ZERO,
    };
    let outcome = step.reload(&ctx);
    assert!(!outcome.ok);
    assert!(outcome.errors[0].contains("devserver.build"));
}

/// Proves the `Reloader` only runs a step that was registered, and counts calls.
#[derive(Default)]
struct CountingStep(AtomicUsize);

impl Step for CountingStep {
    fn name(&self) -> spot_devserver::reload::StepName {
        spot_devserver::reload::StepName::Assets
    }

    fn reload(
        &self,
        _ctx: &spot_devserver::reload::ReloadContext<'_>,
    ) -> spot_devserver::reload::StepOutcome {
        self.0.fetch_add(1, Ordering::Relaxed);
        spot_devserver::reload::StepOutcome::ok("asset noted")
    }
}

#[test]
fn a_registered_step_runs_once_per_batch() {
    let counting = Arc::new(CountingStep::default());
    let reloader = Reloader::new(4).with_step(counting.clone());
    let summary = reloader.reload(
        &[change("assets/logo.svg", ChangeKind::Asset)],
        &SystemClock::new(),
    );
    assert!(summary.all_ok());
    assert_eq!(counting.0.load(Ordering::Relaxed), 1);
}
