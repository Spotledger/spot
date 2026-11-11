//! The service traits the `/api/v1` endpoint layer runs on.
//!
//! The document engine (WP-DOC-04), authentication (WP-PLT-02), metadata (WP-MET-01) and
//! reports (WP-PLT-14) sit behind these traits; the HTTP layer knows nothing about them. All
//! traits are object-safe with boxed `Send` futures, like `spot-app-api` (ADR-057). Tests run
//! against the in-memory fake in `tests/support/fake.rs`, built on `spot_app_api::stub::StubHost`.
//!
//! `ResourceService` is the shape the native API needs; the document engine's real host
//! implements it by adapting [`spot_app_api::context::DocumentService`] onto it (the `patch`
//! argument is the decoded contract body, per ADR-055).

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;

use serde_json::Value as Json;
use spot_core::SiteId;
use spot_storage_contract::{Name, RecordId, Value, Version};

use crate::request::RequestId;

use super::codec::{ApiDocument, DocumentPatchApi};
use super::filter::{FilterAst, OrderKey};
use super::lookup::{LookupPage, LookupRequest};
use super::meta::FieldType;
use super::permissions::PermissionSummary;
use super::registry::Code;

/// A boxed `Send` future, the same idiom `spot-app-api` uses (ADR-057) so services can be
/// objects behind `Arc<dyn …>`. Defined locally to keep the main crate's dependency list to
/// what the brief allows; `spot-app-api` is a dev-dependency for the test fake only.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The caller's identity as the HTTP layer extracted it. The credential is the raw bearer
/// token; validating it (session token, API key, expiry, user) is `AuthService`'s job and each
/// service's own check, not the router's (WP-PLT-02, ADR-055).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Credential {
    /// The bearer token exactly as it arrived in the `Authorization` header.
    pub token: String,
}

impl Credential {
    /// Wraps a bearer token.
    pub fn new(token: impl Into<String>) -> Self {
        Credential {
            token: token.into(),
        }
    }
}

/// Everything a service needs about the request. One per API request.
#[derive(Clone, Debug)]
pub struct RequestCtx {
    /// The site the request resolved to (ADR-34).
    pub site: SiteId,
    /// The bearer credential, when the request carried one.
    pub credential: Option<Credential>,
    /// The `Idempotency-Key` header, when present (it becomes the outbox command id, ADR-053).
    pub idempotency_key: Option<String>,
    /// The request's preferred language from `Accept-Language`, when given.
    pub language: Option<String>,
    /// The end-to-end request id (`X-Request-Id`).
    pub request_id: RequestId,
    /// The address the request is attributed to (the caller's source): the connection, or the
    /// forwarded client when the connection is a trusted proxy. `None` when unknown. The port
    /// is not recorded.
    pub peer: Option<std::net::IpAddr>,
}

/// A non-fatal message on a successful result (the contract's `Message`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    /// `"info"` or `"warning"` (the contract's enum).
    pub level: MessageLevel,
    /// Stable dotted code — a registry code or an app code.
    pub code: String,
    /// Plain-language text.
    pub text: String,
    /// The field it is about, if any (`items[2].qty`, idx from 1).
    pub field: Option<String>,
}

/// How important a message is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageLevel {
    /// Information.
    Info,
    /// The user should check something; the operation still succeeded.
    Warning,
}

impl MessageLevel {
    /// The contract's string form.
    pub fn as_str(self) -> &'static str {
        match self {
            MessageLevel::Info => "info",
            MessageLevel::Warning => "warning",
        }
    }
}

/// One field error inside an `ApiError` (the contract's `FieldError`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldIssue {
    /// Field path, `customer` or `items[2].qty` (idx from 1).
    pub field: String,
    /// Stable dotted code (registry code or app code, for example `selling.qty_not_positive`).
    pub code: String,
    /// Plain-language message.
    pub message: String,
}

impl FieldIssue {
    /// A field error.
    pub fn new(
        field: impl Into<String>,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        FieldIssue {
            field: field.into(),
            code: code.into(),
            message: message.into(),
        }
    }

    /// The contract's JSON form of this field error.
    pub fn to_json(&self) -> Json {
        let mut map = serde_json::Map::new();
        map.insert("field".to_owned(), Json::String(self.field.clone()));
        map.insert("code".to_owned(), Json::String(self.code.clone()));
        map.insert("message".to_owned(), Json::String(self.message.clone()));
        Json::Object(map)
    }
}

/// A platform error code from the contract registry, or an app-defined dotted code. The
/// closed registry enum makes status/retryable total; app codes (`selling.*`, …) appear in
/// `fields` and `messages`, and `ApiError::code` carries `App(String)` when a service raises an
/// error under an unregistered code (the router then reports `http.internal` and logs it, the
/// same way `AppError` classes map in app-api).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ErrorCode {
    /// A code registered in the contract's `x-spot-error-codes`.
    Registry(Code),
    /// An app-defined dotted code.
    App(String),
}

impl ErrorCode {
    /// The dotted string form.
    pub fn as_str(&self) -> &str {
        match self {
            ErrorCode::Registry(code) => code.as_str(),
            ErrorCode::App(s) => s,
        }
    }
}

/// The error a service returns; the router renders it with the error envelope from
/// `error.rs` and the registry's status and `retryable` flag.
#[derive(Clone, Debug, PartialEq)]
pub struct ApiError {
    /// The code this error is reported under.
    pub code: ErrorCode,
    /// Short plain-language message.
    pub message: String,
    /// Longer offline help text (ADR-26).
    pub explanation: Option<String>,
    /// Field errors, in the order found.
    pub fields: Vec<FieldIssue>,
    /// Code-specific data, documented per code (for example `current_version`).
    pub details: BTreeMap<String, Json>,
}

impl ApiError {
    /// A registry-coded error with the contract's default message.
    pub fn from_code(code: Code) -> Self {
        ApiError {
            code: ErrorCode::Registry(code),
            message: code.default_message().to_owned(),
            explanation: None,
            fields: Vec::new(),
            details: BTreeMap::new(),
        }
    }

    /// A registry-coded error with a specific message.
    pub fn new(code: Code, message: impl Into<String>) -> Self {
        ApiError {
            code: ErrorCode::Registry(code),
            message: message.into(),
            explanation: None,
            fields: Vec::new(),
            details: BTreeMap::new(),
        }
    }

    /// A validation error for one field.
    pub fn field(
        path: impl Into<String>,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        let message = message.into();
        let mut err = ApiError::new(Code::DocumentValidationFailed, message.clone());
        err.fields.push(FieldIssue::new(path, code, message));
        err
    }

    /// The error an app raised, keeping its dotted code for the envelope's `error.code` when
    /// the platform registered it; `AppError::api_code` decides otherwise, so services adapt
    /// app-api errors through this.
    pub fn app(code: impl Into<String>, message: impl Into<String>) -> Self {
        let code = code.into();
        match Code::from_code_str(&code) {
            Some(registry) => ApiError::new(registry, message),
            None => ApiError {
                code: ErrorCode::App(code),
                message: message.into(),
                explanation: None,
                fields: Vec::new(),
                details: BTreeMap::new(),
            },
        }
    }

    /// Adds an explanation.
    pub fn explained(mut self, explanation: impl Into<String>) -> Self {
        self.explanation = Some(explanation.into());
        self
    }

    /// Adds one detail.
    pub fn detail(mut self, key: &str, value: Json) -> Self {
        self.details.insert(key.to_owned(), value);
        self
    }

    /// Adds one field error.
    pub fn with_field(mut self, issue: FieldIssue) -> Self {
        self.fields.push(issue);
        self
    }

    /// Whether the same request unchanged may succeed (from the registry; an unregistered app
    /// code is not retryable — the service should map it through a registry class instead).
    pub fn retryable(&self) -> bool {
        match &self.code {
            ErrorCode::Registry(code) => code.retryable(),
            ErrorCode::App(_) => false,
        }
    }

    /// The HTTP status registered for this error. An app-coded error has no registered status;
    /// the router reports it as `http.internal` after logging.
    pub fn status(&self) -> StatusCode {
        match &self.code {
            ErrorCode::Registry(code) => code.status(),
            ErrorCode::App(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// Render this error into the contract's error envelope body. `request_id` comes from the
    /// router; `explanation`, `fields` and `details` are omitted when empty (ADR-055).
    pub fn to_envelope_json(&self, request_id: &str) -> Json {
        use crate::api::service::ErrorCode as E;
        let mut error = serde_json::Map::new();
        let code = match &self.code {
            // An unregistered app code in `error.code` would break the contract's registry;
            // the router logs it and reports `http.internal` instead.
            E::Registry(code) => code.as_str().to_owned(),
            E::App(_) => "http.internal".to_owned(),
        };
        error.insert("code".to_owned(), Json::String(code));
        error.insert("message".to_owned(), Json::String(self.message.clone()));
        if let Some(explanation) = &self.explanation {
            error.insert("explanation".to_owned(), Json::String(explanation.clone()));
        }
        error.insert("retryable".to_owned(), Json::Bool(self.retryable()));
        error.insert("request_id".to_owned(), Json::String(request_id.to_owned()));
        if !self.fields.is_empty() {
            error.insert(
                "fields".to_owned(),
                Json::Array(self.fields.iter().map(FieldIssue::to_json).collect()),
            );
        }
        if !self.details.is_empty() {
            let mut details = serde_json::Map::new();
            for (k, v) in &self.details {
                details.insert(k.clone(), v.clone());
            }
            error.insert("details".to_owned(), Json::Object(details));
        }
        let mut body = serde_json::Map::new();
        body.insert("error".to_owned(), Json::Object(error));
        Json::Object(body)
    }
}

impl From<Code> for ApiError {
    fn from(code: Code) -> Self {
        ApiError::from_code(code)
    }
}

use axum::http::StatusCode;

/// One page of a list or query (the contract's `DocumentPage`).
#[derive(Clone, Debug, PartialEq)]
pub struct Page {
    /// The page's documents (field maps; the service returns only the requested fields plus
    /// `id` and `version`).
    pub documents: Vec<ApiDocument>,
    /// Opaque cursor for the next page, `None` on the last page.
    pub next_cursor: Option<String>,
    /// Total match count; present only when `with_total` was requested.
    pub total: Option<u64>,
}

impl Page {
    /// A page with documents and a cursor.
    pub fn new(documents: Vec<ApiDocument>, next_cursor: Option<String>) -> Self {
        Page {
            documents,
            next_cursor,
            total: None,
        }
    }
}

/// A list/query the router parsed, validated and assembled for the service. Conversion of the
/// filter to the storage query IR is the service's job (ADR-053).
#[derive(Clone, Debug, PartialEq)]
pub struct QuerySpec {
    /// Fields to return (`id` and `version` are always added by the encoder).
    pub fields: Vec<Name>,
    /// The validated filter tree; `None` means no filter.
    pub filter: Option<FilterAst>,
    /// Order keys, in order (max 8; the router checks).
    pub order: Vec<OrderKey>,
    /// Page size (1..=500, default 20; the router checks).
    pub limit: u32,
    /// Opaque cursor from the previous page.
    pub cursor: Option<String>,
    /// Whether the response should carry `total`.
    pub with_total: bool,
}

/// What `call_method` produced (the contract's `MethodResponse` data).
#[derive(Clone, Debug, PartialEq)]
pub struct MethodOutcome {
    /// The document as it is after the method ran.
    pub document: ApiDocument,
    /// The method's return value; `Json::Null` when it returns nothing.
    pub result: Json,
}

/// What `run_report` produced (the contract's `ReportResult`).
#[derive(Clone, Debug, PartialEq)]
pub struct ReportResult {
    /// The report's columns.
    pub columns: Vec<ReportColumn>,
    /// Rows of values in the contract's value encoding, aligned with `columns`.
    pub rows: Vec<Vec<Json>>,
    /// True when `limit` cut the result.
    pub truncated: bool,
}

/// One report column (the contract's `ReportResult.columns` item).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReportColumn {
    /// Storage name.
    pub name: String,
    /// Human label.
    pub label: String,
    /// The value type of the column (the contract's `FieldType`).
    pub field_type: FieldType,
}

/// A user as the contract's `UserSummary`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserInfo {
    /// The user's record id.
    pub id: RecordId,
    /// E-mail.
    pub email: String,
    /// Full name.
    pub full_name: String,
}

/// A new session (the contract's `Session`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Session {
    /// Opaque bearer token.
    pub token: String,
    /// Expiry as a UTC RFC 3339 instant ending in `Z`.
    pub expires_at: String,
    /// The signed-in user.
    pub user: UserInfo,
}

/// The current session (the contract's `SessionInfo`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionInfo {
    /// Expiry as a UTC RFC 3339 instant ending in `Z`.
    pub expires_at: String,
    /// The signed-in user.
    pub user: UserInfo,
    /// The site the session belongs to.
    pub site: String,
}

pub use super::meta::{DocTypeMeta, MetaField};

/// A field's value inside an `ApiDocument`: either a plain storage `Value`, or a child table's
/// rows. The codec converts between JSON and these, driven by `DocTypeMeta`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FieldValue {
    /// A scalar storage value.
    Scalar(Value),
    /// A child table's rows, in `idx` order (each row carries its own id).
    Rows(Vec<ChildTableRow>),
}

/// One child-table row as the API layer holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChildTableRow {
    /// The row's record id.
    pub id: RecordId,
    /// The row's field values (`idx` is implicit in the order of the vec).
    pub fields: BTreeMap<Name, Value>,
}

/// A read or write outcome carrying the saved/loaded document and the engine's messages.
#[derive(Clone, Debug, PartialEq)]
pub struct WriteResult {
    /// The document as the engine has it.
    pub document: ApiDocument,
    /// Non-fatal messages from the engine.
    pub messages: Vec<Message>,
}

/// DocType metadata, as the client runtime needs it (the contract's `meta_get`).
pub trait MetaService: Send + Sync + 'static {
    /// The DocType's metadata. Unknown DocType → `meta.unknown_doctype`.
    fn doctype_meta<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
    ) -> BoxFuture<'a, Result<DocTypeMeta, ApiError>>;
}

/// Documents: reads, writes, lifecycle actions, methods, functions and reports. Every route
/// the router mounts as secured guarantees `ctx.credential` is `Some`; validating the
/// credential itself (and permissions) is each service's job.
pub trait ResourceService: Send + Sync + 'static {
    /// Load one document with its child tables. Not found → `document.not_found`.
    fn get<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
    ) -> BoxFuture<'a, Result<WriteResult, ApiError>>;

    /// List documents with default fields, newest first (the contract's `resource_list`).
    fn list<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        spec: &'a QuerySpec,
    ) -> BoxFuture<'a, Result<Page, ApiError>>;

    /// Query documents with filters, ordering and keyset paging (`resource_query`).
    fn query<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        spec: &'a QuerySpec,
    ) -> BoxFuture<'a, Result<Page, ApiError>>;

    /// Create a document through its full create lifecycle (ADR-51).
    fn create<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        document: &'a ApiDocument,
    ) -> BoxFuture<'a, Result<WriteResult, ApiError>>;

    /// Update a document through its update (or `update_posted`) lifecycle with merge-patch
    /// semantics (ADR-055). `expected` is the parsed `If-Match` version.
    fn update<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
        patch: &'a DocumentPatchApi,
        expected: Version,
    ) -> BoxFuture<'a, Result<WriteResult, ApiError>>;

    /// Delete a draft document. Linked documents block it (`document.linked`).
    fn delete<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, Result<Vec<Message>, ApiError>>;

    /// Post a draft (ADR-42).
    fn post<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, Result<WriteResult, ApiError>>;

    /// Cancel a posted document.
    fn cancel<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, Result<WriteResult, ApiError>>;

    /// Correct a posted document: cancel it and create its amended draft as one step
    /// (ADR-42). The result is the new draft.
    fn correct<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, Result<WriteResult, ApiError>>;

    /// Change the human-readable name. The id never changes.
    fn rename<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
        new_name: &'a str,
        expected: Version,
    ) -> BoxFuture<'a, Result<WriteResult, ApiError>>;

    /// Call a whitelisted controller method on one document. `expected` is the parsed
    /// `If-Match`; a method that mutates requires it (the router answers `428`).
    fn call_method<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
        method: &'a str,
        args: Json,
        expected: Option<Version>,
    ) -> BoxFuture<'a, Result<(MethodOutcome, Vec<Message>), ApiError>>;

    /// Call a module-level whitelisted function. `via_get` says the request came through the
    /// GET form; a function not declared GET-safe answers `api.method_not_allowed` (405).
    fn call_function<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        app: &'a str,
        module: &'a str,
        function: &'a str,
        args: Json,
        via_get: bool,
    ) -> BoxFuture<'a, Result<(Json, Vec<Message>), ApiError>>;

    /// Run a report with filters (WP-PLT-14).
    fn run_report<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        report: &'a str,
        filters: Json,
        limit: Option<u64>,
    ) -> BoxFuture<'a, Result<(ReportResult, Vec<Message>), ApiError>>;
}

/// A lookup spec a field's definition carries, as the contract's meta schema (`LookupSpec`):
/// the field names a seam and version (ADR-068 §3.2). The service owns the resolution —
/// the form's view, then the site override, then the field's definition, then the platform
/// default — and the endpoint never sees a seam name: `POST /api/v1/lookup/{doctype}/{field}`
/// has no seam parameter, and a spec object in a request body would be rejected as an
/// unknown property.
pub trait LookupService: Send + Sync + 'static {
    /// One page of candidates as the contract's `LookupPage` (see [`super::lookup`]).
    /// The implementation enforces the caller's permissions on what it shows and resolves
    /// the field's seam; a failing or missing seam answers `lookup.seam_failed` with the
    /// explanation under `message`/`explanation` (ADR-26) and never falls back to the
    /// platform default (ADR-068 §3.8).
    fn search<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        field: &'a str,
        request: &'a LookupRequest,
    ) -> BoxFuture<'a, Result<LookupPage, ApiError>>;
}

/// The permission summary (`auth_permissions`, contract 0.5.0). The HTTP layer carries
/// what the service returns and computes nothing.
pub trait PermissionService: Send + Sync + 'static {
    /// The summary for the caller. `doctypes` empty asks for every DocType the caller holds
    /// a right on. An unknown DocType answers `meta.unknown_doctype`.
    fn summary<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctypes: &'a [Name],
    ) -> BoxFuture<'a, Result<PermissionSummary, ApiError>>;
}

/// Field reveal (`field_reveal`, contract 0.5.0). The one place a real value of a masked
/// field is read. The HTTP layer never logs, caches or stores the value.
pub trait RevealService: Send + Sync + 'static {
    /// The real value of `field` of one document, in the field's value encoding. Every
    /// refusal is reported by the router as the same `permission.reveal_denied`.
    fn reveal<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
        field: &'a str,
        reason: &'a str,
    ) -> BoxFuture<'a, Result<Json, ApiError>>;
}

/// The value kinds a lookup can serve (ADR-068 §3.1). A `secret` is never a lookup source,
/// and a table's value is rows of documents rather than candidates to pick.
pub fn is_lookup_capable_field(field: &MetaField) -> Result<(), ApiError> {
    let capable = matches!(field.field_type, FieldType::Link | FieldType::Text);
    if capable {
        Ok(())
    } else {
        Err(ApiError::new(
            Code::ApiInvalidRequest,
            "The lookup operation is for link, select, table_multiselect or text fields.",
        ))
    }
}

/// Login, logout and session introspection. The HTTP layer never validates credentials
/// itself; it hands the bearer token through `RequestCtx` (WP-PLT-02 builds the real service).
pub trait AuthService: Send + Sync + 'static {
    /// Exchange credentials for a session token.
    fn login<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        body: &'a LoginBody,
    ) -> BoxFuture<'a, Result<Session, ApiError>>;

    /// End the current session. Idempotent; `204` even when the session is already gone.
    fn logout<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, Result<(), ApiError>>;

    /// The current session and its user.
    fn session<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, Result<SessionInfo, ApiError>>;
}

/// The decoded `POST /api/v1/auth/login` body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoginBody {
    /// E-mail or user name.
    pub user: String,
    /// The password.
    pub password: String,
    /// A one-time code for two-factor users.
    pub totp: Option<String>,
    /// A recovery code instead of a TOTP.
    pub recovery_code: Option<String>,
}
