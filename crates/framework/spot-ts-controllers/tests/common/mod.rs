//! Shared fixtures for the integration tests: a native reference controller, the equivalent
//! TypeScript module, effective metadata and an engine over the fake backend.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use serde_json::{Value as Json, json};
use spot_app_api::error::FieldPath;
use spot_app_api::prelude::{Name, RecordId, Value};
use spot_app_api::{
    Actor, AppError, AppResult, BoxFuture, Controller, ErrorClass, HookContext, Invocation,
    MethodContext, MethodDecl, StepDecl, StepName,
};
use spot_controllers::{ControllerFactory, ControllerRef};
use spot_core::{SiteId, SystemClock};
use spot_document::engine::{Authorizer, Engine, V7Ids};
use spot_meta::{Definition, FieldSpec, FieldType, Registry};
use spot_storage_fake::FakeBackend;
use spot_ts::{EngineConfig, ScriptEngine};
use spot_ts_controllers::{MemoryModules, TsControllerFactory, TsLimits};

/// A storage name.
pub fn n(name: &str) -> Name {
    Name::new(name).expect("a valid storage name")
}

/// A system invocation.
pub fn system_inv() -> Invocation {
    Invocation {
        site: SiteId::new("tscontrollers.test").expect("a site id"),
        actor: Actor::System {
            component: n("ts_controller_tests"),
        },
        command_id: None,
        language: None,
        now: time::OffsetDateTime::UNIX_EPOCH,
    }
}

/// A user invocation.
pub fn user_inv(id: RecordId) -> Invocation {
    Invocation {
        actor: Actor::User { id, via: None },
        ..system_inv()
    }
}

/// The `note` metadata the tests use: `title`, `total`, `flag` and a field a view may hide.
pub fn note_meta() -> Arc<spot_meta::EffectiveMeta> {
    let mut note = Definition::new("note");
    note.fields.push(FieldSpec::new("title", FieldType::Text));
    let mut total = FieldSpec::new("total", FieldType::Decimal);
    total.scale = Some(2);
    note.fields.push(total);
    note.fields.push(FieldSpec::new("flag", FieldType::Bool));
    note.fields.push(FieldSpec::new("hidden", FieldType::Text));
    Registry::new(vec![note])
        .expect("the definition compiles")
        .effective()
}

/// A fresh engine over the fake backend with `authorizer`.
pub fn build_engine(
    seams: Arc<spot_seams::Registry>,
    meta: &Arc<spot_meta::EffectiveMeta>,
    authorizer: Arc<dyn Authorizer>,
) -> Engine<FakeBackend> {
    Engine::new(
        FakeBackend::new(),
        Arc::clone(meta),
        seams,
        Arc::new(SystemClock::new()),
        Arc::new(V7Ids),
    )
    .expect("the engine builds")
    .with_authorizer(authorizer)
}

/// An engine whose authorizer enforces nothing, for tests that are not about permissions.
pub fn open_engine(seams: Arc<spot_seams::Registry>) -> Engine<FakeBackend> {
    build_engine(
        seams,
        &note_meta(),
        Arc::new(spot_document::engine::AllowAllAuthorizer),
    )
}

/// A script engine with the controller host surface.
pub fn ts_engine() -> ScriptEngine {
    ScriptEngine::new(EngineConfig {
        surface: spot_ts_controllers::ts_host_surface(),
        ..EngineConfig::default()
    })
}

/// A TypeScript controller factory serving `TS_MODULE` for the app `ts_controllers`.
pub fn ts_factory() -> TsControllerFactory {
    let modules = MemoryModules::new().with("ts_controllers", "controllers/note.ts", TS_MODULE);
    TsControllerFactory::new("ts_controllers", ts_engine(), Arc::new(modules))
        .with_limits(TsLimits::default())
}

/// The reference the TypeScript module is named by.
pub fn ts_reference() -> ControllerRef {
    ControllerRef::parse("ts:controllers/note.ts").expect("a valid reference")
}

/// The TypeScript module the conformance fixture describes.
pub const TS_MODULE: &str = r#"
const controller = {
  doctype: "note",
  steps: {
    fill_title: { point: "before_save", priority: 10 },
    peek: { point: "validate" },
  },
  methods: {
    ping: {
      right: "read",
      args: {},
      result: { pong: "bool" },
      handler() { return { pong: true }; },
    },
    bump: {
      right: "write",
      args: { amount: "decimal", fail: "bool?" },
      result: { total: "decimal" },
      handler({ args }) {
        if (args.fail) {
          throw { code: "tscontroller.conformance_fail", message: "asked to fail" };
        }
        return { total: args.amount };
      },
    },
  },
};
export default controller;
export function contract() {
  return {
    doctype: "note",
    steps: {
      fill_title: { point: "before_save", priority: 10 },
      peek: { point: "validate" },
    },
    methods: {
      ping: { right: "read", args: {}, result: { pong: "bool" } },
      bump: { right: "write", args: { amount: "decimal", fail: "bool?" }, result: { total: "decimal" } },
    },
  };
}
export function run_step({ step, doc, ctx }) {
  if (step === "fill_title") { return { patch: { title: "conformed" } }; }
  if (step === "peek" && doc.flag) { return { patch: { title: "bad" } }; }
  return null;
}
export function run_method({ method, doc, args }) {
  return controller.methods[method].handler({ doc, args });
}
"#;

/// The native reference controller: the same fixture, in Rust.
pub struct NativeNote {
    doctype: String,
    steps: Vec<StepDecl>,
    methods: Vec<MethodDecl>,
}

impl NativeNote {
    /// The fixture's controller.
    pub fn new() -> NativeNote {
        NativeNote {
            doctype: "note".to_owned(),
            steps: vec![
                StepDecl {
                    name: StepName::new("native:fill_title").expect("a step name"),
                    point: spot_app_api::HookPoint::of(
                        spot_app_api::Phase::Before,
                        spot_app_api::Target::Save,
                    ),
                    priority: 10,
                    budget: std::time::Duration::from_millis(200),
                },
                StepDecl {
                    name: StepName::new("native:peek").expect("a step name"),
                    point: spot_app_api::HookPoint::of(
                        spot_app_api::Phase::Validate,
                        spot_app_api::Target::Save,
                    ),
                    priority: 0,
                    budget: std::time::Duration::from_millis(200),
                },
            ],
            methods: vec![
                MethodDecl::new(n("ping"), false),
                MethodDecl::new(n("bump"), true),
            ],
        }
    }
}

impl Controller for NativeNote {
    fn doctype(&self) -> &str {
        &self.doctype
    }

    fn steps(&self) -> Vec<StepDecl> {
        self.steps.clone()
    }

    fn methods(&self) -> Vec<MethodDecl> {
        self.methods.clone()
    }

    fn run_step<'a>(
        &'a self,
        step: &'a StepName,
        ctx: &'a mut dyn HookContext,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            match step.as_str() {
                "native:fill_title" => {
                    ctx.doc_mut()?
                        .set(n("title"), Value::Text("conformed".to_owned()));
                }
                "native:peek" => {
                    let flagged = matches!(ctx.doc().get("flag"), Value::Bool(true));
                    if flagged {
                        // The engine refuses this with `platform.phase_violation`.
                        ctx.doc_mut()?
                            .set(n("title"), Value::Text("bad".to_owned()));
                    }
                }
                _ => {}
            }
            Ok(())
        })
    }

    fn call_method<'a>(
        &'a self,
        method: &'a str,
        args: Json,
        _ctx: &'a mut dyn MethodContext,
    ) -> BoxFuture<'a, AppResult<Json>> {
        Box::pin(async move {
            match method {
                "ping" => Ok(json!({ "pong": true })),
                "bump" => {
                    let amount = args
                        .get("amount")
                        .and_then(Json::as_str)
                        .filter(|text| spot_money::rounding::parse_decimal(text).is_ok())
                        .ok_or_else(|| {
                            AppError::field(
                                FieldPath::field("amount"),
                                "whitelist.bad_arguments",
                                "expected a decimal string",
                            )
                        })?;
                    if args.get("fail").and_then(Json::as_bool) == Some(true) {
                        return Err(AppError::new(
                            ErrorClass::Validation,
                            "tscontroller.conformance_fail",
                            "asked to fail",
                        ));
                    }
                    Ok(json!({ "total": amount }))
                }
                _ => Err(AppError::new(
                    ErrorClass::MethodNotFound,
                    "method.not_found",
                    format!("no whitelisted method `{method}`"),
                )),
            }
        })
    }
}

/// A factory serving the native reference controller.
pub struct NativeFactory;

impl ControllerFactory for NativeFactory {
    fn create(&self, reference: &ControllerRef) -> AppResult<Arc<dyn Controller>> {
        match reference.source() {
            spot_controllers::ControllerSource::Native => Ok(Arc::new(NativeNote::new())),
            other => Err(AppError::new(
                ErrorClass::Unavailable,
                "controller.host_unavailable",
                format!("no {other} adapter in this test"),
            )),
        }
    }
}

/// A TypeScript controller loaded from `source`, for a test that does not need a bundle.
pub fn ts_controller(
    app: &str,
    path: &str,
    source: &str,
    limits: TsLimits,
) -> Arc<spot_ts_controllers::TsController> {
    let loaded = spot_ts_controllers::load_in_memory(app, path, source, limits)
        .expect("the module loads and matches the contract");
    Arc::new(spot_ts_controllers::TsController::from_parts(
        app,
        path,
        loaded,
        ts_engine(),
        limits,
    ))
}

/// An engine whose only controller is `controller`, registered as the TypeScript source.
pub fn engine_for(controller: Arc<dyn Controller>, app: &str) -> Engine<FakeBackend> {
    let mut registry = spot_controllers::ControllerRegistry::new();
    registry.register(
        app,
        Arc::clone(&controller),
        spot_controllers::ControllerSource::TypeScript,
    );
    let seams = registry.install().expect("no step conflicts");
    let engine = open_engine(seams);
    engine.set_controllers(vec![controller]);
    engine
}
