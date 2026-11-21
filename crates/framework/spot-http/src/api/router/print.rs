//! `print_get` (`GET /api/v1/print/{doctype}/{id}?format=`), contract 0.7.0, WP-PLT-13.T2.
//!
//! The router validates the two path names and the one query parameter, hands the request to
//! the [`PrintPort`] and answers the PDF bytes. Permission is the port's: this layer never
//! learns whether a refused document exists, and every refusal is the same
//! `404 document.not_found`. The `Content-Disposition` file name is built from the two
//! validated path names only. No error carries the port's text, template text or document
//! content, and nothing here logs any of it.

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::Response;
use serde_json::json;

use crate::api::print::PrintPortError;
use crate::api::registry::Code;
use crate::api::service::ApiError;
use crate::request::RequestId;
use crate::site::SiteContext;

use super::{
    Failure, HandlerResult, SharedState, build_ctx, fail, path_identifier, path_record_id,
};

/// PDF files start with this.
const PDF_MAGIC: &[u8] = b"%PDF-";

/// The contract's `Identifier`: `^[a-z][a-z0-9_]{0,62}$`.
fn is_identifier(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    (1..=63).contains(&bytes.len())
        && bytes[0].is_ascii_lowercase()
        && bytes[1..]
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_')
}

fn invalid(message: &str, request_id: &RequestId) -> Failure {
    fail(ApiError::new(Code::ApiInvalidRequest, message), request_id)
}

/// The `format` query parameter: the only one the route takes, at most once, an identifier.
fn format_param(query: &str, request_id: &RequestId) -> Result<Option<String>, Failure> {
    let mut format = None;
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if key != "format" {
            return Err(invalid(
                "`format` is the only query parameter of a print.",
                request_id,
            ));
        }
        if format.is_some() {
            return Err(invalid("`format` can be given only once.", request_id));
        }
        if !is_identifier(value) {
            return Err(invalid("`format` must be a print format name.", request_id));
        }
        format = Some(value.to_owned());
    }
    Ok(format)
}

/// The error for a port failure. Only the code is honoured: the message is the registry's
/// default, so template text or document content a port put in its own message never reaches
/// the caller. Refusals (404, permission) are the one indistinguishable answer; a code the
/// registry does not know is a failed render.
fn print_error(error: &PrintPortError) -> ApiError {
    let known = Code::from_code_str(&error.code);
    // The two print codes keep their own answer even though one of them is a 404.
    if matches!(
        known,
        Some(Code::PrintFormatUnknown | Code::PrintRenderFailed)
    ) {
        return ApiError::from_code(known.unwrap_or(Code::PrintRenderFailed));
    }
    let refusal = error.status == 404
        || matches!(known, Some(Code::DocumentNotFound | Code::PermissionDenied));
    if refusal {
        return ApiError::from_code(Code::DocumentNotFound);
    }
    match known {
        Some(code) => ApiError::from_code(code),
        None => ApiError::from_code(Code::PrintRenderFailed),
    }
}

fn not_configured(request_id: &RequestId) -> Response {
    let body = json!({ "error": {
        "code": "service.not_configured",
        "message": "This site has no print service configured.",
        "retryable": false,
        "request_id": request_id.as_str(),
    }});
    let mut response = Response::new(Body::from(body.to_string()));
    *response.status_mut() = StatusCode::NOT_IMPLEMENTED;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

pub(super) async fn print_get(
    State(state): State<SharedState>,
    site: SiteContext,
    headers: HeaderMap,
    Path((doctype, id)): Path<(String, String)>,
    request: axum::extract::Request,
) -> HandlerResult {
    let request_id = &site.request_id;
    let Some(print) = state.print.clone() else {
        return Ok(not_configured(request_id));
    };
    path_identifier(&doctype, "DocType", request_id)?;
    let record_id = path_record_id(&id, request_id)?;
    let format = format_param(request.uri().query().unwrap_or(""), request_id)?;
    let ctx = build_ctx(&site, &headers, None);
    let pdf = print
        .render(&ctx, &doctype, record_id, format.as_deref())
        .await
        .map_err(|e| fail(print_error(&e), request_id))?;
    // A port that hands back anything but a PDF is a failed render, not a 200.
    if !pdf.starts_with(PDF_MAGIC) {
        return Err(fail(
            ApiError::from_code(Code::PrintRenderFailed),
            request_id,
        ));
    }
    let mut response = Response::new(Body::from(pdf));
    *response.status_mut() = StatusCode::OK;
    let out = response.headers_mut();
    out.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/pdf"),
    );
    // Both names were validated above (lower-case storage names and a canonical UUID), so the
    // file name carries no document content and needs no escaping.
    if let Ok(value) =
        HeaderValue::from_str(&format!("inline; filename=\"{doctype}-{record_id}.pdf\""))
    {
        out.insert(header::CONTENT_DISPOSITION, value);
    }
    out.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    out.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    Ok(response)
}
