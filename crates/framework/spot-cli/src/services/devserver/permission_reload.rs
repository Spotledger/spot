//! Permission TOML hot reload (WP-DX-02.T4): editing an app's `permissions/*.toml` takes effect on
//! the next request, without a restart. Development hosts only (the module that builds this
//! step is registered only when `[services.devserver]` is enabled and every address is loopback).
//!
//! # How a change is applied
//!
//! 1. The watcher already sees every file of the app directory; the classifier files a
//!    `permissions/*.toml` under metadata, so the metadata step receives it. [`MetaOrPermissions`]
//!    sits in that step's place and splits the batch: permission files go to
//!    [`PermissionReload`], everything else to the real metadata step, untouched.
//! 2. [`PermissionReload`] re-reads the **whole site's** merged permission model and the apps'
//!    named permissions ([`PermissionModels`]), asks the target to check them against the
//!    metadata being served ([`PermissionTarget::validate`]), and only when that passes swaps
//!    them in ([`PermissionTarget::swap`]).
//! 3. The swap replaces the model the site keeps for every later snapshot build and then rebuilds
//!    and installs the decision core in one step (the path runtime customisation uses): a request
//!    in flight keeps the snapshot it started with, the next request decides with the new one.
//!
//! # A file that does not build
//!
//! The old model keeps serving. The outcome is a failure whose errors carry the catalogued
//! `devserver.metadata` code and the loader's own sentence (the file and the problem), which the
//! dev console and `GET /dev/status` show. Nothing from a file's grants is echoed beyond what the
//! loader's sentence names.

use std::path::PathBuf;
use std::sync::Arc;

use spot_devserver::DevError;
use spot_devserver::reload::{ReloadContext, Step, StepName, StepOutcome};

use crate::config;
use crate::live::SiteLive;
use crate::permissions::AppPerms;

/// What a re-read of the permission files produced: the merged model and the apps' named
/// permissions, the two things a site's snapshot is built from.
#[derive(Clone, Debug, Default)]
pub struct PermissionBuild {
    /// The merged grants, field grants, protection groups and shares of every installed app.
    pub model: spot_permissions::Model,
    /// The installed apps' named permissions and the roles' grants of them.
    pub apps: AppPerms,
}

/// Reads the permission files of the site.
pub trait PermissionModels: Send + Sync {
    /// The merged model and named permissions as the files hold them now.
    ///
    /// # Errors
    ///
    /// Every problem the loader found, one sentence each.
    fn read(&self) -> Result<PermissionBuild, Vec<String>>;
}

/// Where a built model is checked and put in force.
pub trait PermissionTarget: Send + Sync {
    /// Checks `build` against the metadata served now, changing nothing.
    ///
    /// # Errors
    ///
    /// Every problem, one sentence each.
    fn validate(&self, build: &PermissionBuild) -> Result<(), Vec<String>>;

    /// Puts `build` in force atomically and returns the snapshot version it is served at.
    ///
    /// # Errors
    ///
    /// A sentence; the previous model keeps serving.
    fn swap(&self, build: PermissionBuild) -> Result<u64, String>;
}

/// Reads the permission files of a site from its installed apps' directories.
pub struct SiteModels {
    root: PathBuf,
    loaded: config::Loaded,
    site: config::Site,
}

impl SiteModels {
    /// A reader for `site`.
    #[must_use]
    pub fn new(root: PathBuf, loaded: config::Loaded, site: config::Site) -> Self {
        Self { root, loaded, site }
    }
}

fn sentences(error: impl std::fmt::Display) -> Vec<String> {
    error.to_string().lines().map(str::to_owned).collect()
}

impl PermissionModels for SiteModels {
    fn read(&self) -> Result<PermissionBuild, Vec<String>> {
        let model =
            crate::apps::site_model(&self.root, &self.loaded, &self.site).map_err(sentences)?;
        let apps =
            crate::services::controllers::app_permissions(&self.root, &self.loaded, &self.site)
                .map_err(sentences)?;
        Ok(PermissionBuild { model, apps })
    }
}

/// The real site: checks with the engine builder the site itself uses and swaps through
/// [`SiteLive`].
pub struct LiveTarget {
    live: Arc<SiteLive>,
    runtime: tokio::runtime::Handle,
}

impl LiveTarget {
    /// A target over `live`; `runtime` runs the swap's async part from the watcher's own
    /// (blocking) thread.
    #[must_use]
    pub fn new(live: Arc<SiteLive>, runtime: tokio::runtime::Handle) -> Self {
        Self { live, runtime }
    }
}

impl PermissionTarget for LiveTarget {
    fn validate(&self, build: &PermissionBuild) -> Result<(), Vec<String>> {
        let served = self
            .live
            .served()
            .ok_or_else(|| vec!["no runtime is attached to swap".to_owned()])?;
        // The same model a swap builds: the files' grants plus the default grants of the
        // DocTypes made at runtime.
        let mut model = build.model.clone();
        model.grants.extend(self.live.grants());
        crate::permissions::engine(&served.meta, &model, &build.apps)
            .map(|_| ())
            .map_err(sentences)
    }

    fn swap(&self, build: PermissionBuild) -> Result<u64, String> {
        let served = self
            .live
            .served()
            .ok_or_else(|| "no runtime is attached to swap".to_owned())?;
        let (Some(engine), Some(authorizer), Some(principals)) = (
            self.live.engine(),
            self.live.authorizer(),
            self.live.principals(),
        ) else {
            return Err("no runtime is attached to swap".to_owned());
        };
        let version = served.version.map_or(1, |v| v.saturating_add(1));
        // `attach` replaces the model and named permissions every later snapshot is built from
        // (it parks the pair it is given until the host commits a rebuild, and the swap below
        // clears that); `swap_to` then builds the decision core from them and installs it with
        // the engine, the authorizer and the served pair in one step.
        self.live.attach(
            Arc::clone(&served.meta),
            build.model,
            build.apps,
            Arc::clone(&served.decision),
            engine,
            authorizer,
            principals,
        );
        self.runtime
            .block_on(
                self.live
                    .swap_to(Arc::clone(&served.meta), version, self.live.grants()),
            )
            .map(|()| version)
    }
}

/// Whether `change` is a permission file: a `.toml` directly or deeply under `permissions/`.
#[must_use]
pub fn is_permission_file(relative: &camino::Utf8Path) -> bool {
    relative.extension() == Some("toml")
        && relative.components().next().is_some_and(
            |first| matches!(first, camino::Utf8Component::Normal(name) if name == "permissions"),
        )
}

/// The step that reloads permission files.
pub struct PermissionReload {
    models: Arc<dyn PermissionModels>,
    target: Arc<dyn PermissionTarget>,
}

impl PermissionReload {
    /// A step reading through `models` and swapping into `target`.
    #[must_use]
    pub fn new(models: Arc<dyn PermissionModels>, target: Arc<dyn PermissionTarget>) -> Self {
        Self { models, target }
    }

    /// Re-reads, checks and swaps. A failure leaves the old model serving.
    #[must_use]
    pub fn apply(&self) -> StepOutcome {
        let build = match self.models.read() {
            Ok(build) => build,
            Err(errors) => {
                return Self::failed(
                    "permissions unchanged; the permission files do not build",
                    errors,
                );
            }
        };
        if let Err(errors) = self.target.validate(&build) {
            return Self::failed(
                "permissions unchanged; the new permissions do not fit the served metadata",
                errors,
            );
        }
        match self.target.swap(build) {
            Ok(version) => StepOutcome::ok(format!("permissions live at version {version}"))
                .with_version(version),
            Err(why) => Self::failed(
                "permissions unchanged; the new permissions could not be put in force",
                vec![why],
            ),
        }
    }

    fn failed(detail: &str, errors: Vec<String>) -> StepOutcome {
        let code = DevError::Metadata { errors: Vec::new() }.code();
        StepOutcome::failed(
            detail,
            errors
                .into_iter()
                .map(|line| format!("{code}: {line}"))
                .collect(),
        )
    }
}

/// The metadata step's replacement: permission files to [`PermissionReload`], everything else to
/// the metadata step as it was.
pub struct MetaOrPermissions {
    meta: Arc<dyn Step>,
    permissions: PermissionReload,
}

impl MetaOrPermissions {
    /// Wraps `meta` so a batch's permission files are applied by `permissions`.
    #[must_use]
    pub fn new(meta: Arc<dyn Step>, permissions: PermissionReload) -> Self {
        Self { meta, permissions }
    }
}

/// One outcome for two steps of one batch: failed when either failed, every error kept, the
/// later version, the details joined.
fn combine(first: StepOutcome, second: StepOutcome) -> StepOutcome {
    let mut errors = first.errors;
    errors.extend(second.errors);
    StepOutcome {
        ok: first.ok && second.ok,
        detail: format!("{}; {}", first.detail, second.detail),
        errors,
        version: second.version.or(first.version),
        artifacts: first
            .artifacts
            .into_iter()
            .chain(second.artifacts)
            .collect(),
    }
}

impl Step for MetaOrPermissions {
    fn name(&self) -> StepName {
        StepName::Metadata
    }

    fn reload(&self, ctx: &ReloadContext<'_>) -> StepOutcome {
        let (permission, other): (Vec<_>, Vec<_>) = ctx
            .changes
            .iter()
            .cloned()
            .partition(|change| is_permission_file(&change.relative));
        if permission.is_empty() {
            return self.meta.reload(ctx);
        }
        let permissions = self.permissions.apply();
        if other.is_empty() {
            return permissions;
        }
        // Permissions first: the metadata swap then builds its snapshot over the new model.
        let meta = self.meta.reload(&ReloadContext {
            changes: &other,
            now: ctx.now,
        });
        combine(permissions, meta)
    }
}
