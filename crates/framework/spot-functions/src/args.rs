//! [`FunctionHandler`]: the object-safe trait every implementation of a whitelisted function
//! sits behind, plus [`Caller`] and [`Context`]: what a call runs in.

use std::future::Future;
use std::pin::Pin;

use spot_app_api::error::AppError;
use spot_manifest::HttpVerb;

/// A boxed, `Send` future: the return type of [`FunctionHandler::call`], so the trait stays
/// object-safe behind `Arc<dyn FunctionHandler>` (the app API contract's shape).
pub type HandlerFuture<'a> =
    Pin<Box<dyn Future<Output = Result<serde_json::Value, AppError>> + Send + 'a>>;

/// Who is calling a function.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Caller {
    /// An unauthenticated guest; only a function declared `guest` accepts it, and only a
    /// signed-in user accepts the verbs that change things.
    Anonymous,
    /// A signed-in user or API key owner, by record id.
    User(String),
    /// A platform component acting without a user: a job, the scheduler, a migration, sync
    /// apply — the manifest's `Actor::System` components, named for the rate limiter.
    System(String),
}

impl Caller {
    /// The rate limiter's bucket for this caller: a `System` or `User` identity gets its own
    /// bucket; every anonymous caller shares one bucket.
    pub fn bucket(&self) -> String {
        match self {
            Caller::Anonymous => "anonymous".to_owned(),
            Caller::System(id) => format!("system:{id}"),
            Caller::User(id) => format!("user:{id}"),
        }
    }

    /// Whether an anonymous caller.
    pub fn is_anonymous(&self) -> bool {
        matches!(self, Caller::Anonymous)
    }
}

/// What one function call runs in (the brief: caller identity, verb, tenant, command id; no
/// storage or HTTP types, so handlers stay testable alone).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Context {
    /// The caller: a guest, a user or a platform component.
    pub caller: Caller,
    /// The verb the call arrived with, when it arrived over HTTP.
    pub verb: Option<HttpVerb>,
    /// The tenant (site) the call runs in.
    pub tenant: String,
    /// The command id (the API's `Idempotency-Key`), when the transport supplied one.
    pub command_id: Option<String>,
}

impl Context {
    /// A context for tests and jobs: nobody, no verb, a tenant, no command id.
    pub fn new(tenant: &str, caller: Caller) -> Context {
        Context {
            caller,
            verb: None,
            tenant: tenant.to_owned(),
            command_id: None,
        }
    }
}

/// One whitelisted function's implementation: native, WASM or script, all behind this one
/// trait (ADR-16). Object-safe for `Arc<dyn FunctionHandler>`.
pub trait FunctionHandler: Send + Sync + 'static {
    /// Runs the function with `args` in `ctx`.
    ///
    /// Implementations must never log `args` or copy argument content into errors (the crate's
    /// no-argument-logging rule) and must not read the clock except through a host-injected
    /// `Clock`. A failing handler returns its own `AppError`; the dispatcher passes it through
    /// untouched.
    fn call<'a>(&'a self, ctx: &'a Context, args: serde_json::Value) -> HandlerFuture<'a>;
}

/// What a function is, as [`crate::FunctionRegistry::list`] reports it: the handler and, when
/// set, its declared limit.
pub struct FunctionHandlerInfo {
    /// The handler that runs (the override's, when one overrides).
    pub handler: std::sync::Arc<dyn FunctionHandler>,
    /// The declared rate limit, when one is set.
    pub limit: Option<crate::RateLimit>,
}

impl FunctionHandlerInfo {
    /// Wraps a bare handler with no limit.
    pub fn bare(handler: std::sync::Arc<dyn FunctionHandler>) -> FunctionHandlerInfo {
        FunctionHandlerInfo {
            handler,
            limit: None,
        }
    }
}
