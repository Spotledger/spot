//! Permission TOML hot reload (WP-DX-02.T4): the step over fake models and a fake target, so no
//! database is needed. The real target is exercised by the end-to-end cases of the bundle.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use camino::Utf8PathBuf;
use spot_cli::services::devserver::{
    MetaOrPermissions, PermissionBuild, PermissionModels, PermissionReload, PermissionTarget,
    is_permission_file,
};
use spot_devserver::classify::{Change, ChangeKind};
use spot_devserver::reload::{ReloadContext, Step, StepName, StepOutcome};
use spot_devserver::watch::Op;

/// Models whose read answers what the test set, and counts the reads.
struct Models {
    answer: Mutex<Result<PermissionBuild, Vec<String>>>,
    reads: Mutex<u32>,
}

impl Models {
    fn new(answer: Result<PermissionBuild, Vec<String>>) -> Arc<Self> {
        Arc::new(Self {
            answer: Mutex::new(answer),
            reads: Mutex::new(0),
        })
    }

    fn reads(&self) -> u32 {
        *self.reads.lock().unwrap()
    }
}

impl PermissionModels for Models {
    fn read(&self) -> Result<PermissionBuild, Vec<String>> {
        *self.reads.lock().unwrap() += 1;
        self.answer.lock().unwrap().clone()
    }
}

/// A target that holds the "serving" version number, refuses what the test says, and records
/// every swap.
struct Target {
    validate: Mutex<Result<(), Vec<String>>>,
    swaps: Mutex<Vec<u64>>,
    serving: Mutex<u64>,
}

impl Target {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            validate: Mutex::new(Ok(())),
            swaps: Mutex::new(Vec::new()),
            serving: Mutex::new(1),
        })
    }

    fn serving(&self) -> u64 {
        *self.serving.lock().unwrap()
    }
}

impl PermissionTarget for Target {
    fn validate(&self, _build: &PermissionBuild) -> Result<(), Vec<String>> {
        self.validate.lock().unwrap().clone()
    }

    fn swap(&self, _build: PermissionBuild) -> Result<u64, String> {
        let mut serving = self.serving.lock().unwrap();
        *serving += 1;
        self.swaps.lock().unwrap().push(*serving);
        Ok(*serving)
    }
}

/// The metadata step the wrapper delegates to.
struct Meta {
    seen: Mutex<Vec<String>>,
}

impl Step for Meta {
    fn name(&self) -> StepName {
        StepName::Metadata
    }

    fn reload(&self, ctx: &ReloadContext<'_>) -> StepOutcome {
        self.seen
            .lock()
            .unwrap()
            .extend(ctx.changes.iter().map(|c| c.relative.to_string()));
        StepOutcome::ok("meta ok").with_version(40)
    }
}

fn change(relative: &str) -> Change {
    Change {
        path: Utf8PathBuf::from(format!("/app/{relative}")),
        relative: Utf8PathBuf::from(relative),
        op: Op::Modified,
        kind: ChangeKind::Metadata,
    }
}

fn run(step: &MetaOrPermissions, relatives: &[&str]) -> StepOutcome {
    let changes: Vec<Change> = relatives.iter().map(|r| change(r)).collect();
    step.reload(&ReloadContext {
        changes: &changes,
        now: Duration::ZERO,
    })
}

struct Rig {
    step: MetaOrPermissions,
    models: Arc<Models>,
    target: Arc<Target>,
    meta: Arc<Meta>,
}

fn rig(answer: Result<PermissionBuild, Vec<String>>) -> Rig {
    let models = Models::new(answer);
    let target = Target::new();
    let meta = Arc::new(Meta {
        seen: Mutex::new(Vec::new()),
    });
    let step = MetaOrPermissions::new(
        Arc::clone(&meta) as Arc<dyn Step>,
        PermissionReload::new(
            Arc::clone(&models) as Arc<dyn PermissionModels>,
            Arc::clone(&target) as Arc<dyn PermissionTarget>,
        ),
    );
    Rig {
        step,
        models,
        target,
        meta,
    }
}

#[test]
fn the_permission_directory_is_recognised() {
    for yes in [
        "permissions/grants.toml",
        "permissions/app_permissions.toml",
        "permissions/a/b.toml",
    ] {
        assert!(is_permission_file(camino::Utf8Path::new(yes)), "{yes}");
    }
    for no in [
        "doctypes/permissions.toml",
        "doctypes/widget.toml",
        "permissions/readme.md",
        "permissions",
        "fixtures/permissions/x.toml",
    ] {
        assert!(!is_permission_file(camino::Utf8Path::new(no)), "{no}");
    }
}

#[test]
fn an_edited_permission_file_is_swapped_in_for_the_next_request() {
    let rig = rig(Ok(PermissionBuild::default()));
    assert_eq!(rig.target.serving(), 1);
    let outcome = run(&rig.step, &["permissions/grants.toml"]);
    assert!(outcome.ok, "{outcome:?}");
    assert_eq!(outcome.version, Some(2));
    assert_eq!(rig.target.serving(), 2);
    assert_eq!(rig.models.reads(), 1);
    // The metadata step was not asked to rebuild anything.
    assert!(rig.meta.seen.lock().unwrap().is_empty());
}

#[test]
fn an_invalid_file_keeps_the_old_model_and_reports_the_catalogued_error() {
    let rig = rig(Err(vec![
        "permissions/grants.toml: unknown right `wirte`".to_owned(),
    ]));
    let outcome = run(&rig.step, &["permissions/grants.toml"]);
    assert!(!outcome.ok);
    assert_eq!(rig.target.serving(), 1, "nothing was swapped");
    assert!(rig.target.swaps.lock().unwrap().is_empty());
    assert_eq!(outcome.errors.len(), 1);
    assert!(
        outcome.errors[0].starts_with("devserver.metadata: "),
        "{:?}",
        outcome.errors
    );
    assert!(outcome.errors[0].contains("unknown right"));
}

#[test]
fn a_model_that_does_not_fit_the_metadata_is_not_swapped() {
    let rig = rig(Ok(PermissionBuild::default()));
    *rig.target.validate.lock().unwrap() = Err(vec![
        "grant names a DocType the metadata does not have".to_owned(),
    ]);
    let outcome = run(&rig.step, &["permissions/grants.toml"]);
    assert!(!outcome.ok);
    assert!(outcome.detail.contains("unchanged"));
    assert_eq!(rig.target.serving(), 1);
}

#[test]
fn a_fixed_file_applies_on_the_next_change() {
    let rig = rig(Err(vec!["bad".to_owned()]));
    assert!(!run(&rig.step, &["permissions/grants.toml"]).ok);
    *rig.models.answer.lock().unwrap() = Ok(PermissionBuild::default());
    let outcome = run(&rig.step, &["permissions/grants.toml"]);
    assert!(outcome.ok);
    assert_eq!(rig.target.serving(), 2);
}

#[test]
fn a_batch_without_permission_files_is_the_metadata_step_alone() {
    let rig = rig(Ok(PermissionBuild::default()));
    let outcome = run(&rig.step, &["doctypes/widget.toml"]);
    assert!(outcome.ok);
    assert_eq!(outcome.detail, "meta ok");
    assert_eq!(outcome.version, Some(40));
    assert_eq!(rig.models.reads(), 0, "the permission files are not read");
    assert_eq!(rig.target.serving(), 1, "non-dev behaviour is unchanged");
}

#[test]
fn a_mixed_batch_applies_permissions_then_metadata() {
    let rig = rig(Ok(PermissionBuild::default()));
    let outcome = run(
        &rig.step,
        &["doctypes/widget.toml", "permissions/grants.toml"],
    );
    assert!(outcome.ok);
    assert_eq!(rig.target.serving(), 2);
    assert_eq!(
        *rig.meta.seen.lock().unwrap(),
        vec!["doctypes/widget.toml".to_owned()]
    );
    assert_eq!(outcome.version, Some(40), "the later version wins");
    assert!(outcome.detail.contains("permissions live") && outcome.detail.contains("meta ok"));
}

#[test]
fn a_failed_permission_file_still_lets_the_metadata_apply_but_the_batch_fails() {
    let rig = rig(Err(vec!["bad".to_owned()]));
    let outcome = run(
        &rig.step,
        &["doctypes/widget.toml", "permissions/grants.toml"],
    );
    assert!(!outcome.ok);
    assert_eq!(rig.meta.seen.lock().unwrap().len(), 1);
    assert_eq!(outcome.errors.len(), 1);
}

#[test]
fn the_wrapper_stands_in_for_the_metadata_step() {
    let rig = rig(Ok(PermissionBuild::default()));
    assert_eq!(rig.step.name(), StepName::Metadata);
}
