//! `POST /mgmt/v1/commands`: the signed-command intake. The order of the steps is fixed and
//! tested:
//!
//! 1. the client certificate (verified by the TLS layer) is mapped to an operator on the
//!    allow-list (`mgmt.unauthenticated`, `mgmt.forbidden`);
//! 2. the body size limit, before any parse (`mgmt.body_too_large`);
//! 3. the envelope is parsed (`mgmt.signature_invalid`);
//! 4. signature, domain, audience and expiry are verified and the nonce is consumed in the durable
//!    store, last, by the signed-command verifier (`mgmt.signature_*`, `mgmt.replayed`; a store
//!    error is `mgmt.internal`: never proceed);
//! 5. the operation is looked up and the parameters validated (`mgmt.unknown_operation`,
//!    `mgmt.forbidden`, `mgmt.invalid_parameter`);
//! 6. risk requirements: approvals (`mgmt.approval_required`);
//! 7. the per-site operation lock (`mgmt.conflict`, `mgmt.rate_limited`);
//! 8. the command is stored and, once its acceptance is audited, run on a spawned task and answered
//!    with `202`.
//!
//! A `read` operation needs the certificate only: it may omit signatures (then no nonce is
//! consumed); an operation that is not in the catalogue is treated as needing a signature, so an
//! unsigned request cannot probe the catalogue.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::http::header::CONTENT_LENGTH;
use axum::http::request::Parts;
use axum::response::Response;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use spot_mgmt_ops::{OpId, OperationSpec, Risk};
use spot_signed_command::CommandError;
use tokio::sync::watch;

use crate::app::{
    AppState, Launch, MAX_BODY_BYTES, MAX_CLOCK_SKEW_SECS, MAX_COMMAND_LIFETIME_SECS, RunningEntry,
    Success, Trace, finish,
};
use crate::error::{MgmtCode, MgmtError};
use crate::handlers::{Handler, HandlerContext, HandlerRequest};
use crate::lifecycle::{
    CommandRecord, CommandState, LockDenied, LockGuard, MAX_RESULT_BYTES, StoreError,
};
use crate::ports::{ApprovalRequest, AuditOutcome, AuditRecord};
use crate::view::{invalid_parameter, invocation_from_params};
use crate::wire::{self, WireEnvelope};

/// The route handler.
pub(crate) async fn invoke(State(app): State<Arc<AppState>>, request: Request) -> Response {
    let mut trace = Trace::new(&app, "mgmt.command.invoke");
    let (parts, body) = request.into_parts();
    let result = invoke_inner(&app, &mut trace, &parts, body).await;
    finish(&app, trace, result).await
}

/// The signed-command verifier's error as the API's.
fn map_command_error(error: &CommandError) -> MgmtError {
    match error {
        CommandError::Replay => MgmtError::with_reason(MgmtCode::Replayed, "mgmt.replayed"),
        CommandError::Store(_) => {
            MgmtError::with_reason(MgmtCode::Internal, "mgmt.nonce_store_unavailable")
        }
        CommandError::InvalidKey => MgmtError::new(MgmtCode::Internal),
        CommandError::NotYetValid | CommandError::Expired | CommandError::LifetimeExceeded => {
            MgmtError::with_reason(MgmtCode::SignatureExpired, "mgmt.signature_expired")
        }
        _ => MgmtError::with_reason(MgmtCode::SignatureInvalid, "mgmt.signature_invalid"),
    }
}

async fn read_body(parts: &Parts, body: Body) -> Result<Vec<u8>, MgmtError> {
    let too_large = || MgmtError::with_reason(MgmtCode::BodyTooLarge, "mgmt.body_too_large");
    if let Some(length) = parts
        .headers
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|text| text.parse::<usize>().ok())
        && length > MAX_BODY_BYTES
    {
        return Err(too_large());
    }
    let bytes = axum::body::to_bytes(body, MAX_BODY_BYTES)
        .await
        .map_err(|_| too_large())?;
    Ok(bytes.to_vec())
}

/// Reads a JSON body of at most 64 KiB (validate endpoint).
pub(crate) async fn read_json(parts: &Parts, body: Body) -> Result<Value, MgmtError> {
    let bytes = read_body(parts, body).await?;
    serde_json::from_slice(&bytes).map_err(|_| MgmtError::parameter("body"))
}

fn idempotency_key(parts: &Parts) -> Result<String, MgmtError> {
    let value = parts
        .headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| MgmtError::parameter("Idempotency-Key"))?;
    let valid = !value.is_empty()
        && value.len() <= 128
        && value.bytes().all(|byte| (0x21..=0x7e).contains(&byte));
    if valid {
        Ok(value.to_owned())
    } else {
        Err(MgmtError::parameter("Idempotency-Key"))
    }
}

fn digest_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

fn id_is_sane(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

/// The window check the verifier applies, for unsigned `read` commands.
fn check_unsigned(app: &AppState, envelope: &WireEnvelope, now: i64) -> Result<(), MgmtError> {
    let wire = &envelope.command;
    if wire.version != 1 || !id_is_sane(&wire.id) {
        return Err(MgmtError::with_reason(
            MgmtCode::SignatureInvalid,
            "mgmt.signature_invalid",
        ));
    }
    if wire.audience != app.site_id {
        return Err(MgmtError::with_reason(
            MgmtCode::SignatureInvalid,
            "mgmt.wrong_audience",
        ));
    }
    let expired = now < wire.issued_at - MAX_CLOCK_SKEW_SECS
        || now >= wire.expires_at
        || wire.expires_at - wire.issued_at > MAX_COMMAND_LIFETIME_SECS;
    if expired {
        return Err(MgmtError::with_reason(
            MgmtCode::SignatureExpired,
            "mgmt.signature_expired",
        ));
    }
    Ok(())
}

async fn invoke_inner(
    app: &Arc<AppState>,
    trace: &mut Trace,
    parts: &Parts,
    body: Body,
) -> Result<Success, MgmtError> {
    // (a) certificate and operator allow-list.
    let operator = app.authenticate(trace, parts)?;
    // (b) body size, before parse.
    let bytes = read_body(parts, body).await?;
    // (c) parse.
    let key = idempotency_key(parts)?;
    let envelope = wire::decode(&bytes).map_err(|_| {
        MgmtError::with_reason(MgmtCode::SignatureInvalid, "mgmt.signature_invalid")
    })?;
    let op_id = OpId::new(&envelope.command.op);
    trace.operation = op_id.as_ref().map(|id| id.as_str().to_owned());
    let spec = op_id.as_ref().and_then(|id| app.catalogue.get(id));
    trace.risk = spec.map(|spec| spec.risk);

    // A retransmission of the same request returns the first answer.
    let digest = digest_hex(&bytes);
    let previous = app
        .commands
        .find_by_idempotency(&operator.principal, &key)
        .await
        .map_err(|_| {
            MgmtError::with_reason(MgmtCode::Internal, "mgmt.command_store_unavailable")
        })?;
    if let Some(previous) = previous {
        if previous.body_digest == digest {
            trace.command_id = Some(previous.id.clone());
            return Ok(accepted(&previous.id, None));
        }
        return Err(MgmtError::with_reason(
            MgmtCode::Replayed,
            "mgmt.idempotency_key_reused",
        ));
    }

    // (d) + (e) signature, domain, audience, expiry, then the nonce (the verifier consumes it last).
    let now = app.now();
    let needs_signature =
        spec.is_none_or(|spec| spec.risk > Risk::Read) || !envelope.signatures.is_empty();
    let command_id;
    let params: Value;
    if needs_signature {
        let signed = envelope.to_signed(&app.keys).map_err(|_| {
            MgmtError::with_reason(MgmtCode::SignatureInvalid, "mgmt.signature_invalid")
        })?;
        let verified = app
            .verifier
            .verify(&signed, &app.site_id, now)
            .await
            .map_err(|error| map_command_error(&error))?;
        command_id = verified.command.id().to_owned();
        params = verified.command.params().clone();
    } else {
        check_unsigned(app, &envelope, now)?;
        command_id = envelope.command.id.clone();
        params = envelope.command.params.clone();
    }
    trace.command_id = Some(command_id.clone());

    // (f) the operation and its parameters.
    let (Some(op_id), Some(spec)) = (op_id, spec) else {
        return Err(MgmtError::with_reason(
            MgmtCode::UnknownOperation,
            "mgmt.unknown_operation",
        ));
    };
    if !operator.scope.allows(op_id.as_str()) {
        return Err(MgmtError::with_reason(
            MgmtCode::Forbidden,
            "mgmt.operation_not_permitted",
        ));
    }
    let invocation = invocation_from_params(spec, &op_id, &params)?;
    let validated = app
        .catalogue
        .validate(&invocation)
        .map_err(|problems| invalid_parameter(&problems))?;

    // (g) risk requirements.
    check_approvals(app, spec, &envelope.approvals, &command_id).await?;

    let registered = app.handlers.get(op_id.as_str()).cloned().ok_or_else(|| {
        MgmtError::with_reason(MgmtCode::NotAvailableHere, "mgmt.not_available_here")
    })?;

    // (h) the per-site lock.
    let guard =
        app.locks
            .try_acquire(spec.risk == Risk::Critical)
            .map_err(|denied| match denied {
                LockDenied::Conflict => MgmtError::with_reason(MgmtCode::Conflict, "mgmt.conflict"),
                LockDenied::Saturated => {
                    MgmtError::with_reason(MgmtCode::RateLimited, "mgmt.too_many_commands")
                }
            })?;
    let record = CommandRecord {
        id: command_id.clone(),
        operation: op_id.as_str().to_owned(),
        principal: operator.principal.clone(),
        state: CommandState::Accepted,
        progress: Vec::new(),
        result: None,
        error_code: None,
        audit_ref: String::new(),
        created_at: now,
        finished_at: None,
        idempotency_key: key,
        body_digest: digest,
        cancellable: registered.cancellable,
    };
    app.commands
        .insert(record, now)
        .await
        .map_err(|error| match error {
            StoreError::Duplicate => {
                MgmtError::with_reason(MgmtCode::Replayed, "mgmt.command_id_reused")
            }
            StoreError::Unavailable => {
                MgmtError::with_reason(MgmtCode::Internal, "mgmt.command_store_unavailable")
            }
        })?;

    // (i) the run, started by `finish` once the acceptance is audited.
    let (cancel_tx, cancel_rx) = watch::channel(false);
    if let Ok(mut running) = app.running.lock() {
        running.insert(
            command_id.clone(),
            RunningEntry {
                cancel: cancel_tx,
                cancellable: registered.cancellable,
            },
        );
    }
    let request = HandlerRequest {
        command_id: command_id.clone(),
        principal: operator.principal.clone(),
        operation: op_id.clone(),
        params: validated,
    };
    let context = HandlerContext::new(
        command_id.clone(),
        Arc::clone(&app.commands),
        cancel_rx.clone(),
        Arc::clone(&app.clock),
    );
    let job = RunJob {
        app: Arc::clone(app),
        id: command_id.clone(),
        handler: registered.handler,
        context,
        request,
        cancel: cancel_rx,
        request_id: trace.request_id.clone(),
        principal: operator.principal.clone(),
        fingerprint: trace.fingerprint.clone(),
        operation: op_id.as_str().to_owned(),
        risk: spec.risk,
    };
    let abort_app = Arc::clone(app);
    let abort_id = command_id.clone();
    let launch = Launch {
        guard,
        run: Box::new(move |audit_ref, guard| {
            Box::pin(async move {
                tokio::spawn(run_command(job, audit_ref, guard));
            })
        }),
        abort: Box::new(move || {
            Box::pin(async move {
                let now = abort_app.now();
                let _ = abort_app
                    .commands
                    .finish(
                        &abort_id,
                        CommandState::Failed,
                        None,
                        Some(MgmtCode::Internal.as_str().to_owned()),
                        now,
                    )
                    .await;
                if let Ok(mut running) = abort_app.running.lock() {
                    running.remove(&abort_id);
                }
            })
        }),
    };
    Ok(accepted(&command_id, Some(launch)))
}

/// Everything the spawned run needs.
struct RunJob {
    app: Arc<AppState>,
    id: String,
    handler: Arc<dyn Handler>,
    context: HandlerContext,
    request: HandlerRequest,
    cancel: watch::Receiver<bool>,
    request_id: String,
    principal: String,
    fingerprint: Option<String>,
    operation: String,
    risk: Risk,
}

/// A handler's error code is echoed in the status: only a registry-shaped code is.
fn code_is_valid(code: &str) -> bool {
    let mut segments = 0;
    for segment in code.split('.') {
        segments += 1;
        let mut bytes = segment.bytes();
        let first_ok = bytes.next().is_some_and(|b| b.is_ascii_lowercase());
        if !first_ok || !bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_') {
            return false;
        }
    }
    segments >= 2 && code.len() <= 100
}

async fn run_command(job: RunJob, audit_ref: String, guard: LockGuard) {
    let RunJob {
        app,
        id,
        handler,
        context,
        request,
        cancel: _cancel,
        request_id,
        principal,
        fingerprint,
        operation,
        risk,
    } = job;
    let _ = app.commands.start(&id, &audit_ref).await;
    let joined = tokio::spawn(handler.call(context, request)).await;
    let internal = MgmtCode::Internal.as_str();
    let (state, result, error_code) = match joined {
        Ok(Ok(value)) => {
            let size = serde_json::to_vec(&value).map_or(usize::MAX, |bytes| bytes.len());
            if size > MAX_RESULT_BYTES {
                (CommandState::Failed, None, Some(internal.to_owned()))
            } else {
                (CommandState::Succeeded, Some(value), None)
            }
        }
        Ok(Err(error)) => {
            let code = if code_is_valid(error.code) {
                error.code
            } else {
                internal
            };
            let state = if error.cancelled {
                CommandState::Cancelled
            } else {
                CommandState::Failed
            };
            (state, None, Some(code.to_owned()))
        }
        Err(_) => (CommandState::Failed, None, Some(internal.to_owned())),
    };
    let now = app.now();
    let outcome = match state {
        CommandState::Succeeded => AuditOutcome::Succeeded,
        CommandState::Cancelled => AuditOutcome::Cancelled,
        _ => AuditOutcome::Failed,
    };
    let reason = error_code.clone();
    let _ = app
        .commands
        .finish(&id, state, result, error_code, now)
        .await;
    if let Ok(mut running) = app.running.lock() {
        running.remove(&id);
    }
    let _ = app
        .audit
        .record(AuditRecord {
            request_id,
            principal: Some(principal),
            certificate_fingerprint: fingerprint,
            action: operation.clone(),
            operation: Some(operation),
            risk: Some(risk),
            outcome,
            reason,
            command_id: Some(id),
            at: now,
        })
        .await;
    drop(guard);
}

fn accepted(command_id: &str, launch: Option<Launch>) -> Success {
    Success {
        status: StatusCode::ACCEPTED,
        body: json!({ "data": { "command_id": command_id } }),
        launch,
    }
}

async fn check_approvals(
    app: &AppState,
    spec: &OperationSpec,
    approvals: &[String],
    command_id: &str,
) -> Result<(), MgmtError> {
    let kinds = app.approvals.required(spec);
    if kinds.is_empty() {
        return Ok(());
    }
    let refused = || MgmtError::with_reason(MgmtCode::ApprovalRequired, "mgmt.approval_required");
    if approvals.is_empty() {
        return Err(refused());
    }
    let request = ApprovalRequest {
        operation: spec.id.as_str(),
        risk: spec.risk,
        kinds: &kinds,
        approvals,
        command_id,
    };
    app.approvals.verify(request).await.map_err(|()| refused())
}
