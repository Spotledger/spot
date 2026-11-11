//! The controller registry: which controllers belong to which DocType, in which order, and
//! which whitelisted methods a DocType answers.
//!
//! Apps register native, WASM and TypeScript controllers alike ([`ControllerRegistry::register`]).
//! The registry keeps them in a deterministic order (app priority, then registration order),
//! serves the whitelisted-method index for dispatch, and — through [`ControllerSet`] — turns
//! their steps into [`spot_seams`] registrations so the document engine needs no
//! controller-specific code.
//!
//! # The immutable set
//!
//! Registering or unregistering produces a new, immutable [`ControllerSet`] behind an `Arc`.
//! A host swaps the set like metadata ([`LiveMeta`]): an operation that began under one set
//! finishes under it, and only the next one sees the new set. `in-flight operations keep the set
//! they began with`.
//!
//! # Doctypes with no controller
//!
//! [`ControllerSet::resolve`] returns the registered controllers of a DocType, or a single
//! [`DefaultController`](crate::DefaultController) when no app registered one — including for a
//! DocType that did not exist when the set was built.
//!
//! [`LiveMeta`]: https://example.invalid/spot-ledger/live-meta

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use spot_app_api::{AppError, AppResult, Controller, ErrorClass, MethodDecl, StepDecl};
use spot_seams::{Conflict, InstalledOverride, Registry, RegistryBuilder};

use crate::codes;
use crate::default::DefaultController;
use crate::install::{AdjustedController, adjusted_steps};
use crate::source::ControllerSource;

/// The app name a [`DefaultController`](crate::DefaultController) answers under, so the admin
/// listing can tell a framework default from an app controller.
pub const DEFAULT_APP: &str = "platform";

/// A controller as the registry serves it: the app that owns it, where it came from and its
/// order.
#[derive(Clone)]
pub struct Registered {
    /// The app that registered it.
    pub app: String,
    /// Native, WASM or TypeScript.
    pub source: ControllerSource,
    /// Lower runs first among the controllers of one DocType.
    pub priority: i32,
    /// The controller itself.
    pub controller: Arc<dyn Controller>,
}

impl Registered {
    /// The DocType it belongs to.
    pub fn doctype(&self) -> &str {
        self.controller.doctype()
    }
}

/// The right a whitelisted method needs: a read-only method needs read, a mutating one write.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Right {
    /// Read-only: the native API needs no `If-Match`.
    Read,
    /// Writes: the native API requires `If-Match`.
    Write,
}

impl Right {
    /// The right a method declaration needs.
    pub fn of(decl: &MethodDecl) -> Right {
        if decl.mutates {
            Right::Write
        } else {
            Right::Read
        }
    }

    /// The right's name, for diagnostics.
    pub fn as_str(self) -> &'static str {
        match self {
            Right::Read => "read",
            Right::Write => "write",
        }
    }
}

/// One whitelisted method of a DocType, as the registry resolves it for dispatch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MethodInfo {
    /// Its storage-style name.
    pub name: String,
    /// The app whose controller declares it.
    pub app: String,
    /// The DocType it belongs to.
    pub doctype: String,
    /// Whether it changes documents; the native API then requires `If-Match`.
    pub mutates: bool,
    /// The right it needs.
    pub right: Right,
}

/// The whitelisted methods of one DocType, by name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MethodIndex {
    by_name: BTreeMap<String, MethodInfo>,
}

impl MethodIndex {
    /// The method called `name`, or `None`.
    pub fn get(&self, name: &str) -> Option<&MethodInfo> {
        self.by_name.get(name)
    }

    /// How many methods the DocType declares.
    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    /// Whether the DocType declares no methods.
    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    /// Every method, in name order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &MethodInfo)> {
        self.by_name
            .iter()
            .map(|(name, info)| (name.as_str(), info))
    }

    /// Every method name, in order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.by_name.keys().map(String::as_str)
    }
}

/// One controller the registry holds.
#[derive(Clone)]
struct Entry {
    app: String,
    doctype: String,
    source: ControllerSource,
    priority: i32,
    order: u64,
    controller: Arc<dyn Controller>,
}

/// Per app: the manifest's step declarations by (DocType, step name).
pub(crate) type ManifestSteps = BTreeMap<String, BTreeMap<(String, String), StepDecl>>;

/// An app's declaration that one of its methods overrides another app's on the same DocType.
#[derive(Clone, Debug, PartialEq, Eq)]
struct MethodOverride {
    app: String,
    target_app: String,
    doctype: String,
    method: String,
}

/// An immutable snapshot of everything registered at one moment.
///
/// A host holds one behind an `Arc` and swaps it on install or uninstall. Reading it never takes
/// a lock and never changes; an operation that began on a set keeps that set to its end.
pub struct ControllerSet {
    /// Sorted by (doctype, priority, order).
    entries: Vec<Entry>,
    overrides: Vec<InstalledOverride>,
    method_overrides: Vec<MethodOverride>,
    /// Per app: the manifest's step declarations, applied to the controller's own steps at
    /// install (priority from the manifest, falling back to the step's own).
    manifest_steps: ManifestSteps,
}

impl ControllerSet {
    /// The empty set.
    pub fn empty() -> ControllerSet {
        ControllerSet {
            entries: Vec::new(),
            overrides: Vec::new(),
            method_overrides: Vec::new(),
            manifest_steps: BTreeMap::new(),
        }
    }

    fn build(
        mut entries: Vec<Entry>,
        mut overrides: Vec<InstalledOverride>,
        method_overrides: Vec<MethodOverride>,
        manifest_steps: ManifestSteps,
    ) -> ControllerSet {
        entries.sort_by(|a, b| {
            a.doctype
                .cmp(&b.doctype)
                .then(a.priority.cmp(&b.priority))
                .then(a.order.cmp(&b.order))
        });
        overrides.sort_by(|a, b| {
            (&a.over.target, a.app.as_str()).cmp(&(&b.over.target, b.app.as_str()))
        });
        ControllerSet {
            entries,
            overrides,
            method_overrides,
            manifest_steps,
        }
    }

    /// The controllers registered for `doctype`, in deterministic order (app priority, then
    /// registration order). Empty when no app registered one.
    pub fn controllers_for(&self, doctype: &str) -> Vec<Registered> {
        self.entries
            .iter()
            .filter(|e| e.doctype == doctype)
            .map(|e| Registered {
                app: e.app.clone(),
                source: e.source,
                priority: e.priority,
                controller: e.controller.clone(),
            })
            .collect()
    }

    /// Like [`ControllerSet::controllers_for`], but a DocType with no controller resolves to the
    /// framework's [`DefaultController`](crate::DefaultController). A DocType added after the set
    /// was built resolves to it too.
    pub fn resolve(&self, doctype: &str) -> Vec<Registered> {
        let found = self.controllers_for(doctype);
        if found.is_empty() {
            vec![Registered {
                app: DEFAULT_APP.to_owned(),
                source: ControllerSource::Native,
                priority: 0,
                controller: Arc::new(DefaultController::new(doctype)),
            }]
        } else {
            found
        }
    }

    /// The first controller [`ControllerSet::resolve`] returns: an app's, or the default.
    ///
    /// # Panics
    ///
    /// Never: `resolve` always returns at least the default controller.
    pub fn controller_for(&self, doctype: &str) -> Registered {
        self.resolve(doctype)
            .into_iter()
            .next()
            .unwrap_or_else(|| Registered {
                app: DEFAULT_APP.to_owned(),
                source: ControllerSource::Native,
                priority: 0,
                controller: Arc::new(DefaultController::new(doctype)),
            })
    }

    /// The whitelisted methods of `doctype`, by name, with the declaring app and required right.
    ///
    /// # Errors
    ///
    /// `controller.method_conflict` when two apps declare the same method name on one DocType and
    /// neither is declared an override of the other.
    pub fn methods(&self, doctype: &str) -> AppResult<MethodIndex> {
        let mut by_name: BTreeMap<String, Vec<(String, MethodDecl)>> = BTreeMap::new();
        for entry in self.entries.iter().filter(|e| e.doctype == doctype) {
            for decl in entry.controller.methods() {
                by_name
                    .entry(decl.name.as_str().to_owned())
                    .or_default()
                    .push((entry.app.clone(), decl));
            }
        }
        let mut index = MethodIndex::default();
        for (name, declarations) in by_name {
            let winner = self.resolve_method(doctype, &name, &declarations)?;
            index.by_name.insert(
                name.clone(),
                MethodInfo {
                    name,
                    app: winner.app,
                    doctype: doctype.to_owned(),
                    mutates: winner.decl.mutates,
                    right: Right::of(&winner.decl),
                },
            );
        }
        Ok(index)
    }

    /// The declaration a method name resolves to, or `controller.method_conflict`.
    fn resolve_method(
        &self,
        doctype: &str,
        name: &str,
        declarations: &[(String, MethodDecl)],
    ) -> AppResult<Winner> {
        let apps: BTreeSet<&str> = declarations.iter().map(|(a, _)| a.as_str()).collect();
        if apps.len() == 1 {
            let Some((app, decl)) = declarations.first() else {
                return Err(self.method_conflict(doctype, name, &apps));
            };
            if declarations
                .iter()
                .any(|(other, d)| other != app || d.mutates != decl.mutates)
            {
                return Err(self.method_conflict(doctype, name, &apps));
            }
            return Ok(Winner {
                app: app.clone(),
                decl: decl.clone(),
            });
        }
        let overriders: Vec<&str> = apps
            .iter()
            .copied()
            .filter(|candidate| {
                apps.iter().all(|other| {
                    candidate == other || self.is_override(candidate, other, doctype, name)
                })
            })
            .collect();
        if let [winner] = overriders.as_slice()
            && let Some((_, decl)) = declarations.iter().find(|(a, _)| a == winner)
        {
            return Ok(Winner {
                app: (*winner).to_owned(),
                decl: decl.clone(),
            });
        }
        Err(self.method_conflict(doctype, name, &apps))
    }

    /// The `controller.method_conflict` error for a name several apps declare.
    fn method_conflict(&self, doctype: &str, name: &str, apps: &BTreeSet<&str>) -> AppError {
        let apps: Vec<&str> = apps.iter().copied().collect();
        AppError::new(
            ErrorClass::Internal,
            codes::METHOD_CONFLICT,
            format!(
                "Apps {} declare method `{name}` on DocType `{doctype}`; one must be declared an \
                 override of the other.",
                apps.iter()
                    .map(|a| format!("`{a}`"))
                    .collect::<Vec<_>>()
                    .join(" and ")
            ),
        )
    }

    /// Whether `app` declares that its `method` on `doctype` overrides `target_app`'s.
    fn is_override(&self, app: &str, target_app: &str, doctype: &str, method: &str) -> bool {
        self.method_overrides.iter().any(|o| {
            o.app == app && o.target_app == target_app && o.doctype == doctype && o.method == method
        })
    }

    /// Every app that registered at least one controller, sorted and without repeats.
    pub fn apps(&self) -> Vec<String> {
        let apps: BTreeSet<&str> = self.entries.iter().map(|e| e.app.as_str()).collect();
        apps.into_iter().map(str::to_owned).collect()
    }

    /// Registers every controller's steps with `seams`, so the engine can run them.
    ///
    /// A step's priority and budget come from the app manifest when the manifest names the step,
    /// falling back to the step's own declaration. Overrides are installed too. Conflicts are
    /// not detected here; [`spot_seams::RegistryBuilder::build`] reports **all** of them at once
    /// when the host builds the registry, or use [`ControllerSet::install`].
    pub fn install_into(&self, seams: &mut RegistryBuilder) {
        for entry in &self.entries {
            let steps = adjusted_steps(&entry.controller, self.manifest_steps.get(&entry.app));
            let controller: Arc<dyn Controller> =
                Arc::new(AdjustedController::new(entry.controller.clone(), steps));
            seams.add_controller(&entry.app, controller);
        }
        for over in &self.overrides {
            seams.add_override(&over.app, over.over.clone());
        }
    }

    /// A fresh [`Registry`] holding only this set's controllers and overrides.
    ///
    /// # Errors
    ///
    /// Every [`Conflict`] `spot-seams` finds, at once.
    ///
    /// # Leniency
    ///
    /// This builds with the lenient `RegistryBuilder::new()`, which accepts any well-formed
    /// effect class: an unknown class is **not** refused (no `effects.unknown_class`). That is
    /// deliberate. Its callers are tests and tools that build a set without the platform class
    /// list. A platform host must not use it: build with `RegistryBuilder::for_platform()` (the
    /// strict platform builder) and call [`ControllerSet::install_into`] instead, so a misspelt
    /// effect class fails closed.
    pub fn install(&self) -> Result<Arc<Registry>, Vec<Conflict>> {
        let mut builder = RegistryBuilder::new();
        self.install_into(&mut builder);
        builder.build().map(Arc::new)
    }
}

/// The declaration a method name resolved to.
struct Winner {
    app: String,
    decl: MethodDecl,
}

/// The mutable registry: apps register controllers into it, and every change produces a new
/// immutable [`ControllerSet`].
pub struct ControllerRegistry {
    entries: Vec<Entry>,
    overrides: Vec<InstalledOverride>,
    method_overrides: Vec<MethodOverride>,
    manifest_steps: ManifestSteps,
    order: u64,
    set: Arc<ControllerSet>,
}

impl Default for ControllerRegistry {
    fn default() -> Self {
        ControllerRegistry::new()
    }
}

impl ControllerRegistry {
    /// An empty registry.
    pub fn new() -> ControllerRegistry {
        ControllerRegistry {
            entries: Vec::new(),
            overrides: Vec::new(),
            method_overrides: Vec::new(),
            manifest_steps: BTreeMap::new(),
            order: 0,
            set: Arc::new(ControllerSet::empty()),
        }
    }

    /// Registers `controller` for `app`, at the default priority (0).
    pub fn register(
        &mut self,
        app: &str,
        controller: Arc<dyn Controller>,
        source: ControllerSource,
    ) -> &mut Self {
        self.register_prioritized(app, controller, source, 0)
    }

    /// Registers `controller` for `app` at an explicit priority: among the controllers of one
    /// DocType, lower priority first, then registration order.
    pub fn register_prioritized(
        &mut self,
        app: &str,
        controller: Arc<dyn Controller>,
        source: ControllerSource,
        priority: i32,
    ) -> &mut Self {
        let doctype = controller.doctype().to_owned();
        self.entries.push(Entry {
            app: app.to_owned(),
            doctype,
            source,
            priority,
            order: self.order,
            controller,
        });
        self.order += 1;
        self.rebuild();
        self
    }

    /// Forgets every controller and override `app` registered. Returns whether anything was
    /// removed, so a host can tell an uninstall from a no-op.
    pub fn unregister_app(&mut self, app: &str) -> bool {
        let before = self.entries.len() + self.overrides.len() + self.method_overrides.len();
        self.entries.retain(|e| e.app != app);
        self.overrides.retain(|o| o.app != app);
        self.method_overrides
            .retain(|o| o.app != app && o.target_app != app);
        self.manifest_steps.remove(app);
        let after = self.entries.len() + self.overrides.len() + self.method_overrides.len();
        if before != after {
            self.rebuild();
        }
        before != after
    }

    /// Declares that `app`'s `method` on `doctype` overrides `target_app`'s, so two declarations
    /// of the name resolve instead of conflicting.
    pub fn declare_method_override(
        &mut self,
        app: &str,
        target_app: &str,
        doctype: &str,
        method: &str,
    ) -> &mut Self {
        self.method_overrides.push(MethodOverride {
            app: app.to_owned(),
            target_app: target_app.to_owned(),
            doctype: doctype.to_owned(),
            method: method.to_owned(),
        });
        self.rebuild();
        self
    }

    /// Installs an override declared by `app` directly (the manifest path does this too).
    pub fn add_override(&mut self, app: &str, over: spot_app_api::StepOverride) -> &mut Self {
        self.overrides.push(InstalledOverride {
            app: app.to_owned(),
            over,
        });
        self.rebuild();
        self
    }

    /// The current immutable set, to hand to the engine.
    pub fn set(&self) -> Arc<ControllerSet> {
        self.set.clone()
    }

    /// See [`ControllerSet::controllers_for`].
    pub fn controllers_for(&self, doctype: &str) -> Vec<Registered> {
        self.set.controllers_for(doctype)
    }

    /// See [`ControllerSet::resolve`].
    pub fn resolve(&self, doctype: &str) -> Vec<Registered> {
        self.set.resolve(doctype)
    }

    /// See [`ControllerSet::controller_for`].
    pub fn controller_for(&self, doctype: &str) -> Registered {
        self.set.controller_for(doctype)
    }

    /// See [`ControllerSet::methods`].
    pub fn methods(&self, doctype: &str) -> AppResult<MethodIndex> {
        self.set.methods(doctype)
    }

    /// See [`ControllerSet::install_into`].
    pub fn install_into(&self, seams: &mut RegistryBuilder) {
        self.set.install_into(seams);
    }

    /// See [`ControllerSet::install`].
    pub fn install(&self) -> Result<Arc<Registry>, Vec<Conflict>> {
        self.set.install()
    }

    /// Replaces the snapshot after a change.
    fn rebuild(&mut self) {
        self.set = Arc::new(ControllerSet::build(
            self.entries.clone(),
            self.overrides.clone(),
            self.method_overrides.clone(),
            self.manifest_steps.clone(),
        ));
    }

    /// Registers an app manifest's step declarations and overrides. A step declaration is
    /// recorded under the DocType its event names, so it only ever adjusts the controller that
    /// serves that DocType.
    pub(crate) fn add_manifest(&mut self, manifest: &spot_manifest::AppManifest) {
        let app = manifest.app.name.as_str().to_owned();
        let mut steps: BTreeMap<(String, String), StepDecl> = BTreeMap::new();
        for (doctype, decl) in manifest.step_decls() {
            steps.insert((doctype, decl.name.as_str().to_owned()), decl);
        }
        if !steps.is_empty() {
            self.manifest_steps.insert(app.clone(), steps);
        }
        for over in manifest.overrides() {
            self.overrides.push(InstalledOverride {
                app: app.clone(),
                over,
            });
        }
    }
}
