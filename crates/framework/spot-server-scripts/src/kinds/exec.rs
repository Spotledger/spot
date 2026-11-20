//! Running one entry point of an API or scheduled script on the engine, servicing its host
//! calls through a [`ScriptDocumentAccess`]. Shared by both kinds.

use std::sync::Arc;
use std::time::Instant;

use serde_json::Value as Json;
use spot_app_api::error::FieldPath;
use spot_app_api::{AppError, AppResult};
use spot_money::currency::CurrencyTable;
use spot_ts::{Compiled, HostBridge, Limits, ScriptEngine, ScriptError, ScriptId};
use spot_ts_transpile::Options;

use super::ScriptKind;
use crate::access::ScriptDocumentAccess;
use crate::codes;
use crate::controller::{now_millis, seed_for};
use crate::error::map_script_error;
use crate::host::{HostRequest, ScriptBridge, service, typed_throw_error};
use crate::record::FIELD_SCRIPT;

/// What one execution produced.
pub(crate) struct Executed {
    /// The script's result, or the app error it maps to (a typed throw wins over everything).
    pub(crate) result: Result<Json, AppError>,
    /// How many host calls the run made.
    pub(crate) host_calls: u32,
    /// The last message the script logged.
    pub(crate) message: Option<String>,
    /// How long the run took, in milliseconds.
    pub(crate) duration_ms: u64,
}

/// Transpiles and compiles `source` for `kind`, once. No script text appears in the error.
pub(crate) fn prepare(
    engine: &ScriptEngine,
    kind: ScriptKind,
    name: &str,
    source: &str,
) -> AppResult<Arc<Compiled>> {
    let options = Options {
        allow_imports: Vec::new(),
        ..Options::default()
    };
    let file = format!("{name}.ts");
    let transpiled =
        spot_ts_transpile::transpile(source, &file, &options).map_err(|diagnostics| {
            let (line, column) = diagnostics
                .first()
                .map_or((0, 0), |first| (first.line, first.column));
            AppError::field(
                FieldPath::field(FIELD_SCRIPT),
                codes::COMPILE_FAILED,
                format!("The script `{name}` did not compile (line {line}, column {column})."),
            )
            .detail("script", name)
        })?;
    engine
        .compile(
            &transpiled.js,
            ScriptId::new(kind.doctype(), name, 0),
            Some(&transpiled.source_map),
        )
        .map_err(|error| {
            AppError::field(
                FieldPath::field(FIELD_SCRIPT),
                codes::COMPILE_FAILED,
                format!("The script `{name}` did not compile: {error}"),
            )
            .detail("script", name)
        })
}

/// Everything one execution needs besides the access.
pub(crate) struct Execution<'a> {
    pub(crate) engine: &'a ScriptEngine,
    pub(crate) compiled: &'a Arc<Compiled>,
    pub(crate) entry: &'a str,
    pub(crate) args: &'a Json,
    pub(crate) limits: Limits,
    pub(crate) name: &'a str,
    pub(crate) currencies: Option<Arc<CurrencyTable>>,
}

/// Runs the entry, servicing every host call on this task through `access`, until the engine
/// finishes. The deterministic clock and seed come from the access's invocation.
pub(crate) async fn execute(run: Execution<'_>, access: &mut dyn ScriptDocumentAccess) -> Executed {
    let started = Instant::now();
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<HostRequest>();
    let now = now_millis(access.invocation());
    let bridge = Arc::new(ScriptBridge::new(
        sender,
        now,
        seed_for(run.name, now),
        run.currencies.clone(),
    ));
    let running = run.engine.run(
        run.compiled,
        run.entry,
        run.args,
        Arc::clone(&bridge) as Arc<dyn HostBridge>,
        run.limits,
    );
    tokio::pin!(running);
    let outcome: Result<Json, ScriptError> = loop {
        tokio::select! {
            finished = &mut running => break finished,
            request = receiver.recv() => {
                if let Some(request) = request {
                    let reply = service(&mut *access, &request.name, request.args).await;
                    let _ = request.reply.send(reply);
                }
            }
        }
    };
    let result = match bridge.typed_throw() {
        Some(thrown) => Err(typed_throw_error(&thrown, run.name)),
        None => outcome.map_err(|error| map_script_error(error, run.name)),
    };
    Executed {
        result,
        host_calls: bridge.host_calls(),
        message: bridge.last_message(),
        duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    }
}
