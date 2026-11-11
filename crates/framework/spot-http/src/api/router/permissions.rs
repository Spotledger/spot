//! `GET /api/v1/auth/permissions` and `POST /api/v1/resource/{doctype}/{id}/reveal`
//! (contract 0.5.0). Both carry what a service answers and compute nothing.
//!
//! Reveal is deliberately outside the idempotency layer: `Idempotency-Key` is ignored (a
//! stored response would keep the real value for 24 hours), the response is `no-store`, every
//! refusal is one identical `403 permission.reveal_denied`, and the value is never logged.

use axum::extract::{Path, Request};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::IntoResponse;
use serde_json::Value as Json;
use spot_storage_contract::Name;

use crate::api::codec::is_field_path;
use crate::api::registry::Code;
use crate::api::service::{ApiError, ErrorCode};
use crate::request::RequestId;
use crate::site::SiteContext;

use super::{
    Failure, HandlerResult, SharedState, build_ctx, envelope, fail, json_response, matches_etag,
    path_identifier, path_record_id, take_body,
};

/// The contract's `maxItems` of the repeated `doctype` parameter.
const MAX_DOCTYPES: usize = 100;
/// The contract's `maxLength` of a reveal `reason`.
const MAX_REASON_CHARS: usize = 500;

pub(super) async fn auth_permissions(
    axum::extract::State(state): axum::extract::State<SharedState>,
    site: SiteContext,
    headers: HeaderMap,
    request: Request,
) -> HandlerResult {
    let Some(service) = &state.permissions else {
        return Err(fail(
            ApiError::from_code(Code::HttpInternal),
            &site.request_id,
        ));
    };
    let doctypes = doctype_params(request.uri().query().unwrap_or(""), &site.request_id)?;
    let ctx = build_ctx(&site, &headers, None);
    let summary = service
        .summary(&ctx, &doctypes)
        .await
        .map_err(|e| fail(e, &site.request_id))?;
    let etag = summary.etag();
    if matches_etag(&headers, &etag) {
        let mut response = StatusCode::NOT_MODIFIED.into_response();
        if let Ok(value) = HeaderValue::from_str(&etag) {
            response.headers_mut().insert(header::ETAG, value);
        }
        response.headers_mut().insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("private, no-cache"),
        );
        return Ok(response);
    }
    let body = summary.to_json().map_err(|e| fail(e, &site.request_id))?;
    let mut response = json_response(StatusCode::OK, body);
    if let Ok(value) = HeaderValue::from_str(&etag) {
        response.headers_mut().insert(header::ETAG, value);
    }
    // The summary is the caller's own rights: a shared cache must never serve it to another user.
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-cache"),
    );
    Ok(response)
}

/// The repeated `doctype` query parameter: identifiers only, at most 100.
fn doctype_params(query: &str, request_id: &RequestId) -> Result<Vec<Name>, Failure> {
    let mut out: Vec<Name> = Vec::new();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if key != "doctype" {
            continue;
        }
        let name = Name::new(value).map_err(|_| {
            fail(
                ApiError::new(
                    Code::ApiInvalidRequest,
                    "`doctype` must be a DocType storage name.",
                ),
                request_id,
            )
        })?;
        if !out.contains(&name) {
            out.push(name);
        }
    }
    if out.len() > MAX_DOCTYPES {
        return Err(fail(
            ApiError::new(
                Code::ApiInvalidRequest,
                format!("At most {MAX_DOCTYPES} DocTypes can be asked about at once."),
            ),
            request_id,
        ));
    }
    Ok(out)
}

pub(super) async fn field_reveal(
    axum::extract::State(state): axum::extract::State<SharedState>,
    site: SiteContext,
    headers: HeaderMap,
    Path((doctype, id)): Path<(String, String)>,
    request: Request,
) -> HandlerResult {
    let Some(service) = &state.reveal else {
        return Err(fail(
            ApiError::from_code(Code::HttpInternal),
            &site.request_id,
        ));
    };
    path_identifier(&doctype, "DocType", &site.request_id)?;
    let id = path_record_id(&id, &site.request_id)?;
    // `Idempotency-Key` is ignored: it never reaches the service.
    let ctx = build_ctx(&site, &headers, None);
    let body = take_body(request, true, &site.request_id).await?;
    let (field, reason) = decode_request(&body.unwrap_or(Json::Null), &site.request_id)?;
    let value = service
        .reveal(&ctx, &doctype, id, &field, &reason)
        .await
        .map_err(|e| fail(normalise_refusal(e), &site.request_id))?;
    let mut data = serde_json::Map::new();
    data.insert("value".to_owned(), value);
    let mut response = json_response(StatusCode::OK, envelope(Json::Object(data), &[]));
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

fn decode_request(body: &Json, request_id: &RequestId) -> Result<(String, String), Failure> {
    let invalid = |message: &str| fail(ApiError::new(Code::ApiInvalidRequest, message), request_id);
    let Json::Object(map) = body else {
        return Err(invalid("The reveal body must be an object."));
    };
    if map.keys().any(|k| k != "field" && k != "reason") {
        return Err(invalid("The reveal body has only `field` and `reason`."));
    }
    let Some(Json::String(field)) = map.get("field") else {
        return Err(invalid("`field` is required and must be a string."));
    };
    if !is_field_path(field) {
        return Err(invalid("`field` must be a field path."));
    }
    let Some(Json::String(reason)) = map.get("reason") else {
        return Err(invalid("`reason` is required and must be a string."));
    };
    let length = reason.chars().count();
    if length == 0 || length > MAX_REASON_CHARS {
        return Err(invalid("`reason` must be 1 to 500 characters."));
    }
    Ok((field.clone(), reason.clone()))
}

/// Every refusal looks the same: no right, not revealable, no such document, an unreadable
/// document, an unknown DocType or field. Other failures (rate limit, outage) pass through.
fn normalise_refusal(error: ApiError) -> ApiError {
    let refusal = matches!(
        error.code,
        ErrorCode::Registry(
            Code::PermissionRevealDenied
                | Code::PermissionDenied
                | Code::PermissionFieldDenied
                | Code::DocumentNotFound
                | Code::MetaUnknownDoctype
                | Code::DocumentUnknownField
        )
    );
    if refusal {
        ApiError::from_code(Code::PermissionRevealDenied)
    } else {
        error
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doctype_params_collect_repeats_and_reject_bad_names() {
        let id = RequestId::generate();
        let names = doctype_params("doctype=a&x=1&doctype=b&doctype=a", &id)
            .ok()
            .unwrap_or_default();
        assert_eq!(names.len(), 2);
        assert!(doctype_params("doctype=Bad-Name", &id).is_err());
        assert!(doctype_params("", &id).is_ok_and(|names| names.is_empty()));
    }

    #[test]
    fn reveal_body_is_closed() {
        let id = RequestId::generate();
        let ok = serde_json::json!({"field": "iban", "reason": "ticket 1"});
        assert!(decode_request(&ok, &id).is_ok());
        for bad in [
            serde_json::json!({"field": "iban"}),
            serde_json::json!({"field": "iban", "reason": ""}),
            serde_json::json!({"field": "IBAN", "reason": "x"}),
            serde_json::json!({"field": "iban", "reason": "x", "fields": []}),
            serde_json::json!({"field": "iban", "reason": "x".repeat(501)}),
        ] {
            assert!(decode_request(&bad, &id).is_err(), "{bad}");
        }
        let long = serde_json::json!({"field": "items[2].iban", "reason": "x".repeat(500)});
        assert!(decode_request(&long, &id).is_ok());
    }

    #[test]
    fn every_refusal_cause_becomes_the_same_error() {
        let first = normalise_refusal(ApiError::from_code(Code::DocumentNotFound));
        for code in [Code::PermissionDenied, Code::PermissionRevealDenied] {
            assert_eq!(normalise_refusal(ApiError::from_code(code)), first);
        }
        let other = normalise_refusal(ApiError::from_code(Code::ServerUnavailable));
        assert_eq!(other, ApiError::from_code(Code::ServerUnavailable));
    }
}
