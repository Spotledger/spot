//! The `MetaService`, `ResourceService` and `AuthService` implementations over `StubHost`,
//! and the `AppError` → `ApiError` mapping the suites rely on (ADR-057: the class's registry
//! code, with `fields`, `details` and `explanation` surviving).

use serde_json::Value as Json;
use spot_app_api::context::DocumentService;
use spot_app_api::document::RowPatch;
use spot_app_api::{AppError, DocumentPatch, prelude::*};
use spot_http::api::codec::{ApiDocument, DocumentPatchApi};
use spot_http::api::lookup::{LookupPage, LookupRequest};
use spot_http::api::meta::DocTypeMeta;
use spot_http::api::registry::Code;
use spot_http::api::service::{
    ApiError, AuthService, BoxFuture, FieldIssue, LoginBody, LookupService, Message as ApiMessage,
    MessageLevel, MetaService, MethodOutcome, Page, QuerySpec, ReportColumn, ReportResult,
    RequestCtx, ResourceService, Session, SessionInfo, WriteResult,
};

use crate::support::Must;
use crate::support::fake::FakeApp;

use super::ok;

/// Maps an `AppError` onto the API layer per ADR-057: the top-level code is the class's
/// registry code (`ErrorClass::api_code`), and `fields`, `details` and `explanation` survive —
/// the suites assert `details.current_version` on a 412 and per-field codes on validation
/// failures, so a lossy mapping would hide exactly what the contract promises.
pub fn api_error(err: AppError) -> ApiError {
    let body = err.into_body();
    let registry = Code::from_code_str(body.class.api_code()).unwrap_or(Code::HttpInternal);
    let mut mapped = ApiError::new(registry, body.message);
    mapped.explanation = body.explanation;
    for field in body.fields {
        mapped.fields.push(FieldIssue::new(
            field.field.as_str().to_owned(),
            field.code,
            field.message,
        ));
    }
    for (key, value) in body.details {
        mapped.details.insert(key, value);
    }
    mapped
}

/// `DocumentPatchApi` → the app API's `DocumentPatch`.
fn patch_from(patch: &DocumentPatchApi) -> DocumentPatch {
    DocumentPatch {
        fields: patch.fields.clone(),
        tables: patch
            .tables
            .iter()
            .map(|(name, rows)| {
                (
                    name.clone(),
                    rows.iter()
                        .map(|row| RowPatch {
                            id: row.id,
                            fields: row.fields.clone(),
                        })
                        .collect(),
                )
            })
            .collect(),
    }
}

fn messages_of(messages: Vec<spot_app_api::Message>) -> Vec<ApiMessage> {
    messages
        .into_iter()
        .map(|message| ApiMessage {
            level: match message.level {
                spot_app_api::Level::Info => MessageLevel::Info,
                spot_app_api::Level::Warning => MessageLevel::Warning,
            },
            code: message.code,
            text: message.text,
            field: message.field.map(|field| field.as_str().to_owned()),
        })
        .collect()
}

impl MetaService for FakeApp {
    fn doctype_meta<'a>(
        &'a self,
        _ctx: &'a RequestCtx,
        doctype: &'a str,
    ) -> BoxFuture<'a, Result<DocTypeMeta, ApiError>> {
        Box::pin(async move {
            // The host's DocTypeDef is the source of truth, so unknown doctypes answer
            // `meta.unknown_doctype` like a real service would.
            let def = DocumentService::meta(&self.host, doctype)
                .await
                .map_err(api_error)?;
            let mut meta = super::def_meta(&def);
            for tweak in super::lock(&self.meta_tweaks).iter() {
                if tweak.doctype != doctype {
                    continue;
                }
                for field in meta
                    .fields
                    .iter_mut()
                    .filter(|f| f.name.as_str() == tweak.field)
                {
                    field.multiple = tweak.multiple;
                    field.hints.control = tweak.control;
                    field.has_hints = !field.hints.is_default();
                }
            }
            Ok(meta)
        })
    }
}

impl ResourceService for FakeApp {
    fn get<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
    ) -> BoxFuture<'a, Result<WriteResult, ApiError>> {
        Box::pin(async move {
            let inv = self.invocation(ctx);
            let outcome = DocumentService::load(&self.host, &inv, doctype, id)
                .await
                .map_err(api_error)?;
            Ok(WriteResult {
                document: self.to_api(&outcome.value),
                messages: messages_of(outcome.messages),
            })
        })
    }

    fn list<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        spec: &'a QuerySpec,
    ) -> BoxFuture<'a, Result<Page, ApiError>> {
        Box::pin(async move {
            let inv = self.invocation(ctx);
            self.fake_list(doctype, spec, &inv).await
        })
    }

    fn query<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        spec: &'a QuerySpec,
    ) -> BoxFuture<'a, Result<Page, ApiError>> {
        Box::pin(async move {
            let inv = self.invocation(ctx);
            self.fake_list(doctype, spec, &inv).await
        })
    }

    fn create<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        document: &'a ApiDocument,
    ) -> BoxFuture<'a, Result<WriteResult, ApiError>> {
        Box::pin(async move {
            let meta = self.meta_of(doctype);
            let id = document.id.unwrap_or_else(RecordId::new_v7);
            let mut claimed: Vec<RecordId> = Vec::new();
            for value in document.fields.values() {
                if let spot_http::api::codec::FieldValue::Rows(rows) = value {
                    claimed.extend(rows.iter().filter_map(|row| row.id));
                }
            }
            self.check_row_ids(id, &claimed)?;
            let inv = self.invocation(ctx);
            let outcome =
                DocumentService::create(&self.host, &inv, self.document_of(&meta, document, id))
                    .await
                    .map_err(api_error)?;
            self.claim_rows(id, &outcome.value);
            self.track(doctype, &outcome.value);
            Ok(WriteResult {
                document: self.to_api(&outcome.value),
                messages: messages_of(outcome.messages),
            })
        })
    }

    fn update<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
        patch: &'a DocumentPatchApi,
        expected: Version,
    ) -> BoxFuture<'a, Result<WriteResult, ApiError>> {
        Box::pin(async move {
            self.uphold_phone(doctype, id, patch)?;
            let mut claimed: Vec<RecordId> = Vec::new();
            for rows in patch.tables.values() {
                claimed.extend(rows.iter().filter_map(|row| row.id));
            }
            self.check_row_ids(id, &claimed)?;
            let inv = self.invocation(ctx);
            let outcome =
                DocumentService::update(&self.host, &inv, doctype, id, patch_from(patch), expected)
                    .await
                    .map_err(api_error)?;
            self.claim_rows(id, &outcome.value);
            if doctype == "customer"
                && let Value::Text(phone) = outcome.value.get("phone")
            {
                super::lock(&self.phones).insert(phone.clone(), id);
            }
            Ok(WriteResult {
                document: self.to_api(&outcome.value),
                messages: messages_of(outcome.messages),
            })
        })
    }

    fn post<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, Result<WriteResult, ApiError>> {
        Box::pin(async move {
            let inv = self.invocation(ctx);
            let outcome = DocumentService::post(&self.host, &inv, doctype, id, expected)
                .await
                .map_err(api_error)?;
            Ok(WriteResult {
                document: self.to_api(&outcome.value),
                messages: messages_of(outcome.messages),
            })
        })
    }

    fn cancel<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, Result<WriteResult, ApiError>> {
        Box::pin(async move {
            let inv = self.invocation(ctx);
            let outcome = DocumentService::cancel(&self.host, &inv, doctype, id, expected)
                .await
                .map_err(api_error)?;
            Ok(WriteResult {
                document: self.to_api(&outcome.value),
                messages: messages_of(outcome.messages),
            })
        })
    }

    fn correct<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, Result<WriteResult, ApiError>> {
        Box::pin(async move {
            let inv = self.invocation(ctx);
            let outcome = DocumentService::correct(&self.host, &inv, doctype, id, expected)
                .await
                .map_err(api_error)?;
            // The amended draft is a new stored document; track it like any other create.
            self.track(doctype, &outcome.value);
            self.claim_rows(outcome.value.id(), &outcome.value);
            Ok(WriteResult {
                document: self.to_api(&outcome.value),
                messages: messages_of(outcome.messages),
            })
        })
    }

    fn delete<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, Result<Vec<ApiMessage>, ApiError>> {
        Box::pin(async move {
            let inv = self.invocation(ctx);
            let messages = DocumentService::delete(&self.host, &inv, doctype, id, expected)
                .await
                .map_err(api_error)?;
            let phone = (doctype == "customer")
                .then(|| self.host.stored(doctype, id))
                .flatten()
                .and_then(|doc| match doc.get("phone") {
                    Value::Text(phone) => Some(phone.clone()),
                    _ => None,
                });
            self.untrack(doctype, id, phone);
            Ok(messages_of(messages.messages))
        })
    }

    fn rename<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
        new_name: &'a str,
        expected: Version,
    ) -> BoxFuture<'a, Result<WriteResult, ApiError>> {
        Box::pin(async move {
            let inv = self.invocation(ctx);
            let outcome =
                DocumentService::rename(&self.host, &inv, doctype, id, new_name, expected)
                    .await
                    .map_err(api_error)?;
            Ok(WriteResult {
                document: self.to_api(&outcome.value),
                messages: messages_of(outcome.messages),
            })
        })
    }

    fn call_method<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
        method: &'a str,
        _args: Json,
        expected: Option<Version>,
    ) -> BoxFuture<'a, Result<(MethodOutcome, Vec<ApiMessage>), ApiError>> {
        Box::pin(async move {
            if doctype != "sales_invoice" || method != "totals" {
                return Err(ApiError::new(
                    Code::MethodNotFound,
                    format!("method `{method}` is not whitelisted here."),
                ));
            }
            // `totals` is declared mutating, so a missing `If-Match` is the service's 428
            // (the concurrency suite walks it).
            if expected.is_none() {
                return Err(ApiError::from_code(Code::ApiPreconditionRequired));
            }
            if self.take_armed_totals_failure() {
                return Err(ApiError::from_code(Code::HttpInternal));
            }
            let inv = self.invocation(ctx);
            let outcome = DocumentService::load(&self.host, &inv, doctype, id)
                .await
                .map_err(api_error)?;
            let document = self.to_api(&outcome.value);
            let result = super::query::totals_json(self, &outcome.value);
            Ok((MethodOutcome { document, result }, Vec::new()))
        })
    }

    fn call_function<'a>(
        &'a self,
        _ctx: &'a RequestCtx,
        app: &'a str,
        module: &'a str,
        function: &'a str,
        _args: Json,
        via_get: bool,
    ) -> BoxFuture<'a, Result<(Json, Vec<ApiMessage>), ApiError>> {
        Box::pin(async move {
            match (app, module, function) {
                // One GET-safe function and one that only POST may call: the GET form of a
                // non-GET-safe function answers 405 `api.method_not_allowed` (the service's
                // choice, per the ResourceService contract).
                ("spot", "sales", "fn") => Ok((Json::Null, Vec::new())),
                ("spot", "sales", "sync_fn") => {
                    if via_get {
                        Err(ApiError::from_code(Code::ApiMethodNotAllowed))
                    } else {
                        Ok((Json::Null, Vec::new()))
                    }
                }
                _ => Err(ApiError::new(
                    Code::MethodNotFound,
                    "no whitelisted function with this name.",
                )),
            }
        })
    }

    fn run_report<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        report: &'a str,
        filters: Json,
        limit: Option<u64>,
    ) -> BoxFuture<'a, Result<(ReportResult, Vec<ApiMessage>), ApiError>> {
        Box::pin(async move {
            let inv = self.invocation(ctx);
            if report != "totals_by_customer" {
                return Err(ApiError::new(
                    Code::ReportNotFound,
                    format!("no report with this name: `{report}`."),
                ));
            }
            let limit = limit.unwrap_or(5);
            let wanted = filters
                .get("customer")
                .and_then(|wanted| wanted.as_str().map(str::to_owned));
            let ids: Vec<RecordId> = self.ids_of("sales_invoice");
            let mut rows: Vec<Vec<Json>> = Vec::new();
            for id in ids {
                if u64::try_from(rows.len()).unwrap_or(u64::MAX) >= limit {
                    break;
                }
                let outcome = DocumentService::load(&self.host, &inv, "sales_invoice", id)
                    .await
                    .map_err(api_error)?;
                let doc = outcome.value;
                if let Some(wanted) = &wanted {
                    let is_wanted = match doc.get("customer") {
                        Value::Id(customer) => customer.to_string() == *wanted,
                        _ => false,
                    };
                    if !is_wanted {
                        continue;
                    }
                }
                let row = vec![
                    spot_json_text(doc.get("name")),
                    self.value_json(doc.get("total")),
                    self.value_json(doc.get("customer")),
                ];
                rows.push(row);
            }
            let columns = vec![
                ReportColumn {
                    name: "name".to_owned(),
                    label: "Name".to_owned(),
                    field_type: spot_http::api::meta::FieldType::Text,
                },
                ReportColumn {
                    name: "total".to_owned(),
                    label: "Total".to_owned(),
                    field_type: spot_http::api::meta::FieldType::Decimal { scale: 2 },
                },
                ReportColumn {
                    name: "customer".to_owned(),
                    label: "Customer".to_owned(),
                    field_type: spot_http::api::meta::FieldType::Link,
                },
            ];
            Ok((
                ReportResult {
                    columns,
                    rows,
                    truncated: false,
                },
                Vec::new(),
            ))
        })
    }
}

fn spot_json_text(value: &Value) -> Json {
    match value {
        Value::Text(text) => Json::String(text.clone()),
        _ => Json::Null,
    }
}

/// The platform default lookup over the fake's stored documents (ADR-068 §3.4, WP-DOC-04's
/// real link search stands in here): label from the target DocType's title field, a
/// case-insensitive prefix match widening to a contains match so an empty text lists
/// everything, newest first, paged by a `p:<offset>` cursor like the fake's list engine.
impl LookupService for FakeApp {
    fn search<'a>(
        &'a self,
        _ctx: &'a RequestCtx,
        doctype: &'a str,
        field: &'a str,
        request: &'a LookupRequest,
    ) -> BoxFuture<'a, Result<LookupPage, ApiError>> {
        Box::pin(async move {
            // The target DocType and title field of the field being filled: the platform
            // default searches it. An unknown DocType or a field the DocType lacks answers
            // as a real host would, before any seam runs.
            let Some(meta) = self.try_meta_of(doctype) else {
                return Err(ApiError::from_code(Code::MetaUnknownDoctype));
            };
            let Some(definition) = meta.field(field) else {
                return Err(unknown_field(doctype, field));
            };
            if definition.child_doctype.is_some() {
                return Err(ApiError::new(
                    Code::ApiInvalidRequest,
                    "The lookup operation is for link, select, table_multiselect or text fields.",
                ));
            }
            // A named override (an app seam the test registered) answers instead of the
            // default when the field's definition names one; that is the service's seam
            // resolution (ADR-068 §3.4), and the endpoint saw no seam at all.
            let spec_seam = definition
                .lookup
                .as_ref()
                .and_then(|spec| spec.seam.clone());
            if let Some(seam) = spec_seam {
                return self.override_page(&seam, doctype, field, request).await;
            }
            // The platform default: link search over the field's target DocType. A
            // `select`/`text` field with no lookup spec is served from its own DocType's
            // documents by the same search (the fake's stand-in for the platform default).
            let target = definition
                .link_to
                .clone()
                .unwrap_or_else(|| ok(Name::new(doctype)));
            let title_field = self.title_field_of(target.as_str());
            self.platform_page(&target, &title_field, request).await
        })
    }
}

fn unknown_field(doctype: &str, field: &str) -> ApiError {
    let mut err = ApiError::new(
        Code::DocumentUnknownField,
        format!("The DocType `{doctype}` has no such field."),
    );
    err.fields.push(FieldIssue::new(
        field.to_owned(),
        "document.unknown_field",
        "The DocType has no such field.",
    ));
    err
}

impl AuthService for FakeApp {
    fn login<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        body: &'a LoginBody,
    ) -> BoxFuture<'a, Result<Session, ApiError>> {
        Box::pin(async move {
            self.note_peer(ctx);
            let Some(creds) = self.users.get(body.user.as_str()).cloned() else {
                return Err(ApiError::new(
                    Code::AuthInvalidCredentials,
                    "Wrong user or password.",
                ));
            };
            if creds.password != body.password {
                return Err(ApiError::new(
                    Code::AuthInvalidCredentials,
                    "Wrong user or password.",
                ));
            }
            if creds.disabled {
                return Err(ApiError::new(
                    Code::AuthAccountDisabled,
                    "Account is disabled.",
                ));
            }
            let two_factor = creds.totp_code.is_some();
            if two_factor && body.totp.is_none() && body.recovery_code.is_none() {
                return Err(ApiError::new(
                    Code::AuthTotpRequired,
                    "A one-time code is required for this user.",
                ));
            }
            if body.recovery_code.is_none() && two_factor {
                let totp = body.totp.as_ref().must();
                if Some(totp.as_str()) != creds.totp_code {
                    return Err(ApiError::new(
                        Code::AuthTotpInvalid,
                        "The one-time code is wrong.",
                    ));
                }
            }
            if let Some(recovery) = &body.recovery_code
                && creds.recovery != Some(recovery.as_str())
            {
                return Err(ApiError::new(
                    Code::AuthTotpInvalid,
                    "The recovery code is not correct.",
                ));
            }
            let serial = {
                let mut serial = super::lock(&self.token_serial);
                *serial += 1;
                *serial
            };
            let token = format!("s3cr3t-{}-{serial}", body.user);
            let user = FakeApp::user_info(body.user.as_str(), RecordId::new_v7());
            {
                let mut sessions = super::lock(&self.sessions);
                sessions.retain(|_, entry| entry.user.email != body.user);
                sessions.insert(token.clone(), super::SessionEntry { user: user.clone() });
            }
            Ok(Session {
                token,
                expires_at: "2030-01-01T00:00:00Z".to_owned(),
                user,
            })
        })
    }

    fn logout<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, Result<(), ApiError>> {
        Box::pin(async move {
            if let Some(credential) = &ctx.credential {
                let mut sessions = super::lock(&self.sessions);
                sessions.remove(&credential.token);
                super::lock(&self.revoked).insert(credential.token.clone());
            }
            Ok(())
        })
    }

    fn session<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, Result<SessionInfo, ApiError>> {
        Box::pin(async move {
            let user = ctx
                .credential
                .as_ref()
                .and_then(|credential| self.session_user(credential.token.as_str()));
            let Some(user) = user else {
                // A token the fake issued and then ended has expired; a token the fake never
                // issued is simply wrong.
                let expired = ctx.credential.as_ref().is_some_and(|credential| {
                    super::lock(&self.revoked).contains(&credential.token)
                });
                return Err(ApiError::new(
                    if expired {
                        Code::AuthSessionExpired
                    } else {
                        Code::AuthInvalidCredentials
                    },
                    "The session is gone.",
                ));
            };
            Ok(SessionInfo {
                expires_at: "2030-01-01T00:00:00Z".to_owned(),
                user,
                site: ctx.site.to_string(),
            })
        })
    }
}
