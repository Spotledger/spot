//! The built registry: the admin listing and the dispatcher.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value as Json;
use spot_app_api::error::{AppError, ErrorClass};
use spot_manifest::HttpVerb;

use crate::args::{Context, FunctionHandlerInfo};
use crate::builder::{Entry, FunctionInfo};
use crate::codes;
use crate::key::FunctionKey;
use crate::limit::RateLimiter;
use crate::schema::validate_arguments;

/// A validated, immutable registry of whitelisted functions. Build one with
/// [`FunctionRegistryBuilder`](crate::FunctionRegistryBuilder).
pub struct FunctionRegistry {
    entries: BTreeMap<FunctionKey, Entry>,
    limiter: RateLimiter,
}

impl FunctionRegistry {
    pub(crate) fn new(
        entries: BTreeMap<FunctionKey, Entry>,
        clock: Arc<dyn spot_core::clock::Clock>,
    ) -> Self {
        FunctionRegistry {
            entries,
            limiter: RateLimiter::new(clock),
        }
    }

    /// Replaces the limiter's clock (tests swap in a manual clock).
    pub fn set_clock(&self, clock: Arc<dyn spot_core::clock::Clock>) {
        self.limiter.set_clock(clock);
    }

    /// Every function, with its declaring app, verbs, guest flag, limit, schema, and who
    /// overrides it and why. Sorted by key: the admin listing is stable.
    pub fn list(&self) -> Vec<FunctionInfo> {
        self.entries
            .values()
            .map(|e| FunctionInfo {
                key: e.decl.key.clone(),
                app: e.app.clone(),
                verbs: e.decl.verbs.clone(),
                guest: e.decl.guest,
                limit: e.decl.limit,
                arguments: e.decl.arguments.clone(),
                overridden_by: e
                    .overridden_by
                    .as_ref()
                    .map(|(a, _, r)| (a.clone(), r.clone())),
            })
            .collect()
    }

    /// Drops every rate-limit bucket idle for longer than `older_than`, bounding memory;
    /// returns how many were dropped.
    pub fn evict_idle_buckets(&self, older_than: Duration) -> usize {
        self.limiter.evict_idle(older_than)
    }

    /// How many rate-limit buckets exist (hosts watch the memory bound).
    pub fn bucket_count(&self) -> usize {
        self.limiter.len()
    }

    /// The handler that runs for `key` (the override's, when one overrides), plus its
    /// declared limit. `None` when nobody declares the function.
    pub fn handler_info(&self, key: &FunctionKey) -> Option<FunctionHandlerInfo> {
        let e = self.entries.get(key)?;
        Some(match &e.overridden_by {
            Some((_, h, _)) => FunctionHandlerInfo {
                handler: h.clone(),
                limit: e.decl.limit,
            },
            None => FunctionHandlerInfo {
                handler: e.handler.clone(),
                limit: e.decl.limit,
            },
        })
    }

    /// Dispatches one call: resolves the function, checks the verb, the caller, the rate
    /// limit and the argument schema, then runs the handler the override table selects.
    ///
    /// Error codes, in the order a call can fail: `function.not_found` — also the answer to
    /// an anonymous caller asking for a non-guest function, who never learns the function
    /// exists; `function.verb_not_allowed` (details carry the `allow` set);
    /// `function.auth_required`; `function.rate_limited` (details carry `retry_after` in
    /// whole milliseconds); `function.bad_arguments` (the explanation names the schema
    /// problem without argument content). Handler errors pass through untouched.
    pub async fn dispatch(
        &self,
        ctx: &Context,
        key: &FunctionKey,
        args: Json,
    ) -> Result<Json, AppError> {
        // Existence first, guest-aware: an anonymous caller asking for a non-guest function
        // gets the same answer as for an unknown one, whatever verb they tried, so the
        // endpoint never reveals which functions exist.
        let Some(entry) = self.entries.get(key) else {
            return Err(not_found(key));
        };
        if ctx.caller.is_anonymous() && !entry.decl.guest {
            return Err(not_found(key));
        }
        // Verb check, guests and users alike: a declared HTTP function must be called with
        // one of its verbs (a call without a verb is a job, and only a function that does
        // not answer HTTP at all accepts it).
        let declared = entry.decl.verbs.as_ref();
        let verb_allowed = match ctx.verb {
            Some(verb) => declared.is_some_and(|verbs| verbs.contains(&verb)),
            None => declared.is_none(),
        };
        if !verb_allowed {
            return Err(AppError::new(
                ErrorClass::MethodNotFound,
                codes::VERB_NOT_ALLOWED,
                format!("`{key}` does not answer this verb"),
            )
            .detail("allow", allow_set(declared.map(Vec::as_slice))));
        }
        if ctx.caller.is_anonymous() {
            // A guest function over a verb that changes things still needs a signed-in
            // caller; a non-guest function never existed as far as a guest is concerned.
            if ctx.verb.is_some_and(write_verb) {
                return Err(AppError::new(
                    ErrorClass::PermissionDenied,
                    codes::AUTH_REQUIRED,
                    format!("`{key}` answers this verb only to a signed-in caller"),
                ));
            }
        }
        let (handler, limit) = match &entry.overridden_by {
            Some((_, h, _)) => (h.clone(), entry.decl.limit),
            None => (entry.handler.clone(), entry.decl.limit),
        };
        if let Some(limit) = limit {
            let bucket = (key.to_string(), ctx.caller.bucket());
            self.limiter
                .check(&bucket, limit.permits, limit.per)
                .map_err(|e| {
                    AppError::new(
                        ErrorClass::Unavailable,
                        codes::RATE_LIMITED,
                        format!("`{key}` is rate limited; retry later"),
                    )
                    .detail("retry_after", e.retry_after.as_millis() as u64)
                })?;
        }
        if let Some(schema) = &entry.decl.arguments {
            validate_arguments(schema, &args).map_err(|e| {
                AppError::new(
                    ErrorClass::Validation,
                    codes::BAD_ARGUMENTS,
                    format!("`{key}` rejected its arguments"),
                )
                .explained(e.to_string())
            })?;
        }
        handler.call(ctx, args).await
    }
}

/// The `function.not_found` error, worded so it says nothing about other functions.
fn not_found(key: &FunctionKey) -> AppError {
    AppError::new(
        ErrorClass::MethodNotFound,
        codes::NOT_FOUND,
        format!("no function `{key}`"),
    )
}

/// The `allow` detail of a verb-not-allowed error: the declared verbs in declared order, so
/// the transport can put the list straight into an `Allow` header; empty for a function that
/// answers no HTTP verbs at all.
fn allow_set(verbs: Option<&[HttpVerb]>) -> String {
    verbs.map_or(String::new(), |verbs| {
        verbs
            .iter()
            .map(|verb| verb_name(*verb))
            .collect::<Vec<_>>()
            .join(", ")
    })
}

/// The HTTP name of a verb.
fn verb_name(v: HttpVerb) -> &'static str {
    match v {
        HttpVerb::Get => "GET",
        HttpVerb::Post => "POST",
        HttpVerb::Put => "PUT",
        HttpVerb::Patch => "PATCH",
        HttpVerb::Delete => "DELETE",
        HttpVerb::Head => "HEAD",
        HttpVerb::Options => "OPTIONS",
    }
}

/// Whether a verb changes things: the verbs a guest may never use.
fn write_verb(v: HttpVerb) -> bool {
    matches!(
        v,
        HttpVerb::Post | HttpVerb::Put | HttpVerb::Patch | HttpVerb::Delete
    )
}
