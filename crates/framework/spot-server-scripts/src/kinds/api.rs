//! API scripts: a TypeScript function served as the whitelisted function
//! `scripting/<module>/<function>`, run as the function's caller.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde_json::{Value as Json, json};
use spot_app_api::prelude::{Clock, CommandId, SiteId};
use spot_app_api::{Actor, AppError, AppResult, ErrorClass, Invocation};
use spot_functions::{
    Caller, Context, FunctionDecl, FunctionHandler, FunctionKey, HandlerFuture, codes as fn_codes,
    validate_arguments,
};
use spot_money::currency::CurrencyTable;
use spot_ts::{Compiled, ScriptEngine};

use super::ScriptKind;
use super::exec::{Execution, execute, prepare};
use super::record::{ApiScript, SCRIPTING_APP};
use super::store::{KindRun, KindStore};
use crate::access::ScriptAccessFactory;
use crate::codes;
use crate::elevation::{ElevatedKind, Elevation, ElevationSession, ElevationWiring, RunIdentity};
use crate::limits::ScriptLimits;
use crate::view::parse_record_id;

/// What the API and scheduled script runners share: the engine, how to reach documents, the
/// injected clock, where runs are recorded and the limits every run is held to.
#[derive(Clone)]
pub struct KindDeps {
    /// The engine every script runs on (a new runtime per execution).
    pub engine: ScriptEngine,
    /// Opens the document access of one run, acting as the run's user.
    pub access: Arc<dyn ScriptAccessFactory>,
    /// The only clock a run sees (its time is an argument, never read ambiently).
    pub clock: Arc<dyn Clock>,
    /// Where runs are recorded.
    pub store: Arc<dyn KindStore>,
    /// The limits every script runs under.
    pub limits: ScriptLimits,
    /// The currency table for `host.format_money`, when the host has one.
    pub currencies: Option<Arc<CurrencyTable>>,
}

/// Two or more scripts claiming one function key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyConflict {
    /// The key they claim: `scripting/<module>/<function>`.
    pub key: String,
    /// The names of the scripts claiming it, sorted.
    pub scripts: Vec<String>,
}

/// One API script as a [`FunctionHandler`].
pub struct ApiScriptHandler {
    script: ApiScript,
    deps: KindDeps,
    compiled: Mutex<Option<Arc<Compiled>>>,
    elevation: Option<ElevationWiring>,
}

impl ApiScriptHandler {
    /// A handler for `script`. The source is transpiled and compiled on the first call and kept
    /// until the handler is dropped (a saved change builds a new handler).
    pub fn new(script: ApiScript, deps: KindDeps) -> ApiScriptHandler {
        ApiScriptHandler {
            script,
            deps,
            compiled: Mutex::new(None),
            elevation: None,
        }
    }

    /// The same handler able to run an elevated script as the platform (default deny: without
    /// this a flagged script runs as its caller).
    #[must_use]
    pub fn with_elevation(mut self, wiring: ElevationWiring) -> ApiScriptHandler {
        self.elevation = Some(wiring);
        self
    }

    /// The script the handler runs.
    pub fn script(&self) -> &ApiScript {
        &self.script
    }

    fn compiled(&self) -> AppResult<Arc<Compiled>> {
        {
            let cached = self
                .compiled
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(compiled) = cached.as_ref() {
                return Ok(Arc::clone(compiled));
            }
        }
        let compiled = prepare(
            &self.deps.engine,
            ScriptKind::Api,
            &self.script.name,
            &self.script.source,
        )?;
        *self
            .compiled
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::clone(&compiled));
        Ok(compiled)
    }

    async fn run(&self, ctx: &Context, args: Json) -> AppResult<Json> {
        let script = &self.script;
        // 1. Who: an API script runs as its caller, never as a guest and never as the platform.
        let (user_text, user_id) = match &ctx.caller {
            Caller::User(text) => match parse_record_id(text) {
                Some(id) => (text.clone(), id),
                None => return Err(guest_refused(script)),
            },
            Caller::Anonymous | Caller::System(_) => return Err(guest_refused(script)),
        };
        // 2. The arguments, before any engine work.
        if let Some(schema) = &script.arguments_schema {
            validate_arguments(schema, &args).map_err(|e| {
                AppError::new(
                    ErrorClass::Validation,
                    fn_codes::BAD_ARGUMENTS,
                    format!("`{}` rejected its arguments", script.key()),
                )
                .explained(e.to_string())
            })?;
        }
        // 3. The invocation: the caller's identity at the injected clock's time.
        let site = SiteId::new(ctx.tenant.as_str()).map_err(|_| {
            AppError::new(
                ErrorClass::Internal,
                codes::STORE_FAILED,
                "The call's tenant is not a valid site id.",
            )
        })?;
        let invocation = Invocation {
            site,
            actor: Actor::User {
                id: user_id,
                via: None,
            },
            command_id: ctx
                .command_id
                .as_deref()
                .and_then(|text| CommandId::new(text).ok()),
            language: None,
            now: self.deps.clock.now_utc(),
        };
        // 4. Run it.
        let compiled = self.compiled()?;
        let entry_args = json!({
            "args": args,
            "ctx": {
                "function": script.key(),
                "user": user_text,
                "tenant": ctx.tenant,
            },
        });
        let proof = Elevation::granted(script);
        let mut session = match (proof, &self.elevation) {
            (Some(proof), Some(wiring)) => Some(
                ElevationSession::begin(
                    wiring,
                    proof,
                    RunIdentity {
                        script: script.name.clone(),
                        kind: ElevatedKind::Api,
                        doctype: None,
                        operation: "call".to_owned(),
                        triggering_user: user_text.clone(),
                    },
                    &invocation,
                )
                .await?,
            ),
            _ => None,
        };
        let run = Execution {
            engine: &self.deps.engine,
            compiled: &compiled,
            entry: ScriptKind::Api.entry(),
            args: &entry_args,
            limits: self.deps.limits.engine_limits_for_ms(script.timeout_ms),
            name: &script.name,
            currencies: self.deps.currencies.clone(),
        };
        let executed = match session.as_mut() {
            Some(session) => execute(run, session.access()).await,
            None => {
                let mut access = self.deps.access.open(invocation);
                execute(run, access.as_mut()).await
            }
        };
        let code = match &executed.result {
            Ok(_) => "ok".to_owned(),
            Err(error) => error.code.clone(),
        };
        let run = KindRun {
            script: script.name.clone(),
            kind: ScriptKind::Api,
            ok: executed.result.is_ok(),
            code,
            duration_ms: executed.duration_ms,
            host_calls: executed.host_calls,
            at: self.deps.clock.now_utc(),
            user: user_text,
            message: executed.message,
        };
        let _ = self.deps.store.record_run(run).await;
        if let Some(session) = session {
            let outcome = match &executed.result {
                Ok(_) => "ok".to_owned(),
                Err(error) => error.code.clone(),
            };
            // No audit, no elevated run: a sink that cannot record fails the run.
            session.finish(&outcome).await?;
        }
        executed.result
    }
}

impl FunctionHandler for ApiScriptHandler {
    fn call<'a>(&'a self, ctx: &'a Context, args: Json) -> HandlerFuture<'a> {
        Box::pin(async move { self.run(ctx, args).await })
    }
}

fn guest_refused(script: &ApiScript) -> AppError {
    AppError::new(
        ErrorClass::PermissionDenied,
        codes::GUEST_REFUSED,
        "This function runs as the signed-in caller; sign in to call it.",
    )
    .detail("script", script.name.as_str())
}

/// Builds the function declarations of a set of API scripts.
pub struct ApiScriptRegistry;

impl ApiScriptRegistry {
    /// The keys claimed by more than one enabled script, sorted by key.
    pub fn conflicts(scripts: &[ApiScript]) -> Vec<KeyConflict> {
        let mut by_key: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for script in scripts.iter().filter(|s| s.enabled) {
            by_key
                .entry(script.key())
                .or_default()
                .push(script.name.clone());
        }
        by_key
            .into_iter()
            .filter(|(_, names)| names.len() > 1)
            .map(|(key, mut scripts)| {
                scripts.sort();
                KeyConflict { key, scripts }
            })
            .collect()
    }

    /// The function declarations the host registers under app `scripting`, one per enabled
    /// script, sorted by key. A declaration is `guest: false`, with the script's verbs and
    /// argument schema, and no rate limit (the host sets its default).
    ///
    /// # Errors
    ///
    /// `serverscript.function_conflict` when two enabled scripts claim one key (the message
    /// names every script claiming it); `serverscript.record_invalid` when a script's module or
    /// function is not a storage name (a record read from a document cannot be).
    pub fn build(
        scripts: &[ApiScript],
        deps: &KindDeps,
    ) -> AppResult<Vec<(FunctionDecl, Arc<dyn FunctionHandler>)>> {
        Self::build_with(scripts, deps, None)
    }

    /// [`ApiScriptRegistry::build`] with the host's elevation wiring: a script whose record says
    /// it is elevated then runs as the platform (audited); without it, it runs as its caller.
    ///
    /// # Errors
    ///
    /// As [`ApiScriptRegistry::build`].
    pub fn build_with(
        scripts: &[ApiScript],
        deps: &KindDeps,
        elevation: Option<&ElevationWiring>,
    ) -> AppResult<Vec<(FunctionDecl, Arc<dyn FunctionHandler>)>> {
        if let Some(conflict) = Self::conflicts(scripts).into_iter().next() {
            let names = conflict
                .scripts
                .iter()
                .map(|name| format!("`{name}`"))
                .collect::<Vec<_>>()
                .join(" and ");
            return Err(AppError::new(
                ErrorClass::Duplicate,
                codes::FUNCTION_CONFLICT,
                format!(
                    "The scripts {names} both claim the function `{}`.",
                    conflict.key
                ),
            )
            .detail("function", conflict.key.as_str()));
        }
        let mut out: Vec<(FunctionDecl, Arc<dyn FunctionHandler>)> = Vec::new();
        for script in scripts.iter().filter(|s| s.enabled) {
            let key = FunctionKey::parse(SCRIPTING_APP, &script.module, &script.function).map_err(
                |_| {
                    AppError::new(
                        ErrorClass::Validation,
                        codes::RECORD_INVALID,
                        format!(
                            "The module or function of the script `{}` is not a storage name.",
                            script.name
                        ),
                    )
                },
            )?;
            let decl = FunctionDecl {
                key,
                verbs: Some(script.verbs.verbs()),
                guest: false,
                limit: None,
                arguments: script.arguments_schema.clone(),
            };
            let mut built = ApiScriptHandler::new(script.clone(), deps.clone());
            if let Some(wiring) = elevation {
                built = built.with_elevation(wiring.clone());
            }
            let handler: Arc<dyn FunctionHandler> = Arc::new(built);
            out.push((decl, handler));
        }
        out.sort_by(|a, b| a.0.key.cmp(&b.0.key));
        Ok(out)
    }
}
