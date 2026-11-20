//! The harness: an app directory loaded into the real document engine over the in-memory
//! backend, with declared users, a manual clock and sequential ids.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value as Json;
use spot_app_api::context::{Actor as EngineActor, DocumentService, Invocation};
use spot_app_api::controller::Controller;
use spot_app_api::document::Document;
use spot_app_api::prelude::{CommandId, Name, OffsetDateTime, RecordId, Value};
use spot_app_permissions::SnapshotAppPermissions;
use spot_core::{Clock, ManualClock, SiteId};
use spot_document::engine::{Authorizer, Engine, PermissionAuthorizer};
use spot_functions::{FunctionHandler, FunctionRegistry};
use spot_meta::EffectiveMeta;
use spot_permissions::Principal;
use spot_seams::{Registry as Seams, RegistryBuilder};
use spot_storage_fake::FakeBackend;
use spot_ts::{EngineConfig, ScriptEngine};

use crate::actor::Actor;
use crate::error::{Result, TestkitError};
use crate::functions::{self, Registered};
use crate::grants::{self, Grant};
use crate::ids::{SeqIds, TestPrincipals, id_from};
use crate::load::{LoadedApp, assemble, load_app, to_path};
use crate::script::{self, ScriptFailure};

/// The first instant of a harness's clock: 2026-01-01T00:00:00Z.
const START: i64 = 1_767_225_600;
/// User ids count from here, so they never collide with the engine's own sequence.
const USER_BASE: u64 = 1 << 40;
/// The built-in user that holds every right by name.
pub const ADMINISTRATOR: &str = "Administrator";
const ADMIN_ROLE: &str = "Tenant Administrator";

type Controllers = Vec<(String, Arc<dyn Controller>)>;

struct State {
    apps: Vec<LoadedApp>,
    controllers: Controllers,
}

/// Collects what to load; `build` does it.
pub struct HarnessBuilder {
    dirs: Vec<PathBuf>,
    controllers: Controllers,
    functions: Vec<Registered>,
}

impl HarnessBuilder {
    /// Loads the app at `dir`.
    #[must_use]
    pub fn app(mut self, dir: impl AsRef<Path>) -> Self {
        self.dirs.push(to_path(dir));
        self
    }

    /// Registers a native controller under the reference the manifest names (`native:myapp`).
    #[must_use]
    pub fn native_controller(mut self, reference: &str, controller: Arc<dyn Controller>) -> Self {
        self.controllers.push((owner_of(reference), controller));
        self
    }

    /// Registers the handler of the whitelisted function `module`/`name` that the manifest of
    /// the app named `app` declares. A handler the manifest does not declare is refused at
    /// `build`. Use [`sync_handler`](crate::sync_handler) for a handler that awaits nothing.
    #[must_use]
    pub fn function(
        mut self,
        app: &str,
        module: &str,
        name: &str,
        handler: Arc<dyn FunctionHandler>,
    ) -> Self {
        self.functions.push(Registered {
            app: app.to_owned(),
            module: module.to_owned(),
            name: name.to_owned(),
            handler,
        });
        self
    }

    /// Registers every controller of an app crate (for example `spot_sample_app::controllers()`)
    /// as belonging to the app `app`.
    #[must_use]
    pub fn controllers(mut self, app: &str, controllers: Vec<Arc<dyn Controller>>) -> Self {
        self.controllers
            .extend(controllers.into_iter().map(|c| (app.to_owned(), c)));
        self
    }

    /// Checks and loads every app, builds the engine and prepares the store.
    ///
    /// # Errors
    ///
    /// `app_testkit.app_invalid` with every problem found.
    pub async fn build(self) -> Result<AppHarness> {
        let mut apps = Vec::new();
        let mut problems = Vec::new();
        for dir in &self.dirs {
            match load_app(dir) {
                Ok(app) => apps.push(app),
                Err(error) => {
                    problems.push(error.message);
                    problems.extend(error.problems);
                }
            }
        }
        if !problems.is_empty() {
            return Err(TestkitError::invalid("the app does not load", problems));
        }
        let state = State {
            apps,
            controllers: self.controllers,
        };
        let (meta, permissions) = assemble(&state.apps, "")?;
        let seams = seams_of(&state.controllers)?;
        let principals = Arc::new(TestPrincipals::default());
        let authorizer = Arc::new(PermissionAuthorizer::new(permissions, principals.clone()));
        let clock = Arc::new(ManualClock::new(start()));
        let function_clock: Arc<dyn Clock> = clock.clone();
        let functions = functions::build(&state.apps, &self.functions, function_clock)?;
        let ids = Arc::new(SeqIds::default());
        let as_authorizer: Arc<dyn Authorizer> = authorizer.clone();
        let engine = Engine::new(
            FakeBackend::new(),
            meta.clone(),
            seams,
            clock.clone(),
            ids.clone(),
        )
        .map_err(engine_failure)?
        .with_authorizer(as_authorizer)
        .with_controllers(controller_list(&state.controllers))
        .with_app_permissions(Arc::new(SnapshotAppPermissions::new(authorizer.clone())));
        engine.prepare().await.map_err(engine_failure)?;
        let harness = AppHarness {
            engine,
            authorizer,
            principals,
            clock,
            ids,
            site: SiteId::new("app-testkit.test").map_err(|e| {
                TestkitError::invalid("the site id is invalid", vec![e.to_string()])
            })?,
            users: Mutex::new(BTreeMap::new()),
            user_seq: AtomicU64::new(0),
            commands: AtomicU64::new(0),
            state: Mutex::new(state),
            created: Mutex::new(Vec::new()),
            meta: Mutex::new(meta),
            functions,
            extra_grants: Mutex::new(BTreeMap::new()),
            script_engine: ScriptEngine::new(EngineConfig::default()),
        };
        harness
            .user(ADMINISTRATOR)
            .with_roles(&[ADMINISTRATOR])
            .await?;
        Ok(harness)
    }
}

fn start() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(START).unwrap_or(OffsetDateTime::UNIX_EPOCH)
}

fn owner_of(reference: &str) -> String {
    reference
        .strip_prefix("native:")
        .and_then(|rest| rest.split('.').next())
        .unwrap_or(reference)
        .to_owned()
}

fn controller_list(controllers: &Controllers) -> Vec<Arc<dyn Controller>> {
    controllers.iter().map(|(_, c)| Arc::clone(c)).collect()
}

fn seams_of(controllers: &Controllers) -> Result<Arc<Seams>> {
    let mut builder = RegistryBuilder::new();
    for (app, controller) in controllers {
        builder.add_controller(app, Arc::clone(controller));
    }
    builder.build().map(Arc::new).map_err(|conflicts| {
        TestkitError::invalid(
            "the controllers conflict",
            conflicts.iter().map(|c| format!("{c:?}")).collect(),
        )
    })
}

fn engine_failure(error: spot_app_api::AppError) -> TestkitError {
    TestkitError::invalid(
        "the engine could not start",
        vec![format!("{} ({:?})", error.code, error.class)],
    )
}

/// An app loaded into the real document engine over the in-memory backend. No database, no
/// server, no wall clock and no randomness: ids count up and time moves only when the test says.
pub struct AppHarness {
    pub(crate) engine: Engine<FakeBackend>,
    authorizer: Arc<PermissionAuthorizer>,
    principals: Arc<TestPrincipals>,
    clock: Arc<ManualClock>,
    pub(crate) ids: Arc<SeqIds>,
    pub(crate) site: SiteId,
    users: Mutex<BTreeMap<String, RecordId>>,
    user_seq: AtomicU64,
    pub(crate) commands: AtomicU64,
    state: Mutex<State>,
    pub(crate) created: Mutex<Vec<(String, RecordId)>>,
    meta: Mutex<Arc<EffectiveMeta>>,
    functions: FunctionRegistry,
    /// The grants TOML of each `with_permissions` user, by user name.
    extra_grants: Mutex<BTreeMap<String, String>>,
    script_engine: ScriptEngine,
}

/// A user being declared.
pub struct UserDecl<'h> {
    harness: &'h AppHarness,
    name: String,
}

impl UserDecl<'_> {
    /// Declares the user with these roles (`"Administrator"` means the platform role that holds
    /// every right by name) and, the first time, creates its platform `user` document so links
    /// to the user resolve. A user the app's permission files give no rights has none.
    ///
    /// # Errors
    ///
    /// `app_testkit.app_invalid` when the platform `user` document is refused.
    pub async fn with_roles(self, roles: &[&str]) -> Result<RecordId> {
        let mapped: Vec<&str> = roles
            .iter()
            .map(|r| if *r == ADMINISTRATOR { ADMIN_ROLE } else { *r })
            .collect();
        let existing = self
            .harness
            .users
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&self.name)
            .copied();
        let id = existing.unwrap_or_else(|| {
            id_from(USER_BASE + self.harness.user_seq.fetch_add(1, Ordering::SeqCst) + 1)
        });
        self.harness
            .principals
            .set(id, Principal::with_roles(&self.name, &mapped));
        if existing.is_none() {
            self.harness.provision(&self.name, id).await?;
            self.harness
                .users
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(self.name, id);
        }
        Ok(id)
    }
}

impl UserDecl<'_> {
    /// Declares the user with **exactly** these rights and no role: the harness makes a role for
    /// the user that holds the grants, rebuilds the permission snapshot and swaps it in, so every
    /// check after that is the engine's own. A refused call (`assert_denied`) is what a right the
    /// grants do not give looks like.
    ///
    /// Calling it again for the same user replaces the grants.
    ///
    /// # Errors
    ///
    /// `app_testkit.app_invalid` for a grant that is not well formed, or one the permission
    /// snapshot refuses (an unknown DocType or right); nothing changes when it fails.
    pub async fn with_permissions(self, grants: &[Grant]) -> Result<RecordId> {
        let text = grants::toml_for(&self.name, grants)?;
        let previous = self
            .harness
            .extra_grants
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(self.name.clone(), text);
        if let Err(error) = self.harness.reassemble() {
            // Put the previous grants back so a refused call leaves the harness as it was.
            let mut extra = self
                .harness
                .extra_grants
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            match previous {
                Some(text) => {
                    extra.insert(self.name.clone(), text);
                }
                None => {
                    extra.remove(&self.name);
                }
            }
            return Err(error);
        }
        let role = grants::role_of(&self.name);
        self.with_roles(&[role.as_str()]).await
    }
}

impl AppHarness {
    /// Starts collecting what to load.
    #[must_use]
    pub fn builder() -> HarnessBuilder {
        HarnessBuilder {
            dirs: Vec::new(),
            controllers: Vec::new(),
            functions: Vec::new(),
        }
    }

    /// Checks and loads the app at `dir` (no native controllers).
    ///
    /// # Errors
    ///
    /// `app_testkit.app_invalid` with every problem found.
    pub async fn load(dir: impl AsRef<Path>) -> Result<AppHarness> {
        Self::builder().app(dir).build().await
    }

    /// Loads a second app into the same harness, to test composition: the metadata, the
    /// permission snapshot and the controllers are rebuilt together and swapped in.
    ///
    /// # Errors
    ///
    /// `app_testkit.app_invalid`; nothing changes when it fails.
    pub async fn load_also(&self, dir: impl AsRef<Path>) -> Result<()> {
        self.load_also_with(dir, Vec::new()).await
    }

    /// As [`load_also`](Self::load_also), with the second app's native controllers.
    ///
    /// # Errors
    ///
    /// `app_testkit.app_invalid`; nothing changes when it fails.
    pub async fn load_also_with(
        &self,
        dir: impl AsRef<Path>,
        controllers: Vec<(String, Arc<dyn Controller>)>,
    ) -> Result<()> {
        let app = load_app(dir.as_ref())?;
        let (apps, all_controllers) = {
            let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            let mut apps = state.apps.clone();
            apps.push(app);
            let mut all = state.controllers.clone();
            all.extend(controllers);
            (apps, all)
        };
        let (meta, permissions) = assemble(&apps, &self.extra_grants_text())?;
        let seams = seams_of(&all_controllers)?;
        self.engine
            .set_meta(meta.clone())
            .await
            .map_err(engine_failure)?;
        self.engine.set_seams(seams);
        self.engine
            .set_controllers(controller_list(&all_controllers));
        self.authorizer.swap(permissions);
        *self.meta.lock().unwrap_or_else(|p| p.into_inner()) = meta;
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.apps = apps;
        state.controllers = all_controllers;
        Ok(())
    }

    /// Declares (or redeclares) a test user: `harness.user("alice").with_roles(&["Sales User"])`.
    #[must_use]
    pub fn user(&self, name: &str) -> UserDecl<'_> {
        UserDecl {
            harness: self,
            name: name.to_owned(),
        }
    }

    /// The actor for a declared user.
    ///
    /// # Errors
    ///
    /// `app_testkit.unknown_user` when the user was never declared.
    pub fn as_user(&self, name: &str) -> Result<Actor<'_>> {
        let id = self
            .users
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(name)
            .copied()
            .ok_or_else(|| TestkitError::unknown_user(name))?;
        Ok(Actor::new(self, name, id))
    }

    /// Sets the harness clock (the business time of every later operation).
    pub fn set_now(&self, now: OffsetDateTime) {
        self.clock.set_wall(now);
    }

    /// Moves the harness clock forward.
    pub fn advance(&self, by: Duration) {
        self.clock.advance(by);
    }

    /// The effective metadata in force.
    #[must_use]
    pub fn meta(&self) -> Arc<EffectiveMeta> {
        self.meta.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// The real engine, for a check the kit has no helper for.
    #[must_use]
    pub fn engine(&self) -> &Engine<FakeBackend> {
        &self.engine
    }

    /// Loads the records of every fixture the loaded apps' manifests declare
    /// (`[[fixtures]]`: a DocType and an app-relative JSON file holding an array of objects with
    /// the document's fields), creating them as the Administrator in manifest order. Returns how
    /// many were created.
    ///
    /// # Errors
    ///
    /// `app_testkit.app_invalid` for a file that is missing or not an array of objects, or the
    /// first refused create; `app_testkit.unsupported` for a fixture that names an import step
    /// (the kit runs plain inserts only).
    pub async fn load_fixtures(&self) -> Result<usize> {
        let apps = self
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .apps
            .clone();
        let admin = self.as_user(ADMINISTRATOR)?;
        let mut created = 0;
        for app in &apps {
            let Some(manifest) = &app.manifest else {
                continue;
            };
            for fixture in &manifest.fixtures {
                if fixture.step.is_some() {
                    return Err(TestkitError::unsupported(
                        "a fixture that names an import step (use seed_json)",
                    ));
                }
                let text =
                    script::read_inside(&app.dir, fixture.file.as_str()).ok_or_else(|| {
                        TestkitError::invalid(
                            "a fixture file cannot be read",
                            vec![fixture.file.as_str().to_owned()],
                        )
                    })?;
                let list: Json = serde_json::from_str(&text).map_err(|e| {
                    TestkitError::invalid(
                        "a fixture file is not JSON",
                        vec![format!("{}: {e}", fixture.file.as_str())],
                    )
                })?;
                let items = list.as_array().ok_or_else(|| {
                    TestkitError::invalid(
                        "a fixture file must be a JSON array",
                        vec![fixture.file.as_str().to_owned()],
                    )
                })?;
                for item in items {
                    let result = admin.create(&fixture.doctype, item).await;
                    if let Some(error) = result.error() {
                        return Err(TestkitError::invalid(
                            "a fixture record was refused",
                            vec![format!(
                                "{}: {} ({:?})",
                                fixture.doctype, error.code, error.class
                            )],
                        ));
                    }
                    created += 1;
                }
            }
        }
        Ok(created)
    }

    /// Runs the exported function `entry` of the TypeScript file `path` (for example
    /// `ts/pricing.ts`) of the loaded app whose directory is called `app`, on the real script
    /// engine with `args`. The script gets no host functions (each is refused with a catchable
    /// error), the harness clock as its time and a zero random seed.
    ///
    /// # Errors
    ///
    /// A [`ScriptFailure`] with the stable code and positions, never source text.
    pub async fn run_script(
        &self,
        app: &str,
        path: &str,
        entry: &str,
        args: &Json,
    ) -> std::result::Result<Json, ScriptFailure> {
        let dir = self
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .apps
            .iter()
            .find(|loaded| loaded.name == app)
            .map(|loaded| loaded.dir.clone());
        let Some(dir) = dir else {
            return Err(ScriptFailure {
                code: crate::error::codes::APP_INVALID.to_owned(),
                message: format!("no loaded app is called `{app}`"),
            });
        };
        let now = self.engine_now().unix_timestamp().saturating_mul(1000);
        script::run(&self.script_engine, app, &dir, path, entry, args, now).await
    }

    /// The function registry the kit dispatches `call_function` through.
    pub(crate) fn functions(&self) -> &FunctionRegistry {
        &self.functions
    }

    /// The grants of every `with_permissions` user, as one TOML text.
    fn extra_grants_text(&self) -> String {
        self.extra_grants
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Rebuilds the permission snapshot from the apps and the `with_permissions` grants and swaps
    /// it into the authorizer.
    fn reassemble(&self) -> Result<()> {
        let apps = self
            .state
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .apps
            .clone();
        let (_, permissions) = assemble(&apps, &self.extra_grants_text())?;
        self.authorizer.swap(permissions);
        Ok(())
    }

    /// Loads a JSON array of documents as the Administrator: each element is an object with a
    /// `doctype` key and the document's fields. Returns how many were created.
    ///
    /// # Errors
    ///
    /// `app_testkit.app_invalid` for a file that is not that shape, or the first refused create.
    pub async fn seed_json(&self, path: impl AsRef<Path>) -> Result<usize> {
        let text = std::fs::read_to_string(path.as_ref()).map_err(|e| {
            TestkitError::invalid("the seed file cannot be read", vec![e.to_string()])
        })?;
        let list: Json = serde_json::from_str(&text)
            .map_err(|e| TestkitError::invalid("the seed file is not JSON", vec![e.to_string()]))?;
        let items = list
            .as_array()
            .ok_or_else(|| TestkitError::invalid("the seed file must be a JSON array", vec![]))?;
        let admin = self.as_user(ADMINISTRATOR)?;
        for item in items {
            let mut object = item.clone();
            let doctype = object
                .as_object_mut()
                .and_then(|o| o.remove("doctype"))
                .and_then(|d| d.as_str().map(str::to_owned))
                .ok_or_else(|| TestkitError::invalid("a seed document has no `doctype`", vec![]))?;
            let result = admin.create(&doctype, &object).await;
            if let Some(error) = result.error() {
                return Err(TestkitError::invalid(
                    "a seed document was refused",
                    vec![format!("{doctype}: {} ({:?})", error.code, error.class)],
                ));
            }
        }
        Ok(items.len())
    }

    /// Creates the platform `user` document of a declared user, as the platform itself.
    async fn provision(&self, name: &str, id: RecordId) -> Result<()> {
        let invalid = |what: &str| {
            TestkitError::invalid("a test user could not be created", vec![what.to_owned()])
        };
        let field = |n: &str| Name::new(n).map_err(|e| invalid(&e.to_string()));
        let mut doc = Document::new(field("user")?, id);
        doc.set(field("user_name")?, Value::Text(name.to_owned()));
        doc.set(field("full_name")?, Value::Text(name.to_owned()));
        doc.set(field("user_type")?, Value::Text("internal".to_owned()));
        doc.set(field("state")?, Value::Text("active".to_owned()));
        let n = self.commands.fetch_add(1, Ordering::SeqCst) + 1;
        let inv = Invocation {
            site: self.site.clone(),
            actor: EngineActor::System {
                component: field("app_testkit")?,
            },
            command_id: CommandId::new(&format!("testkit-{n}")).ok(),
            language: None,
            now: self.engine_now(),
        };
        self.engine
            .create(&inv, doc)
            .await
            .map(|_| ())
            .map_err(|e| invalid(&format!("{} ({:?})", e.code, e.class)))
    }

    /// The harness clock's current instant (the business time of the next operation).
    pub(crate) fn engine_now(&self) -> OffsetDateTime {
        self.clock.now_utc()
    }

    /// The engine's service view (what the actors call).
    pub(crate) fn service(&self) -> &dyn DocumentService {
        &self.engine
    }
}
