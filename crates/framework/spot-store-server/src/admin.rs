//! The operator routes (`/admin/v1/...`, `/metrics`), outside the public contract.
//!
//! They sit behind a second bearer secret (the operator credential, kept as a SHA-256 and
//! compared in constant time) with its own rate class and its own failure counter. Approval of a
//! listing is a person's act: these routes are the only way a version becomes public. Every
//! action is audited, a refusal as well as a success, and every revocation reissues the signed
//! revocation list before the route answers.

use std::sync::Arc;

use axum::extract::{Extension, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use serde::Deserialize;
use serde_json::{Value, json};
use spot_bundle_signing::KeyId;
use spot_registry::{AppName, Digest, IndexStore as _, PublisherStatus, Version};
use spot_store_state::{
    AuditEntry, AuditKind, AuditRecord, BAD_TRANSITION, BundleRevocation, KEY_UNKNOWN,
    KeyRevocation, ListingState, ReviewStatus, ReviewUpdate,
};

use crate::auth::{ReqCtx, admin_check};
use crate::error::{ApiError, codes};
use crate::http::{
    data_response, parse_app, parse_version, read_json, read_json_or_default, set_header,
};
use crate::keys::{hash_key, new_publisher_token};
use crate::metrics::Metrics;
use crate::ops::{clip, rfc3339};
use crate::ratelimit::Class;
use crate::review::ReviewJob;
use crate::service::{Core, Service};

/// The actor named in the audit entries of operator actions.
const OPERATOR: &str = "operator";

/// `{"label": ...}`: what a new publisher key is for.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LabelBody {
    label: String,
}

/// `{"reason": ...}` of a revocation.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReasonBody {
    reason: String,
}

/// The optional reason of an approval, rejection or withdrawal.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct OptionalReason {
    #[serde(default)]
    reason: String,
}

/// The query of the audit read.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AuditParams {
    after: Option<u64>,
    limit: Option<u32>,
}

fn valid_publisher_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn reason_ok(reason: &str) -> Result<(), ApiError> {
    if reason.trim().is_empty() || reason.len() > 1024 {
        return Err(ApiError::invalid_admin("reason must be 1 to 1024 bytes"));
    }
    // The revocation scan reads the `refused:` prefix as "a refused attempt, revokes nothing",
    // so a real reason must not start with it.
    if reason.starts_with("refused:") {
        return Err(ApiError::invalid_admin(
            "reason must not start with \"refused:\"",
        ));
    }
    Ok(())
}

/// Counts the call against the operator class, checks the credential and counts a failure.
fn guard(service: &Service, ctx: &ReqCtx, headers: &HeaderMap) -> Result<(), ApiError> {
    let now = service.core.clock.now();
    let limited = |wait: u64| {
        Metrics::bump(&service.core.metrics.rate_limited);
        ApiError::rate_limited(wait)
    };
    service
        .limiter
        .check(Class::AuthFailure, &ctx.addr, now)
        .map_err(limited)?;
    service
        .limiter
        .hit(Class::Admin, &ctx.addr, now)
        .map_err(limited)?;
    admin_check(&service.core, headers).inspect_err(|_| {
        // The failure opens (or extends) the window that stops a guessing client.
        let _ = service.limiter.hit(Class::AuthFailure, &ctx.addr, now);
        Metrics::bump(&service.core.metrics.auth_failures);
    })
}

/// Audits a refused operator action (never a revocation: the `refused:` reason keeps the
/// revocation scan from reading it as one).
fn audit_refusal(
    core: &Core,
    kind: AuditKind,
    publisher: Option<&str>,
    app: Option<&AppName>,
    version: Option<&Version>,
    error: &ApiError,
    now: i64,
) {
    core.audit(&AuditEntry {
        kind,
        actor: OPERATOR.to_owned(),
        publisher_id: publisher.map(str::to_owned),
        app: app.cloned(),
        version: version.cloned(),
        digest: None,
        reason: clip(&format!("refused: {}", error.code), 512),
        at: now,
    });
}

// ---- operations (worker threads) -----------------------------------------------------------

fn issue_key_inner(
    core: &Core,
    publisher: &str,
    label: &str,
    now: i64,
) -> Result<(u64, String), ApiError> {
    if !valid_publisher_id(publisher) {
        return Err(ApiError::invalid_admin(
            "the publisher id must be 1 to 128 of letters, digits, _ and -",
        ));
    }
    if label.is_empty() || label.len() > 128 {
        return Err(ApiError::invalid_admin("label must be 1 to 128 bytes"));
    }
    let token = new_publisher_token();
    let key_id = core
        .state
        .issue_publisher_key(publisher, hash_key(&token), label, now)?;
    Ok((key_id, token))
}

fn issue_key_sync(
    core: &Core,
    publisher: &str,
    label: &str,
    now: i64,
) -> Result<(u64, String), ApiError> {
    let outcome = issue_key_inner(core, publisher, label, now);
    match &outcome {
        Ok(_) => core.audit(&AuditEntry {
            kind: AuditKind::KeyIssue,
            actor: OPERATOR.to_owned(),
            publisher_id: Some(publisher.to_owned()),
            app: None,
            version: None,
            digest: None,
            reason: clip(label, 512),
            at: now,
        }),
        Err(error) => audit_refusal(core, AuditKind::KeyIssue, None, None, None, error, now),
    }
    outcome
}

fn revoke_publisher_key_sync(core: &Core, key_id: u64, now: i64) -> Result<(), ApiError> {
    let outcome = core
        .state
        .revoke_publisher_key(key_id, now)
        .map_err(|error| {
            if error.code == KEY_UNKNOWN {
                ApiError::not_found("no such publisher key")
            } else {
                ApiError::from(error)
            }
        });
    match &outcome {
        Ok(()) => core.audit(&AuditEntry {
            kind: AuditKind::KeyRevoke,
            actor: OPERATOR.to_owned(),
            publisher_id: None,
            app: None,
            version: None,
            digest: None,
            reason: format!("key {key_id}"),
            at: now,
        }),
        Err(error) => audit_refusal(core, AuditKind::KeyRevoke, None, None, None, error, now),
    }
    outcome
}

fn revoke_publisher_inner(
    core: &Core,
    publisher: &str,
    reason: &str,
    now: i64,
) -> Result<u64, ApiError> {
    if !valid_publisher_id(publisher) {
        return Err(ApiError::invalid_admin(
            "the publisher id must be 1 to 128 of letters, digits, _ and -",
        ));
    }
    reason_ok(reason)?;
    // Suspend in the catalogue (a publisher that never published has no row there) and
    // revoke every key, so the next request of this publisher is refused.
    match core
        .index
        .set_publisher_status(publisher, PublisherStatus::Suspended)
    {
        Ok(()) => {}
        Err(error) if error.code == "registry.not_found" => {}
        Err(error) => return Err(error.into()),
    }
    for key in core.state.list_publisher_keys(publisher)? {
        if key.revoked_at.is_none() {
            core.state.revoke_publisher_key(key.key_id, now)?;
        }
    }
    core.audit_required(&AuditEntry {
        kind: AuditKind::RevokePublisher,
        actor: OPERATOR.to_owned(),
        publisher_id: Some(publisher.to_owned()),
        app: None,
        version: None,
        digest: None,
        reason: clip(reason, 1024),
        at: now,
    })?;
    core.revocations.reissue(now)
}

fn revoke_publisher_sync(
    core: &Core,
    publisher: &str,
    reason: &str,
    now: i64,
) -> Result<u64, ApiError> {
    let outcome = revoke_publisher_inner(core, publisher, reason, now);
    if let Err(error) = &outcome {
        audit_refusal(
            core,
            AuditKind::RevokePublisher,
            Some(publisher),
            None,
            None,
            error,
            now,
        );
    }
    outcome
}

fn revoke_signing_key_inner(
    core: &Core,
    key_id: &str,
    reason: &str,
    now: i64,
) -> Result<u64, ApiError> {
    if KeyId::parse(key_id).is_err() {
        return Err(ApiError::invalid_admin(
            "a signing key id is 32 lower-case hex digits",
        ));
    }
    reason_ok(reason)?;
    core.state.revoke_key(&KeyRevocation {
        key_id: key_id.to_owned(),
        reason: clip(reason, 1024),
        at: now,
    })?;
    core.audit_required(&AuditEntry {
        kind: AuditKind::RevokeKey,
        actor: OPERATOR.to_owned(),
        publisher_id: None,
        app: None,
        version: None,
        digest: None,
        reason: clip(reason, 1024),
        at: now,
    })?;
    core.revocations.reissue(now)
}

fn revoke_signing_key_sync(
    core: &Core,
    key_id: &str,
    reason: &str,
    now: i64,
) -> Result<u64, ApiError> {
    let outcome = revoke_signing_key_inner(core, key_id, reason, now);
    if let Err(error) = &outcome {
        audit_refusal(core, AuditKind::RevokeKey, None, None, None, error, now);
    }
    outcome
}

fn revoke_bundle_inner(core: &Core, digest: &str, reason: &str, now: i64) -> Result<u64, ApiError> {
    let digest: Digest = digest
        .to_ascii_lowercase()
        .parse()
        .map_err(|_| ApiError::invalid_admin("a digest is 64 hex digits"))?;
    reason_ok(reason)?;
    core.state.revoke_bundle(&BundleRevocation {
        digest,
        reason: clip(reason, 1024),
        at: now,
    })?;
    core.audit_required(&AuditEntry {
        kind: AuditKind::RevokeBundle,
        actor: OPERATOR.to_owned(),
        publisher_id: None,
        app: None,
        version: None,
        digest: Some(digest),
        reason: clip(reason, 1024),
        at: now,
    })?;
    core.revocations.reissue(now)
}

fn revoke_bundle_sync(core: &Core, digest: &str, reason: &str, now: i64) -> Result<u64, ApiError> {
    let outcome = revoke_bundle_inner(core, digest, reason, now);
    if let Err(error) = &outcome {
        audit_refusal(core, AuditKind::RevokeBundle, None, None, None, error, now);
    }
    outcome
}

fn listing_kind(to: ListingState) -> Option<AuditKind> {
    match to {
        ListingState::Listed => Some(AuditKind::Approve),
        ListingState::Rejected => Some(AuditKind::Reject),
        ListingState::Withdrawn => Some(AuditKind::Withdraw),
        ListingState::PendingReview => None,
    }
}

fn transition_inner(
    core: &Core,
    app: &AppName,
    version: &Version,
    to: ListingState,
    kind: AuditKind,
    reason: &str,
    now: i64,
) -> Result<(), ApiError> {
    if reason.len() > 1024 {
        return Err(ApiError::invalid_admin("reason must be at most 1024 bytes"));
    }
    let release = core
        .index
        .release(app, version)?
        .ok_or_else(|| ApiError::not_found("no such version"))?;
    match core.state.set_listing(app, version, to, OPERATOR, now) {
        Ok(()) => {}
        Err(error) if error.code == BAD_TRANSITION => {
            return Err(ApiError::server(
                StatusCode::CONFLICT,
                codes::BAD_STATE,
                "the listing state does not allow that transition",
            ));
        }
        Err(error) => return Err(error.into()),
    }
    core.audit_required(&AuditEntry {
        kind,
        actor: OPERATOR.to_owned(),
        publisher_id: Some(release.publisher),
        app: Some(app.clone()),
        version: Some(version.clone()),
        digest: Some(release.digest),
        reason: clip(reason, 1024),
        at: now,
    })
}

fn transition_sync(
    core: &Core,
    app: &AppName,
    version: &Version,
    to: ListingState,
    reason: &str,
    now: i64,
) -> Result<(), ApiError> {
    let Some(kind) = listing_kind(to) else {
        return Err(ApiError::invalid_admin(
            "a version cannot be sent back to review",
        ));
    };
    let outcome = transition_inner(core, app, version, to, kind, reason, now);
    if let Err(error) = &outcome {
        audit_refusal(core, kind, None, Some(app), Some(version), error, now);
    }
    outcome
}

fn rerun_sync(core: &Core, review_id: &str, now: i64) -> Result<(), ApiError> {
    let record = core
        .state
        .get_review(review_id)?
        .ok_or_else(|| ApiError::not_found("no such review"))?;
    core.state.update_review(
        review_id,
        &ReviewUpdate {
            status: ReviewStatus::Queued,
            verdict: None,
            summary: None,
            error_code: None,
            now,
        },
    )?;
    core.reviewer
        .enqueue(ReviewJob {
            review_id: review_id.to_owned(),
            digest: record.digest,
        })
        .map_err(|()| {
            ApiError::server(
                StatusCode::SERVICE_UNAVAILABLE,
                codes::REVIEW_UNAVAILABLE,
                "the review queue is full",
            )
        })
}

fn audit_json(record: &AuditRecord) -> Result<Value, ApiError> {
    let entry = &record.entry;
    Ok(json!({
        "id": record.id,
        "kind": entry.kind.as_str(),
        "actor": entry.actor,
        "publisher_id": entry.publisher_id,
        "app": entry.app.as_ref().map(AppName::as_str),
        "version": entry.version.as_ref().map(ToString::to_string),
        "digest": entry.digest.map(|digest| digest.to_hex()),
        "reason": entry.reason,
        "at": rfc3339(entry.at)?,
    }))
}

// ---- handlers --------------------------------------------------------------------------------

/// `POST /admin/v1/publishers/{id}/keys`: issues a publisher key and shows it once.
pub(crate) async fn issue_key(
    State(service): State<Arc<Service>>,
    Extension(ctx): Extension<ReqCtx>,
    headers: HeaderMap,
    Path(publisher): Path<String>,
    request: axum::extract::Request,
) -> Result<Response, ApiError> {
    guard(&service, &ctx, &headers)?;
    let body: LabelBody = read_json(
        &headers,
        request.into_body(),
        service.core.config.limits.max_json_bytes,
        true,
    )
    .await?;
    let now = service.core.clock.now();
    let core = Arc::clone(&service.core);
    let id = publisher.clone();
    let (key_id, token) = service
        .pool
        .call(move || issue_key_sync(&core, &id, &body.label, now))
        .await??;
    let mut response = data_response(
        StatusCode::CREATED,
        json!({ "publisher_id": publisher, "key_id": key_id, "key": token }),
    );
    set_header(&mut response, "cache-control", "no-store");
    Ok(response)
}

/// `POST /admin/v1/publisher-keys/{key_id}/revoke`.
pub(crate) async fn revoke_publisher_key(
    State(service): State<Arc<Service>>,
    Extension(ctx): Extension<ReqCtx>,
    headers: HeaderMap,
    Path(key_id): Path<u64>,
) -> Result<Response, ApiError> {
    guard(&service, &ctx, &headers)?;
    let now = service.core.clock.now();
    let core = Arc::clone(&service.core);
    service
        .pool
        .call(move || revoke_publisher_key_sync(&core, key_id, now))
        .await??;
    Ok(data_response(StatusCode::OK, json!({ "key_id": key_id })))
}

/// `POST /admin/v1/publishers/{id}/revoke`.
pub(crate) async fn revoke_publisher(
    State(service): State<Arc<Service>>,
    Extension(ctx): Extension<ReqCtx>,
    headers: HeaderMap,
    Path(publisher): Path<String>,
    request: axum::extract::Request,
) -> Result<Response, ApiError> {
    guard(&service, &ctx, &headers)?;
    let body: ReasonBody = read_json(
        &headers,
        request.into_body(),
        service.core.config.limits.max_json_bytes,
        true,
    )
    .await?;
    let now = service.core.clock.now();
    let core = Arc::clone(&service.core);
    let generation = service
        .pool
        .call(move || revoke_publisher_sync(&core, &publisher, &body.reason, now))
        .await??;
    Ok(data_response(
        StatusCode::OK,
        json!({ "generation": generation }),
    ))
}

/// `POST /admin/v1/signing-keys/{key_id}/revoke`.
pub(crate) async fn revoke_signing_key(
    State(service): State<Arc<Service>>,
    Extension(ctx): Extension<ReqCtx>,
    headers: HeaderMap,
    Path(key_id): Path<String>,
    request: axum::extract::Request,
) -> Result<Response, ApiError> {
    guard(&service, &ctx, &headers)?;
    let body: ReasonBody = read_json(
        &headers,
        request.into_body(),
        service.core.config.limits.max_json_bytes,
        true,
    )
    .await?;
    let now = service.core.clock.now();
    let core = Arc::clone(&service.core);
    let generation = service
        .pool
        .call(move || revoke_signing_key_sync(&core, &key_id, &body.reason, now))
        .await??;
    Ok(data_response(
        StatusCode::OK,
        json!({ "generation": generation }),
    ))
}

/// `POST /admin/v1/bundles/{digest}/revoke`.
pub(crate) async fn revoke_bundle(
    State(service): State<Arc<Service>>,
    Extension(ctx): Extension<ReqCtx>,
    headers: HeaderMap,
    Path(digest): Path<String>,
    request: axum::extract::Request,
) -> Result<Response, ApiError> {
    guard(&service, &ctx, &headers)?;
    let body: ReasonBody = read_json(
        &headers,
        request.into_body(),
        service.core.config.limits.max_json_bytes,
        true,
    )
    .await?;
    let now = service.core.clock.now();
    let core = Arc::clone(&service.core);
    let generation = service
        .pool
        .call(move || revoke_bundle_sync(&core, &digest, &body.reason, now))
        .await??;
    Ok(data_response(
        StatusCode::OK,
        json!({ "generation": generation }),
    ))
}

async fn transition(
    service: Arc<Service>,
    ctx: ReqCtx,
    headers: HeaderMap,
    (name, version): (String, String),
    request: axum::extract::Request,
    to: ListingState,
) -> Result<Response, ApiError> {
    guard(&service, &ctx, &headers)?;
    let app = parse_app(&name).map_err(|_| ApiError::invalid_admin("the app name is not valid"))?;
    let version = parse_version(&version)
        .map_err(|_| ApiError::invalid_admin("the version is not a semantic version"))?;
    let body: OptionalReason = read_json_or_default(
        &headers,
        request.into_body(),
        service.core.config.limits.max_json_bytes,
    )
    .await?;
    let now = service.core.clock.now();
    let core = Arc::clone(&service.core);
    let (app_for_job, version_for_job) = (app.clone(), version.clone());
    service
        .pool
        .call(move || transition_sync(&core, &app_for_job, &version_for_job, to, &body.reason, now))
        .await??;
    Ok(data_response(
        StatusCode::OK,
        json!({ "app": app.as_str(), "version": version.to_string(), "state": to.as_str() }),
    ))
}

/// `POST /admin/v1/apps/{name}/versions/{version}/approve`: `pending_review` to `listed`.
pub(crate) async fn approve(
    State(service): State<Arc<Service>>,
    Extension(ctx): Extension<ReqCtx>,
    headers: HeaderMap,
    Path(path): Path<(String, String)>,
    request: axum::extract::Request,
) -> Result<Response, ApiError> {
    transition(service, ctx, headers, path, request, ListingState::Listed).await
}

/// `POST /admin/v1/apps/{name}/versions/{version}/reject`: `pending_review` to `rejected`.
pub(crate) async fn reject(
    State(service): State<Arc<Service>>,
    Extension(ctx): Extension<ReqCtx>,
    headers: HeaderMap,
    Path(path): Path<(String, String)>,
    request: axum::extract::Request,
) -> Result<Response, ApiError> {
    transition(service, ctx, headers, path, request, ListingState::Rejected).await
}

/// `POST /admin/v1/apps/{name}/versions/{version}/withdraw`: `listed` to `withdrawn`.
pub(crate) async fn withdraw(
    State(service): State<Arc<Service>>,
    Extension(ctx): Extension<ReqCtx>,
    headers: HeaderMap,
    Path(path): Path<(String, String)>,
    request: axum::extract::Request,
) -> Result<Response, ApiError> {
    transition(
        service,
        ctx,
        headers,
        path,
        request,
        ListingState::Withdrawn,
    )
    .await
}

/// `POST /admin/v1/reviews/{id}/rerun`: queues a review again (after a failure or a restart).
pub(crate) async fn rerun_review(
    State(service): State<Arc<Service>>,
    Extension(ctx): Extension<ReqCtx>,
    headers: HeaderMap,
    Path(review_id): Path<String>,
) -> Result<Response, ApiError> {
    guard(&service, &ctx, &headers)?;
    let now = service.core.clock.now();
    let core = Arc::clone(&service.core);
    let id = review_id.clone();
    service
        .pool
        .call(move || rerun_sync(&core, &id, now))
        .await??;
    Ok(data_response(
        StatusCode::ACCEPTED,
        json!({ "review_id": review_id }),
    ))
}

/// `GET /admin/v1/audit?after=&limit=`: the audit log, oldest first.
pub(crate) async fn read_audit(
    State(service): State<Arc<Service>>,
    Extension(ctx): Extension<ReqCtx>,
    headers: HeaderMap,
    query: Result<Query<AuditParams>, axum::extract::rejection::QueryRejection>,
) -> Result<Response, ApiError> {
    guard(&service, &ctx, &headers)?;
    let Query(params) =
        query.map_err(|_| ApiError::invalid_admin("the query string is not valid"))?;
    let after = params.after.unwrap_or(0);
    let limit = params
        .limit
        .unwrap_or(100)
        .clamp(1, spot_store_state::MAX_AUDIT_PAGE);
    let core = Arc::clone(&service.core);
    let records = service
        .pool
        .call(move || core.state.read_audit(after, limit))
        .await?
        .map_err(ApiError::from)?;
    let mut entries = Vec::with_capacity(records.len());
    for record in &records {
        entries.push(audit_json(record)?);
    }
    Ok(data_response(StatusCode::OK, Value::Array(entries)))
}

/// `GET /metrics`: Prometheus text, behind the operator credential.
pub(crate) async fn metrics(
    State(service): State<Arc<Service>>,
    Extension(ctx): Extension<ReqCtx>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    guard(&service, &ctx, &headers)?;
    let now = service.core.clock.now();
    let served = service.core.revocations.served();
    let remaining = u64::try_from(served.next_update.saturating_sub(now)).unwrap_or(0);
    let gauges = [
        ("spot_store_revocation_generation", served.generation),
        ("spot_store_revocation_seconds_to_next_update", remaining),
        (
            "spot_store_revocation_stale",
            u64::from(service.core.revocations.is_stale(now)),
        ),
        (
            "spot_store_reviews_stuck",
            u64::try_from(service.core.reviewer.stuck()).unwrap_or(u64::MAX),
        ),
    ];
    let mut response = Response::new(axum::body::Body::from(service.core.metrics.render(&gauges)));
    set_header(
        &mut response,
        "content-type",
        "text/plain; version=0.0.4; charset=utf-8",
    );
    Ok(response)
}
