//! Shared test fixtures: a configurable [`Probe`] controller and small builders.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::Value as Json;
use spot_app_api::prelude::{CommandId, Name, OffsetDateTime, Value};
use spot_app_api::{
    Actor, AppResult, BoxFuture, Controller, HookContext, HookPoint, Invocation, MethodContext,
    MethodDecl, StepDecl, StepName,
};
use spot_core::SiteId;
use spot_manifest::AppManifest;

/// The three apps the ordering tests use.
pub const APP_A: &str = "app_a";
/// See [`APP_A`].
pub const APP_B: &str = "app_b";
/// See [`APP_A`].
pub const APP_C: &str = "app_c";

/// A storage name.
pub fn n(name: &str) -> Name {
    Name::new(name).expect("a valid storage name")
}

/// An invocation for tests.
pub fn inv() -> Invocation {
    Invocation {
        site: SiteId::new("controllers.test").expect("a valid site id"),
        actor: Actor::System {
            component: n("controllers_tests"),
        },
        command_id: CommandId::new("cmd-1").ok(),
        language: None,
        now: OffsetDateTime::UNIX_EPOCH,
    }
}

/// A text value.
pub fn text(value: &str) -> Value {
    Value::Text(value.to_owned())
}

/// A step declaration named `<app>:<step>`.
pub fn step(name: &str, point: HookPoint, priority: i32) -> StepDecl {
    StepDecl {
        name: StepName::new(name).expect("a valid step name"),
        point,
        priority,
        budget: Duration::from_millis(200),
    }
}

/// A whitelisted-method declaration.
pub fn method(name: &str, mutates: bool) -> MethodDecl {
    MethodDecl::new(n(name), mutates)
}

/// A controller that records how often it ran, and can set one field on the draft it sees.
pub struct Probe {
    doctype: String,
    steps: Vec<StepDecl>,
    methods: Vec<MethodDecl>,
    set_field: Option<(Name, Value)>,
    /// How many times a step ran.
    pub runs: Arc<AtomicUsize>,
    /// How many times a method was called.
    pub method_calls: Arc<AtomicUsize>,
}

impl Probe {
    /// An empty controller for `doctype`.
    pub fn new(doctype: &str) -> Probe {
        Probe {
            doctype: doctype.to_owned(),
            steps: Vec::new(),
            methods: Vec::new(),
            set_field: None,
            runs: Arc::new(AtomicUsize::new(0)),
            method_calls: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Declares a step.
    #[must_use]
    pub fn with_step(mut self, decl: StepDecl) -> Probe {
        self.steps.push(decl);
        self
    }

    /// Declares a whitelisted method.
    #[must_use]
    pub fn with_method(mut self, decl: MethodDecl) -> Probe {
        self.methods.push(decl);
        self
    }

    /// Makes the step set `field` to `value` on the draft.
    #[must_use]
    pub fn setting(mut self, field: &str, value: Value) -> Probe {
        self.set_field = Some((n(field), value));
        self
    }

    /// The number of step runs so far.
    pub fn runs(&self) -> usize {
        self.runs.load(Ordering::SeqCst)
    }

    /// The number of method calls so far.
    pub fn calls(&self) -> usize {
        self.method_calls.load(Ordering::SeqCst)
    }
}

impl Controller for Probe {
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
        _step: &'a StepName,
        ctx: &'a mut dyn HookContext,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            self.runs.fetch_add(1, Ordering::SeqCst);
            if let Some((field, value)) = &self.set_field {
                ctx.doc_mut()?.set(field.clone(), value.clone());
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
            self.method_calls.fetch_add(1, Ordering::SeqCst);
            Ok(serde_json::json!({ "method": method, "args": args }))
        })
    }
}

/// Parses a manifest written as TOML, so tests exercise the real manifest path.
pub fn manifest(text: &str) -> AppManifest {
    match spot_manifest::parse(text, "test-manifest.toml") {
        Ok(manifest) => manifest,
        Err(errors) => panic!(
            "manifest must parse: {}",
            errors
                .iter()
                .map(|e| format!("{}: {}", e.code, e.message))
                .collect::<Vec<_>>()
                .join("; ")
        ),
    }
}
