//! Shared helpers: a fake handler whose identity the tests read back, and small conveniences.
#![allow(clippy::unwrap_used, dead_code)]

use std::sync::Arc;
use std::sync::Mutex;

use spot_app_api::error::AppError;
use spot_core::clock::{Clock, SystemClock};
use spot_functions::{Caller, Context, FunctionHandler, InvalidFunctionKey};

/// What the fake handler did: who called it, with what verb, tenant and arguments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Observed {
    /// The tenant the call saw.
    pub tenant: String,
    /// The caller's bucket label (`user:<id>`, `system:<id>` or `anonymous`).
    pub caller_bucket: String,
    /// The verb the call saw, as its upper-case code.
    pub verb: Option<&'static str>,
    /// The serialized arguments, for identity checks only.
    pub args: String,
}

/// A handler that records its calls and answers a fixed JSON value.
#[derive(Clone)]
pub struct FakeHandler {
    observed: Arc<Mutex<Vec<Observed>>>,
}

impl FakeHandler {
    /// A fake handler recording into its own shared log; the log comes back so the test keeps
    /// reading it after the registry holds the handler.
    pub fn new() -> (FakeHandler, Arc<Mutex<Vec<Observed>>>) {
        let observed = Arc::new(Mutex::new(Vec::new()));
        let handler = FakeHandler {
            observed: observed.clone(),
        };
        (handler, observed)
    }
}

impl FunctionHandler for FakeHandler {
    fn call<'a>(
        &'a self,
        ctx: &'a Context,
        args: serde_json::Value,
    ) -> spot_functions::HandlerFuture<'a> {
        let observed = self.observed.clone();
        Box::pin(async move {
            observed.lock().unwrap().push(Observed {
                tenant: ctx.tenant.clone(),
                caller_bucket: ctx.caller.bucket(),
                verb: ctx.verb.map(spot_functions::verb_name),
                args: args.to_string(),
            });
            Ok(serde_json::json!({ "ok": true }))
        })
    }
}

/// A handler that always fails with the given `AppError`.
pub struct FailingHandler(pub AppError);

impl FunctionHandler for FailingHandler {
    fn call<'a>(
        &'a self,
        _ctx: &'a Context,
        _args: serde_json::Value,
    ) -> spot_functions::HandlerFuture<'a> {
        let error = self.0.clone();
        Box::pin(async move { Err(error) })
    }
}

/// A manual clock for the limiter tests: monotonic time moves only when `advance`d.
/// `spot_core`'s own `ManualClock` does exactly this, so the tests drive it behind an `Arc`;
/// the wall start is irrelevant to the limiter (it reads `monotonic` only).
pub fn manual_clock() -> Arc<spot_core::clock::ManualClock> {
    Arc::new(spot_core::clock::ManualClock::new(
        SystemClock::new().now_utc(),
    ))
}

/// The real clock, for builds that do not exercise the limiter.
pub fn system_clock() -> Arc<dyn Clock> {
    Arc::new(SystemClock::new())
}

/// The key of the suite's canonical function: `app1/mod1/fn1`.
pub fn ok_key() -> spot_functions::FunctionKey {
    key("app1", "mod1", "fn1")
}

/// A key under `app` with the given module and name parts.
pub fn key(app: &str, module: &str, name: &str) -> spot_functions::FunctionKey {
    match spot_functions::FunctionKey::parse(app, module, name) {
        Ok(key) => key,
        Err(InvalidFunctionKey { key, .. }) => panic!("test key is not valid: {key}"),
    }
}

/// The empty arguments object, the suite's default call payload.
pub fn json_args() -> serde_json::Value {
    serde_json::json!({})
}

/// A signed-in caller with a user id, as most tests call.
pub fn user(id: &str) -> Caller {
    Caller::User(id.to_owned())
}

/// A `Context` for the tenant `acme`.
pub fn ctx_for(caller: Caller, verb: Option<spot_functions::HttpVerb>) -> Context {
    Context {
        caller,
        verb,
        tenant: "acme".to_owned(),
        command_id: None,
    }
}
