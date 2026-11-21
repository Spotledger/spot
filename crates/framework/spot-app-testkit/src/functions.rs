//! `call_function`: the app's whitelisted module functions, dispatched through the real function
//! registry (verbs, guest rule, rate limit and argument schema are the registry's own).
//!
//! The app declares a function in its manifest (`[[functions]]`: module, name, verbs, guest); the
//! test supplies the handler (the Rust implementation of that function) with
//! [`HarnessBuilder::function`](crate::HarnessBuilder::function). A handler that was not declared
//! in the manifest is refused when the harness is built, so a test cannot call what the app does
//! not whitelist.

use std::sync::Arc;

use serde_json::Value as Json;
use spot_app_api::AppError;
use spot_functions::{
    Context, FunctionHandler, FunctionRegistry, FunctionRegistryBuilder, HandlerFuture,
};

use crate::error::{Result, TestkitError};
use crate::load::LoadedApp;

/// A handler registered by the test, for the function `module`/`name` of `app`.
#[derive(Clone)]
pub(crate) struct Registered {
    pub(crate) app: String,
    pub(crate) module: String,
    pub(crate) name: String,
    pub(crate) handler: Arc<dyn FunctionHandler>,
}

/// A handler from a plain function over the call's context and arguments, for functions that do
/// not need to await anything.
pub struct SyncHandler<F>(F);

/// Wraps `f` as a [`FunctionHandler`].
pub fn sync_handler<F>(f: F) -> Arc<dyn FunctionHandler>
where
    F: Fn(&Context, Json) -> std::result::Result<Json, AppError> + Send + Sync + 'static,
{
    Arc::new(SyncHandler(f))
}

impl<F> FunctionHandler for SyncHandler<F>
where
    F: Fn(&Context, Json) -> std::result::Result<Json, AppError> + Send + Sync + 'static,
{
    fn call<'a>(&'a self, ctx: &'a Context, args: Json) -> HandlerFuture<'a> {
        let result = (self.0)(ctx, args);
        Box::pin(async move { result })
    }
}

/// Builds the registry from the handlers the test registered and the manifests' declarations.
///
/// # Errors
///
/// `app_testkit.app_invalid` for a handler the manifest does not declare, or when the registry
/// reports conflicts (every one is listed).
pub(crate) fn build(
    apps: &[LoadedApp],
    handlers: &[Registered],
    clock: Arc<dyn spot_core::clock::Clock>,
) -> Result<FunctionRegistry> {
    let mut builder = FunctionRegistryBuilder::new();
    builder.clock(clock);
    let mut problems = Vec::new();
    for handler in handlers {
        let declared = apps
            .iter()
            .filter_map(|app| app.manifest.as_ref())
            .filter(|manifest| manifest.app.name.as_str() == handler.app)
            .flat_map(|manifest| manifest.functions.iter())
            .find(|sub| sub.module.as_str() == handler.module && sub.name.as_str() == handler.name);
        match declared {
            Some(sub) => {
                builder.register(
                    &handler.app,
                    sub.clone(),
                    Arc::clone(&handler.handler),
                    None,
                    None,
                );
            }
            None => problems.push(format!(
                "the function `{}/{}/{}` has a handler but the app's manifest does not declare it",
                handler.app, handler.module, handler.name
            )),
        }
    }
    if !problems.is_empty() {
        return Err(TestkitError::invalid("the functions do not load", problems));
    }
    builder.build().map_err(|conflicts| {
        TestkitError::invalid(
            "the functions conflict",
            conflicts
                .iter()
                .map(|c| format!("{}: {}", c.code, c.message))
                .collect(),
        )
    })
}
