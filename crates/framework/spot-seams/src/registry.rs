//! The built registry: precomputed plans, explanations, installed overrides and seams.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value as Json;
use spot_app_api::seams::SeamProvider;
use spot_app_api::{
    AppError, AppResult, BoxFuture, Controller, EffectDecl, ErrorClass, HookPoint, OverrideKind,
    Seams, StepName, StepOverride,
};

use crate::effects::EffectChange;
use crate::explain::ExplainLine;

pub(crate) type Table<T> = HashMap<String, HashMap<HookPoint, Vec<T>>>;

/// A step that runs through another step's wrapper.
#[derive(Clone)]
pub struct WrappedStep {
    /// The wrapped step.
    pub step: StepName,
    /// Its app.
    pub app: String,
    /// The controller that runs it.
    pub controller: Arc<dyn Controller>,
    /// The effects it declares (empty: none declared).
    pub effects: Vec<EffectDecl>,
}

/// One step the host runs, in order.
#[derive(Clone)]
pub struct PlannedStep {
    /// The step to run.
    pub step: StepName,
    /// The app that declared it.
    pub app: String,
    /// Its own priority.
    pub priority: i32,
    /// Its time budget.
    pub budget: Duration,
    /// The controller that runs it (`Controller::run_step`).
    pub controller: Arc<dyn Controller>,
    /// For a wrapper: the step it wraps, which it may run once through `run_wrapped`.
    pub wrapped: Option<WrappedStep>,
    /// The effects it declares (ADR-080 6.3); empty means none declared. For a wrapper or a
    /// replacement these are the wrapper's or replacement's own, never the target's.
    pub effects: Vec<EffectDecl>,
}

/// An installed override with the app that declared it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstalledOverride {
    /// The declaring app.
    pub app: String,
    /// The override.
    pub over: StepOverride,
}

/// Who answers a seam.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeamInfo {
    /// The app providing the original implementation.
    pub provider_app: String,
    /// The replacement: its app and reason, if the seam was replaced.
    pub replaced_by: Option<(String, String)>,
}

pub(crate) struct SeamEntry {
    pub(crate) app: String,
    pub(crate) provider: Arc<dyn SeamProvider>,
    pub(crate) replacement: Option<(String, Arc<dyn SeamProvider>, String)>,
}

/// A validated, immutable registry. Build one with [`RegistryBuilder`](crate::RegistryBuilder).
pub struct Registry {
    pub(crate) plans: Table<PlannedStep>,
    pub(crate) explanations: Table<ExplainLine>,
    pub(crate) overrides: Vec<InstalledOverride>,
    pub(crate) effect_changes: Vec<EffectChange>,
    pub(crate) seams: HashMap<String, BTreeMap<u32, SeamEntry>>,
}

impl Registry {
    /// The steps to run at `point` for `doctype`, in order, with every override applied.
    ///
    /// Computed once at build time. The call is a lookup and returns a borrowed slice; it never
    /// allocates. A DocType or point with no steps gives an empty slice.
    ///
    /// Save points (`before_save` and the rest) are planned apart from operation points
    /// (`before_create`): the host runs both in the ADR-51 order and calls `plan` for each.
    pub fn plan(&self, doctype: &str, point: &HookPoint) -> &[PlannedStep] {
        self.plans
            .get(doctype)
            .and_then(|m| m.get(point))
            .map_or(&[], Vec::as_slice)
    }

    /// Why each step in [`Registry::plan`] runs, and the overrides that affected it. A declared
    /// step that an override disabled appears too, with `runs == false`.
    pub fn explain(&self, doctype: &str, point: &HookPoint) -> Vec<ExplainLine> {
        self.explanations
            .get(doctype)
            .and_then(|m| m.get(point))
            .cloned()
            .unwrap_or_default()
    }

    /// Every installed override, ordered by (target, kind, by), for the admin listing.
    pub fn overrides(&self) -> &[InstalledOverride] {
        &self.overrides
    }

    /// Who answers seam `name` at `version`, or `None` if nobody provides it.
    pub fn seam_info(&self, name: &str, version: u32) -> Option<SeamInfo> {
        let e = self.seams.get(name)?.get(&version)?;
        Some(SeamInfo {
            provider_app: e.app.clone(),
            replaced_by: e
                .replacement
                .as_ref()
                .map(|(a, _, r)| (a.clone(), r.clone())),
        })
    }

    /// The major versions offered for seam `name`, sorted ascending. Empty when nobody offers
    /// the seam.
    pub fn seam_versions(&self, name: &str) -> Vec<u32> {
        self.seams
            .get(name)
            .map_or_else(Vec::new, |m| m.keys().copied().collect())
    }

    /// The admin override listing (WP-EXT-03): every override and seam installed on the site,
    /// with who declared it, what it does and why, as data a UI or CLI can print.
    ///
    /// The report does not change when apps are registered in a different order.
    pub fn override_report(&self) -> crate::report::OverrideReport {
        crate::report::collect(self)
    }
}

impl Seams for Registry {
    fn call_json<'a>(
        &'a self,
        name: &'a str,
        version: u32,
        input: Json,
    ) -> BoxFuture<'a, AppResult<Json>> {
        let provider = self
            .seams
            .get(name)
            .and_then(|m| m.get(&version))
            .map(|e| e.replacement.as_ref().map_or(&e.provider, |r| &r.1));
        match provider {
            Some(p) => p.call(input),
            None => Box::pin(async move {
                Err(AppError::new(
                    ErrorClass::MethodNotFound,
                    "method.not_found",
                    format!("no seam {name} at version {version}"),
                ))
            }),
        }
    }
}

/// Sort key of an override kind: stable across runs.
pub(crate) fn kind_rank(k: OverrideKind) -> u8 {
    match k {
        OverrideKind::Disable => 0,
        OverrideKind::Replace => 1,
        OverrideKind::Wrap => 2,
        OverrideKind::Before => 3,
        OverrideKind::After => 4,
    }
}
