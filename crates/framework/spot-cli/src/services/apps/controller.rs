//! The hooks and functions of an app, as the controllers the seam registry takes.
//!
//! [`wasm_controllers`] builds one [`spot_app_api::Controller`] per `(app, DocType)` from the
//! app's signed manifest (`[[events]]`). The registry decides the order and the phase of each step
//! and refuses a `validate` step that writes (ADR-51); this module only runs a step the registry
//! planned. A step calls the app's `on-event` export with a small JSON event
//! (`{"step","doctype","id","phase"}`); documents the step needs it reads through the `get-doc`
//! import, which runs as the invoking user.
//!
//! A failing step fails the operation when it runs before the write: the app's error text is
//! sanitised ([`super::runtime::sanitise`]-style: bounded, no control characters) and a limit kill
//! is the generic `app.limit_exceeded`.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::{Value, json};
use spot_app_api::{
    Actor, AppError, AppResult, BoxFuture, Controller, ErrorClass, HookContext, StepDecl, StepName,
};

use super::runtime::{AppRuntime, CallError, CallIdentity, HOOK_EXPORT, LIMIT_EXCEEDED};

/// Maps a call failure to the error class an operation reports.
fn app_error(error: &CallError) -> AppError {
    let class = if error.code == LIMIT_EXCEEDED {
        ErrorClass::Unavailable
    } else {
        ErrorClass::Internal
    };
    AppError::new(class, error.code, error.message.clone())
}

/// The user id of an actor, when it is a user: the only identity whose rights an app runs with.
#[must_use]
pub fn user_of(actor: &Actor) -> Option<String> {
    match actor {
        Actor::User { id, .. } => Some(id.to_string()),
        _ => None,
    }
}

/// The controller of one app for one DocType.
pub struct WasmController {
    app: String,
    doctype: String,
    steps: Vec<StepDecl>,
    runtime: Arc<AppRuntime>,
}

impl Controller for WasmController {
    fn doctype(&self) -> &str {
        &self.doctype
    }

    fn steps(&self) -> Vec<StepDecl> {
        self.steps.clone()
    }

    fn run_step<'a>(
        &'a self,
        step: &'a StepName,
        ctx: &'a mut dyn HookContext,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            let who = {
                let inv = ctx.invocation();
                CallIdentity {
                    user: user_of(&inv.actor),
                    now_micros: i64::try_from(inv.now.unix_timestamp_nanos() / 1_000)
                        .unwrap_or(i64::MAX),
                    external_allowed: false,
                }
            };
            let who = CallIdentity {
                external_allowed: ctx.external_allowed().is_ok(),
                ..who
            };
            let event = json!({
                "step": step.as_str(),
                "doctype": ctx.doc().doctype().as_str(),
                "id": ctx.doc().id().to_string(),
                "phase": ctx.point().phase().as_str(),
            });
            self.runtime
                .call(
                    &self.app,
                    HOOK_EXPORT,
                    Value::Array(vec![Value::String(event.to_string())]),
                    who,
                )
                .await
                .map(|_| ())
                .map_err(|e| app_error(&e))
        })
    }
}

/// One controller per `(app, DocType)` that the loaded apps' manifests declare steps for.
#[must_use]
pub fn wasm_controllers(runtime: &Arc<AppRuntime>) -> Vec<(String, Arc<dyn Controller>)> {
    let mut out: Vec<(String, Arc<dyn Controller>)> = Vec::new();
    for info in runtime.list() {
        let Some(loaded) = runtime.app(&info.name) else {
            continue;
        };
        let mut by_doctype: BTreeMap<String, Vec<StepDecl>> = BTreeMap::new();
        let Some(manifest) = &loaded.manifest else {
            continue;
        };
        for (doctype, decl) in manifest.step_decls() {
            by_doctype.entry(doctype).or_default().push(decl);
        }
        for (doctype, steps) in by_doctype {
            out.push((
                info.name.clone(),
                Arc::new(WasmController {
                    app: info.name.clone(),
                    doctype,
                    steps,
                    runtime: Arc::clone(runtime),
                }),
            ));
        }
    }
    out
}

/// The event an app function call sends: `{"function": "<module>/<name>", "args": ...}`.
#[must_use]
pub fn function_event(module: &str, name: &str, args: &Value) -> Value {
    json!({ "function": format!("{module}/{name}"), "args": args })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_function_event_names_the_function() {
        let event = function_event("m", "f", &json!({"a": 1}));
        assert_eq!(event["function"], "m/f");
        assert_eq!(event["args"]["a"], 1);
    }

    #[test]
    fn a_limit_kill_is_unavailable_and_generic() {
        let error = CallError {
            code: LIMIT_EXCEEDED,
            message: "the app exceeded a resource limit and was stopped".to_owned(),
            limit: Some("fuel"),
        };
        let mapped = app_error(&error);
        assert_eq!(mapped.into_body().code, LIMIT_EXCEEDED);
    }
}
