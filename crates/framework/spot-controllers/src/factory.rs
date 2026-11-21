//! Loading controllers from an app manifest.
//!
//! An install names a controller by a [`ControllerRef`]; a [`ControllerFactory`] turns that
//! reference into an `Arc<dyn Controller>`. [`NativeControllers`] is the table a host builds at
//! compile time from its own Rust controllers; the WASM and TypeScript adapters (WP-EXT-10 and
//! the WASM host work) are separate factories with the same shape. Until they exist, the native
//! factory answers every `wasm:` and `ts:` reference with `controller.host_unavailable`, naming
//! the missing adapter — never panicking.
//!
//! [`ControllerRegistry::register_from_manifest`] registers an app's controller and the step
//! priorities and overrides its manifest declares. A native app's controller is looked up under
//! its app name (`native:<app>`); a script app names its controller with
//! [`ControllerRegistry::register_from_manifest_ref`].

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use spot_app_api::{AppError, AppResult, Controller, ErrorClass};
use spot_manifest::AppManifest;

use crate::codes;
use crate::registry::ControllerRegistry;
use crate::source::{ControllerRef, ControllerSource};

/// Turns a controller reference into a controller, or an error naming what is missing.
pub trait ControllerFactory: Send + Sync {
    /// The controller `reference` names.
    ///
    /// # Errors
    ///
    /// `controller.unknown_native` when a native reference names no built-in controller, and
    /// `controller.host_unavailable` when no adapter for this kind is installed.
    fn create(&self, reference: &ControllerRef) -> AppResult<Arc<dyn Controller>>;

    /// The further controllers an app ships beyond `native:<app>`, one per DocType, that an
    /// install of `app` registers with it. The default is none.
    fn companions(&self, _app: &str) -> Vec<ControllerRef> {
        Vec::new()
    }
}

/// The native controllers a host builds into itself, by name.
#[derive(Default)]
pub struct NativeControllers {
    by_name: BTreeMap<String, Arc<dyn Controller>>,
}

impl NativeControllers {
    /// An empty table.
    pub fn new() -> NativeControllers {
        NativeControllers::default()
    }

    /// The same table with `controller` under `name`.
    #[must_use]
    pub fn with(mut self, name: &str, controller: Arc<dyn Controller>) -> NativeControllers {
        self.insert(name, controller);
        self
    }

    /// Adds `controller` under `name`, replacing any earlier one.
    pub fn insert(&mut self, name: &str, controller: Arc<dyn Controller>) -> &mut Self {
        self.by_name.insert(name.to_owned(), controller);
        self
    }

    /// The table for a host's controllers, each under its DocType name.
    pub fn from_controllers(
        controllers: impl IntoIterator<Item = Arc<dyn Controller>>,
    ) -> NativeControllers {
        let mut table = NativeControllers::new();
        for controller in controllers {
            let name = controller.doctype().to_owned();
            table.insert(&name, controller);
        }
        table
    }

    /// The controller under `name`, or `None`.
    pub fn get(&self, name: &str) -> Option<Arc<dyn Controller>> {
        self.by_name.get(name).cloned()
    }

    /// How many controllers the host builds in.
    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    /// Whether the host builds in no controllers.
    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    /// The names, in order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.by_name.keys().map(String::as_str)
    }
}

impl ControllerFactory for NativeControllers {
    fn create(&self, reference: &ControllerRef) -> AppResult<Arc<dyn Controller>> {
        match reference.source() {
            ControllerSource::Native => self.get(reference.value()).ok_or_else(|| {
                AppError::new(
                    ErrorClass::NotFound,
                    codes::UNKNOWN_NATIVE,
                    format!(
                        "No native controller named `{}` is built into the host.",
                        reference.value()
                    ),
                )
            }),
            other => Err(host_unavailable(other)),
        }
    }

    /// The controllers named `<app>.<suffix>`: an app's controllers for DocTypes other than the
    /// one `native:<app>` serves.
    fn companions(&self, app: &str) -> Vec<ControllerRef> {
        let prefix = format!("{app}.");
        self.by_name
            .keys()
            .filter(|name| name.starts_with(&prefix))
            .map(|name| ControllerRef::native(name))
            .collect()
    }
}

/// The `controller.host_unavailable` error for a controller kind with no adapter.
pub(crate) fn host_unavailable(source: ControllerSource) -> AppError {
    AppError::new(
        ErrorClass::Unavailable,
        codes::HOST_UNAVAILABLE,
        format!(
            "The {} controller host adapter is not installed yet.",
            source.adapter_name()
        ),
    )
}

impl ControllerRegistry {
    /// Registers a native app's controllers and its manifest's steps and overrides.
    ///
    /// The app's main controller is looked up as `native:<app name>`; the factory may add
    /// companions (`native:<app name>.<suffix>`, see [`ControllerFactory::companions`]), one per
    /// further DocType. Script apps name their controller explicitly with
    /// [`ControllerRegistry::register_from_manifest_ref`].
    ///
    /// # Errors
    ///
    /// Whatever [`ControllerFactory::create`] returns, `controller.doctype_duplicate` when two of
    /// the app's controllers serve one DocType, and `controller.event_doctype_unserved` when a
    /// manifest event names a DocType none of them serves. The registry is not changed when it
    /// fails.
    pub fn register_from_manifest(
        &mut self,
        manifest: &AppManifest,
        factory: &dyn ControllerFactory,
    ) -> AppResult<&mut Self> {
        let app = manifest.app.name.as_str();
        let mut references = vec![ControllerRef::native(app)];
        references.extend(factory.companions(app));
        self.register_from_manifest_refs(manifest, &references, factory)
    }

    /// Like [`ControllerRegistry::register_from_manifest`], but the manifest declares which
    /// controller reference it names, for example `ts:controllers/invoice.ts`.
    ///
    /// # Errors
    ///
    /// As [`ControllerRegistry::register_from_manifest_refs`] with one reference.
    pub fn register_from_manifest_ref(
        &mut self,
        manifest: &AppManifest,
        reference: &ControllerRef,
        factory: &dyn ControllerFactory,
    ) -> AppResult<&mut Self> {
        self.register_from_manifest_refs(manifest, std::slice::from_ref(reference), factory)
    }

    /// Registers the controllers `references` name, one per DocType, with the manifest's steps
    /// and overrides. Each manifest event attaches only to the controller serving the event's
    /// DocType.
    ///
    /// # Errors
    ///
    /// Whatever [`ControllerFactory::create`] returns; `controller.doctype_duplicate` when two
    /// references serve one DocType; `controller.event_doctype_unserved` when a manifest event
    /// names a DocType no reference serves. The registry is not changed when it fails.
    pub fn register_from_manifest_refs(
        &mut self,
        manifest: &AppManifest,
        references: &[ControllerRef],
        factory: &dyn ControllerFactory,
    ) -> AppResult<&mut Self> {
        let app = manifest.app.name.as_str().to_owned();
        let mut controllers: Vec<(Arc<dyn Controller>, ControllerSource)> = Vec::new();
        for reference in references {
            controllers.push((factory.create(reference)?, reference.source()));
        }
        let mut served: BTreeSet<&str> = BTreeSet::new();
        for (controller, _) in &controllers {
            if !served.insert(controller.doctype()) {
                return Err(AppError::new(
                    ErrorClass::Internal,
                    codes::DOCTYPE_DUPLICATE,
                    format!(
                        "App `{app}` names more than one controller for DocType `{}`.",
                        controller.doctype()
                    ),
                ));
            }
        }
        for event in &manifest.events {
            if !served.contains(event.doctype.as_str()) {
                return Err(AppError::new(
                    ErrorClass::Internal,
                    codes::EVENT_DOCTYPE_UNSERVED,
                    format!(
                        "Event `{}` of app `{app}` is declared for DocType `{}`, which none of \
                         the app's controllers serves.",
                        event.step.as_str(),
                        event.doctype
                    ),
                ));
            }
        }
        self.add_manifest(manifest);
        for (controller, source) in controllers {
            self.register(&app, controller, source);
        }
        Ok(self)
    }
}
