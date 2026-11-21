//! `file_upload` (`POST /api/v1/files`) and `file_get` (`GET /api/v1/files/{id}`), ADR-055.
//!
//! Neither route buffers a body. An upload is handed to the [`FilesPort`] as a
//! [`ByteStream`] over the request body, capped at the port's upload limit: the first chunk
//! that would cross it ends the stream with `413 file.too_large` and nothing further is read.
//! A multipart upload is parsed on the fly (one `file` part, anything else is refused before
//! the port can finish). A download streams the port's body back chunk by chunk.
//!
//! `Idempotency-Key` cannot use the JSON-body middleware (it buffers and requires JSON), so
//! the upload keeps the protocol here: the key is reserved against the file name and declared
//! type, and a repeat is replayed only if the bytes it carries hash to the digest of the
//! stored record (a different body under the same key is `409 api.idempotency_conflict`).
//!
//! Nothing here logs a file name or content, and no response names a path or storage key.

use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use axum::body::{Body, Bytes};
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::Response;
use hyper::body::{Body as HttpBody, Frame};
use serde_json::{Value as Json, json};
use sha2::{Digest as _, Sha256};

use crate::api::files::{
    BoxByteStream, ByteStream, FileGetRequest, FileGetStatus, FilePortError, FileRecord,
    FileUpload, FilesPort, next_chunk,
};
use crate::api::idempotency::{KeyScope, StoredResponse, fingerprint};
use crate::api::registry::Code;
use crate::api::service::ApiError;
use crate::request::RequestId;
use crate::site::SiteContext;

use super::middleware::{
    Admission, CLASS_HTTP_UPLOAD, Outcome, Persist, ReservationGuard, admit, settle,
};
use super::{
    Failure, HandlerResult, REPLAYED_HEADER, SharedState, build_ctx, credential_hash, envelope,
    fail, path_record_id, render_error,
};

/// The most a multipart preamble and part headers may take before the file bytes start.
const MAX_PART_HEAD: usize = 16 * 1024;

/// The longest display name accepted (the contract's `FileRecord.name`).
const MAX_NAME_CHARS: usize = 255;

/// The first error a body stream hit, shared with the handler: the port may report its own
/// failure instead of the stream's, but the cause of the stop is the one the caller sees.
type ErrorSlot = Arc<Mutex<Option<ApiError>>>;

fn record_error(slot: &ErrorSlot, error: &ApiError) {
    if let Ok(mut slot) = slot.lock()
        && slot.is_none()
    {
        *slot = Some(error.clone());
    }
}

fn take_error(slot: &ErrorSlot) -> Option<ApiError> {
    slot.lock().ok().and_then(|mut slot| slot.take())
}

fn invalid(message: &str) -> ApiError {
    ApiError::new(Code::ApiInvalidRequest, message)
}

// ---------------------------------------------------------------------------
// Request body streams

/// The request body as data chunks, with transport errors mapped to API errors.
struct BodyChunks {
    body: Body,
}

impl BodyChunks {
    fn poll_data(&mut self, cx: &mut Context<'_>) -> Poll<Option<Result<Bytes, ApiError>>> {
        loop {
            match Pin::new(&mut self.body).poll_frame(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Ready(Some(Err(error))) => {
                    return Poll::Ready(Some(Err(body_error(&error))));
                }
                Poll::Ready(Some(Ok(frame))) => {
                    if let Ok(data) = frame.into_data()
                        && !data.is_empty()
                    {
                        return Poll::Ready(Some(Ok(data)));
                    }
                }
            }
        }
    }
}

/// The server core caps bodies with `http_body_util::Limited`; its error is the
/// `http.body_too_large` case, anything else is an unreadable body.
fn body_error(error: &axum::Error) -> ApiError {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(current) = source {
        if current.is::<http_body_util::LengthLimitError>() {
            return ApiError::from_code(Code::HttpBodyTooLarge);
        }
        source = current.source();
    }
    invalid("The request body could not be read.")
}

/// An `application/octet-stream` body, capped at the upload limit.
struct OctetStream {
    chunks: BodyChunks,
    remaining: u64,
    slot: ErrorSlot,
    failed: bool,
}

impl ByteStream for OctetStream {
    fn poll_chunk(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Bytes, ApiError>>> {
        if self.failed {
            return Poll::Ready(None);
        }
        match self.chunks.poll_data(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Ready(Some(Ok(data))) => {
                let len = data.len() as u64;
                if len > self.remaining {
                    return self.stop(ApiError::from_code(Code::FileTooLarge));
                }
                self.remaining -= len;
                Poll::Ready(Some(Ok(data)))
            }
            Poll::Ready(Some(Err(error))) => self.stop(error),
        }
    }
}

impl OctetStream {
    fn stop(&mut self, error: ApiError) -> Poll<Option<Result<Bytes, ApiError>>> {
        self.failed = true;
        record_error(&self.slot, &error);
        Poll::Ready(Some(Err(error)))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Content,
    Tail,
    Done,
    Failed,
}

/// The content of the one `file` part of a `multipart/form-data` body, delimiter-aware. Bytes
/// that could be the start of the closing delimiter are held back until they cannot be; after
/// the delimiter the stream must see `--` (the end of the form), or the form had another part
/// and the upload fails before the port can finish.
struct MultipartFile {
    chunks: BodyChunks,
    buf: Vec<u8>,
    delimiter: Vec<u8>,
    phase: Phase,
    remaining: u64,
    slot: ErrorSlot,
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

impl MultipartFile {
    fn stop(&mut self, error: ApiError) -> Poll<Option<Result<Bytes, ApiError>>> {
        self.phase = Phase::Failed;
        record_error(&self.slot, &error);
        Poll::Ready(Some(Err(error)))
    }

    fn emit(&mut self, chunk: Vec<u8>) -> Poll<Option<Result<Bytes, ApiError>>> {
        let len = chunk.len() as u64;
        if len > self.remaining {
            return self.stop(ApiError::from_code(Code::FileTooLarge));
        }
        self.remaining -= len;
        Poll::Ready(Some(Ok(Bytes::from(chunk))))
    }

    /// Pulls more body into `buf`; `Some(poll)` when the caller must return it.
    fn fill(&mut self, cx: &mut Context<'_>) -> Option<Poll<Option<Result<Bytes, ApiError>>>> {
        match self.chunks.poll_data(cx) {
            Poll::Pending => Some(Poll::Pending),
            Poll::Ready(Some(Ok(data))) => {
                self.buf.extend_from_slice(&data);
                None
            }
            Poll::Ready(Some(Err(error))) => Some(self.stop(error)),
            Poll::Ready(None) => Some(self.stop(invalid(
                "The multipart body ended before its closing boundary.",
            ))),
        }
    }
}

impl ByteStream for MultipartFile {
    fn poll_chunk(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Bytes, ApiError>>> {
        let this = &mut *self;
        loop {
            match this.phase {
                Phase::Done | Phase::Failed => return Poll::Ready(None),
                Phase::Content => {
                    if let Some(at) = find(&this.buf, &this.delimiter) {
                        let chunk: Vec<u8> = this.buf.drain(..at).collect();
                        this.buf.drain(..this.delimiter.len());
                        this.phase = Phase::Tail;
                        if !chunk.is_empty() {
                            return this.emit(chunk);
                        }
                        continue;
                    }
                    let safe = this.buf.len().saturating_sub(this.delimiter.len() - 1);
                    if safe > 0 {
                        let chunk: Vec<u8> = this.buf.drain(..safe).collect();
                        return this.emit(chunk);
                    }
                    if let Some(poll) = this.fill(cx) {
                        return poll;
                    }
                }
                Phase::Tail => {
                    if this.buf.len() >= 2 {
                        if this.buf.starts_with(b"--") {
                            this.phase = Phase::Done;
                            return Poll::Ready(None);
                        }
                        return this.stop(invalid(
                            "A multipart upload carries exactly one `file` part.",
                        ));
                    }
                    if let Some(poll) = this.fill(cx) {
                        return poll;
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Request header parsing

/// The `;`-separated parameters of a header value, honouring quoted strings. The first element
/// is the bare value (`attachment`, `form-data`, `multipart/form-data`).
fn header_params(value: &str) -> (String, Vec<(String, String)>) {
    let mut parts: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut escaped = false;
    for c in value.chars() {
        if quoted {
            if escaped {
                current.push(c);
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                quoted = false;
            } else {
                current.push(c);
            }
        } else if c == '"' {
            quoted = true;
        } else if c == ';' {
            parts.push(std::mem::take(&mut current));
        } else {
            current.push(c);
        }
    }
    parts.push(current);
    let mut iter = parts.into_iter();
    let head = iter.next().unwrap_or_default().trim().to_owned();
    let params = iter
        .filter_map(|part| {
            let (key, value) = part.split_once('=')?;
            Some((key.trim().to_ascii_lowercase(), value.trim().to_owned()))
        })
        .collect();
    (head, params)
}

fn percent_decode(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = raw.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// The display name of a `Content-Disposition` value: `filename*` (RFC 8187, UTF-8) wins over
/// `filename`.
fn disposition_name(value: &str) -> Option<String> {
    let (_, params) = header_params(value);
    let extended = params
        .iter()
        .find(|(k, _)| k == "filename*")
        .and_then(|(_, v)| {
            let mut pieces = v.splitn(3, '\'');
            let charset = pieces.next()?;
            let _language = pieces.next()?;
            let encoded = pieces.next()?;
            charset
                .eq_ignore_ascii_case("utf-8")
                .then(|| percent_decode(encoded))
                .flatten()
        });
    extended.or_else(|| {
        params
            .into_iter()
            .find(|(k, _)| k == "filename")
            .map(|(_, v)| v)
    })
}

fn checked_display_name(raw: Option<String>) -> Result<String, ApiError> {
    let name = raw.map(|n| n.trim().to_owned()).unwrap_or_default();
    if name.is_empty() {
        return Err(invalid(
            "The upload must name the file with a Content-Disposition filename.",
        ));
    }
    if name.chars().count() > MAX_NAME_CHARS || name.chars().any(char::is_control) {
        return Err(invalid(
            "The file name must be 1 to 255 characters without control characters.",
        ));
    }
    Ok(name)
}

/// The upload's metadata and its body as a stream, once the request head is understood.
async fn open_upload(
    headers: &HeaderMap,
    body: Body,
    limit: u64,
    slot: &ErrorSlot,
) -> Result<(FileUpload, BoxByteStream), ApiError> {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ApiError::from_code(Code::ApiUnsupportedMediaType))?;
    let (media, params) = header_params(content_type);
    match media.to_ascii_lowercase().as_str() {
        "application/octet-stream" => {
            let declared = headers
                .get(header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok());
            if declared.is_some_and(|length| length > limit) {
                return Err(ApiError::from_code(Code::FileTooLarge));
            }
            let name = checked_display_name(
                headers
                    .get(header::CONTENT_DISPOSITION)
                    .and_then(|v| v.to_str().ok())
                    .and_then(disposition_name),
            )?;
            let stream = OctetStream {
                chunks: BodyChunks { body },
                remaining: limit,
                slot: Arc::clone(slot),
                failed: false,
            };
            Ok((
                FileUpload {
                    name,
                    content_type: None,
                },
                Box::pin(stream),
            ))
        }
        "multipart/form-data" => {
            let boundary = params
                .into_iter()
                .find(|(k, _)| k == "boundary")
                .map(|(_, v)| v)
                .filter(|b| !b.is_empty() && b.len() <= 70)
                .ok_or_else(|| invalid("The multipart Content-Type needs a boundary."))?;
            open_multipart(body, &boundary, limit, slot).await
        }
        _ => Err(ApiError::from_code(Code::ApiUnsupportedMediaType)),
    }
}

/// Reads the preamble and the headers of the first part (bounded), checks it is the `file`
/// part and returns the rest of the body as the file's stream.
async fn open_multipart(
    body: Body,
    boundary: &str,
    limit: u64,
    slot: &ErrorSlot,
) -> Result<(FileUpload, BoxByteStream), ApiError> {
    let mut chunks = BodyChunks { body };
    let opening = format!("--{boundary}\r\n").into_bytes();
    let mut buf: Vec<u8> = Vec::new();
    let head_end = loop {
        if let Some(at) = find(&buf, b"\r\n\r\n") {
            break at;
        }
        if buf.len() > MAX_PART_HEAD {
            return Err(invalid("The multipart part headers are too long."));
        }
        match std::future::poll_fn(|cx| chunks.poll_data(cx)).await {
            Some(Ok(data)) => buf.extend_from_slice(&data),
            Some(Err(error)) => return Err(error),
            None => return Err(invalid("The multipart body has no `file` part.")),
        }
    };
    if !buf.starts_with(&opening) || head_end < opening.len() {
        return Err(invalid(
            "The multipart body must start with its boundary and carry a `file` part.",
        ));
    }
    let head = std::str::from_utf8(&buf[opening.len()..head_end])
        .map_err(|_| invalid("The multipart part headers must be text."))?;
    let mut disposition = None;
    let mut part_type = None;
    for line in head.split("\r\n").filter(|l| !l.is_empty()) {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| invalid("A multipart part header is malformed."))?;
        match name.trim().to_ascii_lowercase().as_str() {
            "content-disposition" => disposition = Some(value.trim().to_owned()),
            "content-type" => part_type = Some(value.trim().to_owned()),
            _ => {}
        }
    }
    let disposition = disposition
        .ok_or_else(|| invalid("The multipart part needs a Content-Disposition header."))?;
    let (kind, params) = header_params(&disposition);
    let field = params.iter().find(|(k, _)| k == "name").map(|(_, v)| v);
    if !kind.eq_ignore_ascii_case("form-data") || field.map(String::as_str) != Some("file") {
        return Err(invalid(
            "A multipart upload carries exactly one part, named `file`.",
        ));
    }
    let name = checked_display_name(disposition_name(&disposition))?;
    let content_type = part_type.filter(|t| !t.is_empty()).map(|t| {
        t.split(';')
            .next()
            .unwrap_or(&t)
            .trim()
            .to_ascii_lowercase()
    });
    let rest = buf.split_off(head_end + 4);
    let stream = MultipartFile {
        chunks,
        buf: rest,
        delimiter: format!("\r\n--{boundary}").into_bytes(),
        phase: Phase::Content,
        remaining: limit,
        slot: Arc::clone(slot),
    };
    Ok((FileUpload { name, content_type }, Box::pin(stream)))
}

// ---------------------------------------------------------------------------
// Responses

fn not_configured(request_id: &RequestId) -> Response {
    let body = json!({ "error": {
        "code": "service.not_configured",
        "message": "This site has no file store configured.",
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

fn record_json(record: &FileRecord) -> Json {
    json!({
        "id": record.id.to_string(),
        "name": record.name,
        "size": record.size,
        "content_type": record.content_type,
        "digest": record.digest,
        "private": record.private,
    })
}

fn created(record: &FileRecord) -> Response {
    let body = envelope(record_json(record), &[]).to_string();
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = StatusCode::CREATED;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    // A new record is at version 1 (the contract's ETag is a decimal version).
    headers.insert(header::ETAG, HeaderValue::from_static("\"1\""));
    if let Ok(location) = HeaderValue::from_str(&format!("/api/v1/files/{}", record.id)) {
        headers.insert(header::LOCATION, location);
    }
    response
}

fn idempotency_key(headers: &HeaderMap, request_id: &RequestId) -> Result<Option<String>, Failure> {
    let Some(raw) = headers.get("idempotency-key") else {
        return Ok(None);
    };
    let key = raw.to_str().ok().filter(|key| {
        !key.is_empty() && key.len() <= 128 && key.bytes().all(|b| (0x21..=0x7e).contains(&b))
    });
    match key {
        Some(key) => Ok(Some(key.to_owned())),
        None => Err(fail(
            invalid("The Idempotency-Key header must be 1 to 128 visible ASCII characters."),
            request_id,
        )),
    }
}

fn replay(stored: &StoredResponse) -> Response {
    let status = StatusCode::from_u16(stored.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut response = Response::new(Body::from(stored.body.clone()));
    *response.status_mut() = status;
    for (name, value) in &stored.headers {
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            response.headers_mut().insert(name, value);
        }
    }
    response.headers_mut().insert(
        HeaderName::from_static(REPLAYED_HEADER),
        HeaderValue::from_static("true"),
    );
    response
}

/// The SHA-256 (hex) of what `stream` yields, read to its end.
async fn digest_of(mut stream: BoxByteStream) -> Result<String, ApiError> {
    let mut hasher = Sha256::new();
    while let Some(chunk) = next_chunk(&mut stream).await {
        hasher.update(&chunk?);
    }
    let mut out = String::with_capacity(64);
    for byte in hasher.finalize() {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    Ok(out)
}

/// A stored response whose record carries `digest`.
fn stored_digest(stored: &StoredResponse) -> Option<String> {
    let body: Json = serde_json::from_slice(&stored.body).ok()?;
    body.pointer("/data/digest")?.as_str().map(str::to_owned)
}

/// Whether an outcome must not be remembered: a retry should run again.
fn releases_key(status: StatusCode) -> bool {
    status.is_server_error()
        || status == StatusCode::UNAUTHORIZED
        || status == StatusCode::BAD_REQUEST
        || status == StatusCode::PAYLOAD_TOO_LARGE
        || status == StatusCode::UNSUPPORTED_MEDIA_TYPE
}

// ---------------------------------------------------------------------------
// file_upload

pub(super) async fn file_upload(
    State(state): State<SharedState>,
    site: SiteContext,
    headers: HeaderMap,
    request: Request,
) -> HandlerResult {
    let request_id = &site.request_id;
    let Some(files) = state.files.clone() else {
        return Ok(not_configured(request_id));
    };
    let key = idempotency_key(&headers, request_id)?;
    let slot: ErrorSlot = Arc::default();
    let (upload, stream) = open_upload(
        &headers,
        request.into_body(),
        files.max_upload_bytes(),
        &slot,
    )
    .await
    .map_err(|e| fail(e, request_id))?;
    let ctx = build_ctx(&site, &headers, key.clone());
    let Some(key) = key else {
        return Ok(store_file(&*files, &ctx, upload, stream, &slot, request_id).await);
    };
    let scope = KeyScope {
        site: site.site.as_str().to_owned(),
        credential: credential_hash(ctx.credential.as_ref()),
        key,
    };
    // The body is not part of the fingerprint (it is never buffered): the replay check below
    // compares its digest instead.
    let what = format!(
        "{}\0{}",
        upload.name,
        upload.content_type.as_deref().unwrap_or("")
    );
    let fp = fingerprint("POST", "/api/v1/files", what.as_bytes());
    match admit(&*state.idempotency, &scope, &fp)
        .await
        .map_err(|e| fail(e, request_id))?
    {
        Admission::Conflict => {
            return Err(fail(
                ApiError::from_code(Code::ApiIdempotencyConflict),
                request_id,
            ));
        }
        Admission::Replay(stored) => {
            let seen = digest_of(stream).await.map_err(|e| fail(e, request_id))?;
            if stored_digest(&stored).as_deref() == Some(seen.as_str()) {
                return Ok(replay(&stored));
            }
            return Err(fail(
                ApiError::from_code(Code::ApiIdempotencyConflict),
                request_id,
            ));
        }
        Admission::Run => {}
    }
    let mut guard = ReservationGuard {
        store: Arc::clone(&state.idempotency),
        scope: scope.clone(),
        fingerprint: fp.clone(),
        armed: true,
    };
    let response = store_file(&*files, &ctx, upload, stream, &slot, request_id).await;
    let status = response.status();
    let (parts, body) = response.into_parts();
    let bytes = axum::body::to_bytes(body, usize::MAX)
        .await
        .unwrap_or_default();
    settle(
        &*state.idempotency,
        &scope,
        &fp,
        Outcome {
            persist: Persist::Body,
            class: CLASS_HTTP_UPLOAD,
            release: releases_key(status),
        },
        &parts,
        &bytes,
    )
    .await;
    guard.armed = false;
    Ok(Response::from_parts(parts, Body::from(bytes)))
}

/// Hands the stream to the port and renders the outcome. A failure the stream itself hit
/// (too large, a second multipart part) is the answer even if the port reported another.
async fn store_file(
    files: &dyn FilesPort,
    ctx: &crate::api::service::RequestCtx,
    upload: FileUpload,
    stream: BoxByteStream,
    slot: &ErrorSlot,
    request_id: &RequestId,
) -> Response {
    let outcome = files.upload(ctx, upload, stream).await;
    if let Some(error) = take_error(slot) {
        return render_error(error, request_id);
    }
    match outcome {
        Ok(record) => created(&record),
        Err(error) => render_error(error.into_api_error(), request_id),
    }
}

// ---------------------------------------------------------------------------
// file_get

/// A download body pulled from the port chunk by chunk. A failure mid-stream aborts the
/// response (the head is already sent), with a message free of paths and content.
struct PortBody {
    stream: BoxByteStream,
}

impl HttpBody for PortBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        loop {
            match self.stream.as_mut().poll_chunk(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Ready(Some(Ok(chunk))) if chunk.is_empty() => {}
                Poll::Ready(Some(Ok(chunk))) => return Poll::Ready(Some(Ok(Frame::data(chunk)))),
                Poll::Ready(Some(Err(error))) => {
                    return Poll::Ready(Some(Err(std::io::Error::other(error.message))));
                }
            }
        }
    }
}

/// Unknown and forbidden are one answer, so the id alone reveals nothing.
fn download_error(error: FilePortError) -> ApiError {
    let refusal = error.status == 404
        || error.code == Code::DocumentNotFound.as_str()
        || error.code == Code::PermissionDenied.as_str();
    if refusal {
        ApiError::from_code(Code::DocumentNotFound)
    } else {
        error.into_api_error()
    }
}

fn header_text(headers: &HeaderMap, name: header::HeaderName) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

pub(super) async fn file_get(
    State(state): State<SharedState>,
    site: SiteContext,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> HandlerResult {
    let request_id = &site.request_id;
    let Some(files) = state.files.clone() else {
        return Ok(not_configured(request_id));
    };
    let id = path_record_id(&id, request_id)?;
    let ctx = build_ctx(&site, &headers, None);
    let request = FileGetRequest {
        range: header_text(&headers, header::RANGE),
        if_none_match: header_text(&headers, header::IF_NONE_MATCH),
    };
    let download = files
        .get(&ctx, id, request)
        .await
        .map_err(|e| fail(download_error(e), request_id))?;
    let status = match download.status {
        FileGetStatus::Full => StatusCode::OK,
        FileGetStatus::Partial => StatusCode::PARTIAL_CONTENT,
        FileGetStatus::NotModified => StatusCode::NOT_MODIFIED,
        FileGetStatus::RangeNotSatisfiable => StatusCode::RANGE_NOT_SATISFIABLE,
    };
    let has_bytes = matches!(
        download.status,
        FileGetStatus::Full | FileGetStatus::Partial
    );
    let body = match download.body {
        Some(stream) if has_bytes => Body::new(PortBody { stream }),
        _ => Body::empty(),
    };
    let mut response = Response::new(body);
    *response.status_mut() = status;
    let out = response.headers_mut();
    // Only these pass through: whatever else a port sets (a path, a key) never reaches the wire.
    let allowed = [
        header::ETAG,
        header::CONTENT_TYPE,
        header::CONTENT_DISPOSITION,
        header::CONTENT_RANGE,
        header::CONTENT_LENGTH,
        header::ACCEPT_RANGES,
    ];
    for (name, value) in &download.headers {
        let Ok(name) = HeaderName::from_bytes(name.as_bytes()) else {
            continue;
        };
        if !allowed.contains(&name) {
            continue;
        }
        if let Ok(value) = HeaderValue::from_str(value) {
            out.insert(name, value);
        }
    }
    out.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    out.insert(header::CACHE_CONTROL, HeaderValue::from_static("private"));
    if has_bytes {
        out.entry(header::CONTENT_TYPE)
            .or_insert(HeaderValue::from_static("application/octet-stream"));
        let usable = out
            .get(header::CONTENT_DISPOSITION)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| {
                let v = v.trim_start().to_ascii_lowercase();
                v.starts_with("attachment") || v.starts_with("inline")
            });
        if !usable {
            out.insert(
                header::CONTENT_DISPOSITION,
                HeaderValue::from_static("attachment"),
            );
        }
        out.entry(header::ACCEPT_RANGES)
            .or_insert(HeaderValue::from_static("bytes"));
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_come_from_quoted_plain_and_extended_parameters() {
        assert_eq!(
            disposition_name("attachment; filename=\"spec.pdf\"").as_deref(),
            Some("spec.pdf")
        );
        assert_eq!(
            disposition_name("attachment; filename=\"a;b \\\"c\\\".txt\"").as_deref(),
            Some("a;b \"c\".txt")
        );
        assert_eq!(
            disposition_name("attachment; filename=plain.txt").as_deref(),
            Some("plain.txt")
        );
        assert_eq!(
            disposition_name("attachment; filename=\"x.txt\"; filename*=UTF-8''caf%C3%A9.txt")
                .as_deref(),
            Some("café.txt")
        );
        assert_eq!(disposition_name("attachment"), None);
    }

    #[test]
    fn display_names_are_bounded() {
        assert!(checked_display_name(Some("ok.txt".into())).is_ok());
        assert!(checked_display_name(None).is_err());
        assert!(checked_display_name(Some("  ".into())).is_err());
        assert!(checked_display_name(Some("x".repeat(256))).is_err());
        assert!(checked_display_name(Some("a\nb".into())).is_err());
    }

    #[test]
    fn find_locates_a_delimiter() {
        assert_eq!(find(b"abc\r\n--zz", b"\r\n--zz"), Some(3));
        assert_eq!(find(b"abc", b"\r\n--zz"), None);
    }
}
