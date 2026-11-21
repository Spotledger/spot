//! Server Scripts and TypeScript controllers, hosted (WP-EXT-06.T4; ADR-054, ADR-51).
//!
//! An administrator writes a short TypeScript function in a `Server Script` document and it runs
//! when a document is validated, saved, submitted or cancelled; an app ships
//! `controllers/<doctype>.ts` as a full controller. Both are **ordinary steps of the lifecycle
//! pipeline**: the engine cannot tell them from a native controller. This module is the host side
//! of `spot-server-scripts` (EXT-06.T1) and `spot-ts-controllers` (EXT-10.T1): the libraries are
//! the machinery, this file installs them in the site.
//!
//! # The shape
//!
//! - [`ScriptHub`]: one controller per scriptable DocType with **nine fixed steps**, one per
//!   document event (`server_script:run_<doctype>_<event>`). A step loads the DocType's enabled scripts
//!   from the [`Runtime`]'s cache and runs the ones of its event, in `(priority, name)` order,
//!   through `spot_server_scripts::ScriptController`. Because the steps are fixed, a script that
//!   is created, edited, enabled or disabled **takes effect on the next request without a
//!   restart and without re-registering anything**: the hub reads the current scripts, the engine
//!   keeps the same seam registry.
//! - [`ScriptAdmin`]: the controller of the `server_script` DocType. It compiles on save
//!   (`ServerScriptController`: a source that does not compile cannot be saved and the
//!   diagnostics come back as field errors with line and column), refuses a script that targets a
//!   reserved DocType, and, once a save, change or delete commits, bumps the cache generation so
//!   every hub reloads.
//! - [`EngineScriptDocuments`]: the `ScriptDocuments` adapter that reads `server_script` documents
//!   through the live engine as the platform itself (`Actor::System { component: "scripts" }`) and
//!   writes back only run-time fields (`last_error`, and `enabled` / `disabled_reason` when a
//!   script keeps failing).
//! - [`AuthorizerView`] and [`EngineMethodCaller`]: the document view a script receives is the
//!   one the site's authorizer allows the **triggering user** to read (hidden fields gone, masked
//!   fields masked), and `host.call` reaches whitelisted methods as that user. Nothing here ever
//!   hands a script the system principal.
//! - [`DirModules`] and [`typescript_factory`]: an app's `[app] controller = "ts:controllers/x.ts"`
//!   is built by `TsControllerFactory` from the installed app's directory and registered exactly
//!   like a native controller; its whitelisted methods appear in `document_method_call`.
//!
//! # Order of steps
//!
//! At one hook point the engine runs steps by ascending priority, then name. Native steps keep
//! the priorities their apps declare (platform steps use multiples of 100). A Server Script step
//! has the priority `1_000_000 + event base + script priority`, so **a script runs after every
//! native step of the same hook point** (a native step with a priority of 1,000,000 or more is a
//! bug, see [`SCRIPT_PRIORITY_OFFSET`]). Within the scripts: the event's base priority first
//! (`before_validate` before `before_save`, which share a hook point), then the script's own
//! `priority` (0 to 9,999), then its name. A TypeScript controller is a controller of its app and
//! orders like one.
//!
//! # Limits
//!
//! `[services.scripts]` in `common.toml` holds the **operator ceilings**: the default and the
//! maximum timeout, the memory, instruction, host-call and output budgets, the number of scripts
//! one event may run and the failure threshold. A script asks for a timeout in its own `timeout_ms`
//! and is clamped to the maximum, never raised; no field of a document can change a ceiling. A
//! script that exceeds a limit is cut off with `serverscript.limit_exceeded` naming the script,
//! never its source; the operation fails and nothing is saved. Script logs (`host.log`) and run
//! records are kept in memory and in `tracing`, always through `spot-redact`.
//!
//! # Level
//!
//! Optional. A site with no `[services.scripts]` table runs with the defaults (the module is on,
//! with no scripts it does nothing). `enabled = false` turns every script off and says so in the
//! log. While the module has not finished assembling the site, a scriptable DocType refuses
//! writes (fail closed, `serverscript.store_failed`), because a validation script that has not
//! loaded must never be skipped; when assembly finishes the state is `Ready` or `Off`.
//!
//! # Security
//!
//! Server scripts are customer code inside the server process: the host surface is the closed
//! list of `spot_server_scripts::host_surface` (no network, files, environment, `eval`, `fetch` or
//! timers), resources are bounded, and every script runs as the triggering user through the
//! step's `HookContext`. No error, log line or audit record carries source text, a field value or
//! a secret: a failure names the script and a stable code, a compile diagnostic carries line and
//! column. Creating, editing and disabling a script needs the administrator right on `server_script`
//! (no role holds it by default).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::Value as Json;
use spot_app_api::context::{Actor, DocumentService, Invocation};
use spot_app_api::document::{Document, DocumentPatch};
use spot_app_api::error::{ErrorClass, FieldPath};
use spot_app_api::lifecycle::{HookPoint, Phase, Target};
use spot_app_api::meta::DocTypeDef;
use spot_app_api::prelude::{Name, RecordId, Value, Version};
use spot_app_api::{
    AppError, AppResult, BoxFuture, Controller, HookContext, Operation, StepDecl, StepName,
};
use spot_controllers::{ControllerRegistry, ControllerSource};
use spot_core::{Clock, SiteId};
use spot_document::engine::{Authorizer, Engine};
use spot_http::api::service::{Door, DoorTier};
use spot_postgres::PgBackend;
use spot_server_scripts::{
    CompileOutcome, FieldView, MethodCaller, SERVER_SCRIPT_DOCTYPE, ScriptController,
    ScriptDocuments, ScriptEvent, ScriptLimits, ScriptLog, ScriptRun, ScriptStore, ServerScript,
    ServerScriptController, host_surface,
};
use spot_storage_contract::{
    Catalog, Expr, Isolation, Query, QueryExecutor, QueryPath, Selection, StorageBackend,
    TextMatch, TextMode, Transaction, TxOptions,
};
use spot_ts::{EngineConfig, ScriptEngine};
use spot_ts_controllers::{ModuleSource, TsControllerFactory, TsLimits, ts_host_surface};
use tokio::sync::watch;

use crate::live::SiteLive;
use crate::services::realtime::{OfflineSlot, door_slot, finalise_door, refusal_error};
use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

#[path = "scripts/elevation.rs"]
pub mod elevation;
#[path = "scripts/engine.rs"]
mod engine;
#[path = "scripts/hub.rs"]
mod hub;
#[path = "scripts/runtime.rs"]
mod runtime;
#[path = "scripts/settings.rs"]
mod settings;
#[path = "scripts/ts.rs"]
mod ts;

pub use elevation::{
    ChainElevationAudit, ElevationEntry, ElevationLog, ElevationStep, PermissionCheck,
    SlotPermissions,
};
pub use engine::{
    AuthorizerView, EngineAccess, EngineMethodCaller, EngineScriptDocuments, LivePermissionCheck,
};
pub use hub::{ScriptAdmin, ScriptHub};
pub use runtime::{Runtime, ScriptSlot, SlotState, slot};
pub use settings::Settings;
pub use ts::{DirModules, SlotView, register_admin, register_controllers, typescript_factory};
// Helpers the sibling files share (visible to this module's tree only).
use hub::*;

/// Start order: after the outbound webhooks (1060); nothing else depends on this module.
pub const ORDER: u32 = 1070;

/// The component name the module acts under when it reads or writes documents as the platform.
pub const COMPONENT: &str = "scripts";

/// The app name the module's controllers register under (the app part of every step name).
pub const APP: &str = "server_script";

/// A Server Script step's priority is this offset plus the event's base priority plus the script's
/// own, so a script runs after every native step at the same hook point.
pub const SCRIPT_PRIORITY_OFFSET: i32 = 1_000_000;

/// The code scripts answer with while the module has not finished loading.
const NOT_READY: &str = spot_server_scripts::STORE_FAILED;

/// The most `server_script` rows one DocType may hold; a DocType with more is refused.
const MAX_SCRIPTS_PER_DOCTYPE: u32 = 256;

/// The DocTypes no script may be attached to: the platform's credential, session, authorisation
/// and webhook records, and the three script records themselves (`server_script`, `api_script`,
/// `scheduled_script`: a script that rewrites scripts is a privilege escalation, and the
/// elevation flag lives on them). Everything else, including every app DocType, is scriptable.
pub const RESERVED: [&str; 16] = [
    "api_key",
    "api_key_scope",
    "api_script",
    "linked_identity",
    "login_attempt",
    "role",
    "role_assignment",
    "role_assignment_scope",
    "role_duty",
    "scheduled_script",
    "server_script",
    "session",
    "user",
    "user_recovery_digest",
    "webhook",
    "webhook_delivery",
];

/// Whether a script may be attached to `doctype`.
#[must_use]
pub fn is_scriptable(doctype: &str) -> bool {
    !RESERVED.contains(&doctype)
}

/// The document view a script receives, decided offline first: a script runs as the triggering
/// user, so a user the node no longer admits reads nothing through it (WP-IAM-09.T5).
struct OfflineView {
    inner: Arc<dyn FieldView>,
    offline: Arc<OfflineSlot>,
}

impl FieldView for OfflineView {
    fn readable<'a>(
        &'a self,
        inv: &'a Invocation,
        def: &'a DocTypeDef,
        doc: &'a Document,
    ) -> BoxFuture<'a, AppResult<Document>> {
        Box::pin(async move {
            self.offline
                .admit_actor(Door::Scripts, &inv.actor, DoorTier::Three, None)
                .map_err(refusal_error)?;
            self.inner.readable(inv, def, doc).await
        })
    }
}

/// The method caller a script receives (`host.call`), decided offline first, with the same rule as
/// [`OfflineView`].
struct OfflineCaller {
    inner: Arc<dyn MethodCaller>,
    offline: Arc<OfflineSlot>,
}

impl MethodCaller for OfflineCaller {
    fn call<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        method: &'a str,
        args: Json,
    ) -> BoxFuture<'a, AppResult<Json>> {
        Box::pin(async move {
            self.offline
                .admit_actor(Door::Scripts, &inv.actor, DoorTier::Three, None)
                .map_err(refusal_error)?;
            self.inner.call(inv, doctype, id, method, args).await
        })
    }
}

/// Wraps a script runtime's two user-facing seams with the scripts door's slot.
fn offline_seams(
    view: Arc<dyn FieldView>,
    caller: Arc<dyn MethodCaller>,
    offline: &Arc<OfflineSlot>,
) -> (Arc<dyn FieldView>, Arc<dyn MethodCaller>) {
    (
        Arc::new(OfflineView {
            inner: view,
            offline: Arc::clone(offline),
        }),
        Arc::new(OfflineCaller {
            inner: caller,
            offline: Arc::clone(offline),
        }),
    )
}

/// The module's service: it holds the readiness answer and nothing else to run (the hubs work in
/// the document pipeline, not in a loop of their own).
struct ScriptsService {
    slot: Arc<ScriptSlot>,
    problem: Option<String>,
}

impl Service for ScriptsService {
    fn name(&self) -> &'static str {
        "scripts"
    }

    fn level(&self) -> Level {
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            if let Some(problem) = &self.problem {
                return Err(problem.clone());
            }
            match self.slot.state() {
                SlotState::Ready(runtime) => runtime
                    .store
                    .enabled_for(SERVER_SCRIPT_DOCTYPE)
                    .await
                    .map(|_| ())
                    .map_err(|e| format!("the Server Script store is unusable: {}", e.code)),
                _ => Err("Server Scripts are not loaded".to_owned()),
            }
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            while !*stop.borrow() {
                if stop.changed().await.is_err() {
                    break;
                }
            }
            Ok(())
        })
    }
}

/// Joins the site: builds the runtime over the live engine and fills the slot the controllers
/// hold.
///
/// # Errors
///
/// A setting out of range. The error names the key, never a value.
pub async fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("scripts")?;
    if let Some(key) = settings.invalid_key() {
        return Err(Error(format!("services.scripts.{key}: out of range")));
    }
    let site_name = {
        let env = *ctx.env()?;
        env.site.site.clone()
    };
    // The offline decision of the scripts door (view and `host.call`) and of the controllers door
    // (every controller step and method, installed by `controllers::build`): both slots are per site
    // and survive a reload, and are filled once every module has registered.
    let scripts_door = door_slot(&site_name, Door::Scripts);
    let controllers_door = door_slot(&site_name, Door::Controllers);
    finalise_door(ctx, "offline_door_scripts", {
        let slot = Arc::clone(&scripts_door);
        move |admission| slot.set(admission)
    });
    finalise_door(ctx, "offline_door_controllers", move |admission| {
        controllers_door.set(admission);
    });
    let slot = slot(&site_name);
    // A site without the platform's Server Script DocType has nowhere to keep scripts.
    let has_store = {
        let env = *ctx.env()?;
        env.permission
            .meta
            .definitions()
            .iter()
            .any(|d| d.name == spot_server_scripts::SERVER_SCRIPT_DOCTYPE)
    };
    if !has_store {
        slot.set(SlotState::Off);
        return Ok(());
    }
    if !settings.enabled || !ctx.assembling() {
        slot.set(SlotState::Off);
        if !settings.enabled {
            tracing::warn!(site = %site_name, "services.scripts.enabled is false: Server Scripts do not run");
        }
        return Ok(());
    }
    let (access, live) = {
        let env = *ctx.env()?;
        let site = SiteId::new(&env.site.site).map_err(crate::error)?;
        (
            EngineAccess::new(
                site,
                Arc::clone(env.live),
                Arc::clone(ctx.clock()),
                env.permission.catalog.clone(),
            ),
            Arc::clone(env.live),
        )
    };
    // The store needs the runtime's generation counter and the runtime needs the store: the
    // counter is made first and handed to both.
    let generation = Arc::new(AtomicU64::new(0));
    let documents = EngineScriptDocuments::new(access, Arc::clone(&generation));
    let store: Arc<dyn ScriptStore> = Arc::new(spot_server_scripts::DocumentScriptStore::new(
        Arc::new(documents),
    ));
    // Who holds a named permission: a module that provides the port decides, else the site's own
    // permission engine does.
    let permissions: Arc<dyn PermissionCheck> = ctx
        .port::<dyn PermissionCheck>()
        .unwrap_or_else(|| Arc::new(LivePermissionCheck::new(Arc::clone(&live))));
    // Elevation (WP-EXT-06.T10) needs somewhere to audit: the port `ElevationLog`, bound to the
    // audit chain appender by the module that owns the chain. Without it no script is ever
    // elevated (default deny), whatever its record says.
    let wiring = ctx.port::<dyn ElevationLog>().map(|log| {
        let engine_of = Arc::clone(&live);
        spot_server_scripts::elevation::ElevationWiring {
            factory: Arc::new(crate::services::script_kinds::EngineAccessFactory::new(
                Arc::new(move || engine_of.engine()),
            )),
            audit: Arc::new(ChainElevationAudit::new(log)),
        }
    });
    if wiring.is_none() {
        tracing::info!(site = %site_name, "no elevation log is provided: scripts are never elevated");
    }
    let (view, caller) = offline_seams(
        Arc::new(AuthorizerView::new(Arc::clone(&live))),
        Arc::new(EngineMethodCaller::new(live)),
        &scripts_door,
    );
    let mut runtime = Runtime::new(settings, slot.script_engine().clone(), store, view, caller)
        .with_permissions(permissions);
    if let Some(wiring) = wiring {
        runtime = runtime.with_elevation(wiring);
    }
    runtime.generation = generation;
    slot.set(SlotState::Ready(Arc::new(runtime)));
    ctx.add(Arc::new(ScriptsService {
        slot: Arc::clone(&slot),
        problem: None,
    }));
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::ts::safe_module_path;
    use super::*;

    #[test]
    fn the_defaults_are_in_range() {
        assert_eq!(Settings::default().invalid_key(), None);
    }

    #[test]
    fn a_ceiling_out_of_range_names_its_key() {
        let settings = Settings {
            max_timeout_ms: 60_000,
            ..Settings::default()
        };
        assert_eq!(settings.invalid_key(), Some("max_timeout_ms"));
        let settings = Settings {
            default_timeout_ms: 3_000,
            ..Settings::default()
        };
        assert_eq!(settings.invalid_key(), Some("default_timeout_ms"));
    }

    #[test]
    fn reserved_doctypes_are_not_scriptable() {
        assert!(!is_scriptable("server_script"));
        assert!(!is_scriptable("api_script"));
        assert!(!is_scriptable("scheduled_script"));
        assert!(!is_scriptable("session"));
        assert!(is_scriptable("invoice"));
    }

    #[test]
    fn module_paths_are_plain_relative_ts_files() {
        assert!(safe_module_path("controllers/invoice.ts"));
        assert!(!safe_module_path("../invoice.ts"));
        assert!(!safe_module_path("/etc/passwd.ts"));
        assert!(!safe_module_path("controllers/.hidden.ts"));
        assert!(!safe_module_path("controllers\\invoice.ts"));
        assert!(!safe_module_path("controllers/invoice.js"));
        assert!(!safe_module_path(""));
    }

    #[test]
    fn hub_steps_are_nine_and_ordered_by_event() {
        let hub = ScriptHub::new("note", ScriptSlot::detached(), Duration::from_secs(1));
        let steps = hub.steps();
        assert_eq!(steps.len(), 9);
        assert!(steps.iter().all(|s| s.priority >= SCRIPT_PRIORITY_OFFSET));
        let before_validate = steps
            .iter()
            .find(|s| s.name.as_str() == "server_script:run_note_before_validate")
            .unwrap();
        let before_save = steps
            .iter()
            .find(|s| s.name.as_str() == "server_script:run_note_before_save")
            .unwrap();
        assert_eq!(before_validate.point, before_save.point);
        assert!(before_validate.priority < before_save.priority);
    }

    use serde_json::json;

    // ---- the offline decision (WP-IAM-09.T5) ----

    struct Echo;

    impl MethodCaller for Echo {
        fn call<'a>(
            &'a self,
            _inv: &'a Invocation,
            _doctype: &'a str,
            _id: RecordId,
            _method: &'a str,
            args: Json,
        ) -> BoxFuture<'a, AppResult<Json>> {
            Box::pin(async move { Ok(args) })
        }
    }

    struct Refusing(spot_http::api::service::OfflineRefusal);

    impl spot_http::api::service::OfflineAdmission for Refusing {
        fn admit(
            &self,
            _door: Door,
            _user: &str,
            _tier: DoorTier,
            _permission: Option<&str>,
        ) -> std::result::Result<(), spot_http::api::service::OfflineRefusal> {
            Err(self.0)
        }

        fn node_admitted(
            &self,
            _door: Door,
            _tier: DoorTier,
        ) -> std::result::Result<(), spot_http::api::service::OfflineRefusal> {
            Err(self.0)
        }
    }

    fn user_invocation() -> Invocation {
        let mut bytes = [3_u8; 16];
        bytes[6] = 0x70;
        bytes[8] = 0x80;
        Invocation {
            site: SiteId::new("acme.test").unwrap(),
            actor: Actor::User {
                id: RecordId::from_bytes(bytes).unwrap(),
                via: None,
            },
            command_id: None,
            language: None,
            now: time::OffsetDateTime::UNIX_EPOCH,
        }
    }

    fn call(caller: &dyn MethodCaller, inv: &Invocation) -> AppResult<Json> {
        let mut bytes = [4_u8; 16];
        bytes[6] = 0x70;
        bytes[8] = 0x80;
        let id = RecordId::from_bytes(bytes).unwrap();
        block_on(caller.call(inv, "note", id, "ping", json!({"n": 1})))
    }

    /// Polls a future that never waits (the doubles answer at once).
    fn block_on<T>(future: BoxFuture<'_, T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(future)
    }

    #[test]
    fn a_script_acting_for_a_revoked_user_gets_nothing_and_an_empty_slot_changes_nothing() {
        use spot_http::api::service::OfflineRefusal;
        let inv = user_invocation();
        let slot = Arc::new(OfflineSlot::default());
        let (_, caller) = offline_seams(
            Arc::new(spot_server_scripts::AllowAllView),
            Arc::new(Echo),
            &slot,
        );
        // No provider installed: unchanged.
        assert_eq!(call(&*caller, &inv).unwrap(), json!({"n": 1}));
        slot.set(Some(Arc::new(Refusing(OfflineRefusal::Denied))));
        assert_eq!(call(&*caller, &inv).unwrap_err().code, "permission.denied");
        slot.set(Some(Arc::new(Refusing(OfflineRefusal::NotReady))));
        assert_eq!(call(&*caller, &inv).unwrap_err().code, "server.unavailable");
        // The platform itself is never decided.
        let system = Invocation {
            actor: Actor::System {
                component: Name::new("scripts").unwrap(),
            },
            ..inv
        };
        assert!(call(&*caller, &system).is_ok());
    }
}
