//! The native controllers built into this binary, by the name an app's `[app] controller`
//! reference gives them (`native:sample`).
//!
//! The table starts with the sample app's controllers. A binary that links further apps (and
//! an integration test that needs a controller of its own) adds to it with [`register`] before
//! the host starts; nothing is ever read from a path, a plug-in directory or the network, so
//! the set of native controllers a process can run is fixed by what was compiled in and what
//! its own `main` registered.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use spot_app_api::Controller;
use spot_controllers::NativeControllers;
use spot_functions::{FunctionHandler, HandlerFuture};
use spot_jobs::worker::{HandlerResult, JobContext, JobHandlerRegistry};

fn extra() -> MutexGuard<'static, BTreeMap<String, Arc<dyn Controller>>> {
    static EXTRA: OnceLock<Mutex<BTreeMap<String, Arc<dyn Controller>>>> = OnceLock::new();
    EXTRA
        .get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

/// Makes `controller` available as `native:<name>`, replacing an earlier one of that name.
///
/// Call it before the host starts. A name is `<app>` for an app's main controller and
/// `<app>.<suffix>` for a companion serving another DocType.
pub fn register(name: &str, controller: Arc<dyn Controller>) {
    extra().insert(name.to_owned(), controller);
}

/// Whether `name` is a native controller of this process.
pub fn is_built_in(name: &str) -> bool {
    spot_sample_app::NATIVE_CONTROLLERS
        .iter()
        .any(|(native, _)| *native == name)
        || extra().contains_key(name)
}

/// The table of every native controller of this process.
pub fn table() -> NativeControllers {
    let mut table = NativeControllers::new();
    let sample = spot_sample_app::controllers();
    for (name, doctype) in spot_sample_app::NATIVE_CONTROLLERS {
        if let Some(controller) = sample.iter().find(|c| c.doctype() == *doctype) {
            table.insert(name, Arc::clone(controller));
        }
    }
    for (name, controller) in extra().iter() {
        table.insert(name, Arc::clone(controller));
    }
    table
}

/// A job handler an app contributes, by the job name it serves.
pub type JobFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = spot_jobs::Result<HandlerResult>> + Send>>;

type JobFn = Arc<dyn Fn(JobContext) -> JobFuture + Send + Sync>;

fn job_table() -> MutexGuard<'static, BTreeMap<String, JobFn>> {
    static JOBS: OnceLock<Mutex<BTreeMap<String, JobFn>>> = OnceLock::new();
    JOBS.get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

/// Makes `handler` run the jobs called `name` (`<app>:<job>`) on every site of this process.
///
/// Call it before the host starts. Like [`register`], it only widens what the binary itself
/// contains: the host never loads handlers from anywhere else.
pub fn register_job(name: &str, handler: impl Fn(JobContext) -> JobFuture + Send + Sync + 'static) {
    job_table().insert(name.to_owned(), Arc::new(handler));
}

/// Adds every registered job handler to `registry`.
pub fn add_job_handlers(registry: &mut JobHandlerRegistry) {
    for (name, handler) in job_table().iter() {
        let handler = Arc::clone(handler);
        registry.register(name.clone(), move |ctx| handler(ctx));
    }
}

// ---------------------------------------------------------------------------------------------
// Whitelisted module-level functions (WP-PLT-17.T2)
// ---------------------------------------------------------------------------------------------

/// What a native function adds to its manifest declaration: the rate limit and the argument
/// schema the manifest does not carry.
///
/// Neither is required. A guest function without a `limit` runs under the site's default guest
/// limit (`services.function`), so a public entry point is never unlimited by omission.
#[derive(Clone, Debug, Default)]
pub struct FunctionSpec {
    /// Calls per window and the window, per `(function, caller)`.
    pub limit: Option<(u32, std::time::Duration)>,
    /// The argument schema (`type`, `required` and `properties` only).
    pub arguments: Option<serde_json::Value>,
}

/// One native function as the table holds it.
#[derive(Clone)]
pub struct NativeFunction {
    /// The declaring app.
    pub app: String,
    /// The module the function is declared in.
    pub module: String,
    /// The function's name.
    pub name: String,
    /// The limit and schema.
    pub spec: FunctionSpec,
    /// The implementation.
    pub handler: Arc<dyn FunctionHandler>,
}

type FunctionKeyParts = (String, String, String);

fn function_table() -> MutexGuard<'static, BTreeMap<FunctionKeyParts, NativeFunction>> {
    static FUNCTIONS: OnceLock<Mutex<BTreeMap<FunctionKeyParts, NativeFunction>>> = OnceLock::new();
    FUNCTIONS
        .get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
}

/// Makes `handler` the implementation of the function `app/module/name`, replacing an earlier
/// one with the same address.
///
/// Registering is not declaring: a function is callable only when an installed app's manifest
/// declares it with a `[[functions]]` entry (the whitelist). A handler nobody declares is never
/// reached, and the host says so once at start-up. Call it before the host starts; like
/// [`register`] it only widens what the binary itself contains.
pub fn register_function(app: &str, module: &str, name: &str, handler: Arc<dyn FunctionHandler>) {
    register_function_with(app, module, name, FunctionSpec::default(), handler);
}

/// Like [`register_function`], with the rate limit and argument schema of `spec`.
pub fn register_function_with(
    app: &str,
    module: &str,
    name: &str,
    spec: FunctionSpec,
    handler: Arc<dyn FunctionHandler>,
) {
    function_table().insert(
        (app.to_owned(), module.to_owned(), name.to_owned()),
        NativeFunction {
            app: app.to_owned(),
            module: module.to_owned(),
            name: name.to_owned(),
            spec,
            handler,
        },
    );
}

/// What a closure-style native function is called with: the caller and the call, as plain
/// values (no `spot-functions` type is needed to write one).
#[derive(Clone, Debug)]
pub struct FunctionCall {
    /// The site the call runs in.
    pub tenant: String,
    /// The signed-in caller's user id; `None` for a guest.
    pub user: Option<String>,
    /// `"GET"` or `"POST"` for a call over HTTP.
    pub verb: Option<&'static str>,
    /// The call's `Idempotency-Key`, when the caller sent one.
    pub command_id: Option<String>,
    /// The (validated) arguments.
    pub args: serde_json::Value,
}

/// A closure-style function's refusal: a stable dotted code and a plain message. It reaches the
/// caller as a `422 document.validation_failed`, or as a `403 permission.denied` when `denied` is
/// set (the caller lacks a right the function needs); it never carries arguments.
#[derive(Clone, Debug)]
pub struct FunctionFailure {
    /// A dotted lower-case code, for example `sample.nothing_to_total`.
    pub code: String,
    /// What went wrong, in plain words.
    pub message: String,
    /// The caller may not do this (a `403`), as opposed to a request that is wrong (a `422`).
    pub denied: bool,
}

/// The future a closure-style function returns.
pub type FunctionCallFuture = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<serde_json::Value, FunctionFailure>> + Send>,
>;

struct ClosureFunction<F>(F);

impl<F> FunctionHandler for ClosureFunction<F>
where
    F: Fn(FunctionCall) -> FunctionCallFuture + Send + Sync + 'static,
{
    fn call<'a>(
        &'a self,
        ctx: &'a spot_functions::Context,
        args: serde_json::Value,
    ) -> HandlerFuture<'a> {
        let call = FunctionCall {
            tenant: ctx.tenant.clone(),
            user: match &ctx.caller {
                spot_functions::Caller::User(id) => Some(id.clone()),
                _ => None,
            },
            verb: ctx.verb.map(spot_functions::verb_name),
            command_id: ctx.command_id.clone(),
            args,
        };
        let future = (self.0)(call);
        Box::pin(async move {
            future.await.map_err(|failure| {
                // The handler's own code is kept when it is a well-formed dotted code; otherwise
                // the answer is the catalogued generic one for its class.
                let code = if spot_app_api::error::is_code(&failure.code) {
                    failure.code
                } else if failure.denied {
                    "permission.denied".to_owned()
                } else {
                    "document.validation_failed".to_owned()
                };
                let class = if failure.denied {
                    spot_app_api::error::ErrorClass::PermissionDenied
                } else {
                    spot_app_api::error::ErrorClass::Validation
                };
                spot_app_api::AppError::new(class, &code, failure.message)
            })
        })
    }
}

/// Registers a closure as the implementation of `app/module/name` (see [`register_function`]).
pub fn register_function_fn(
    app: &str,
    module: &str,
    name: &str,
    spec: FunctionSpec,
    handler: impl Fn(FunctionCall) -> FunctionCallFuture + Send + Sync + 'static,
) {
    register_function_with(app, module, name, spec, Arc::new(ClosureFunction(handler)));
}

/// Every native function of this process, in address order.
pub fn function_handlers() -> Vec<NativeFunction> {
    function_table().values().cloned().collect()
}
