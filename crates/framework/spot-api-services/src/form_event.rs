//! `POST /api/v1/resource/{doctype}/form-event` over the engine's `field_changed` (WP-UI-01.T1).
//!
//! In, an unsaved draft and the field the user changed; out, a **document diff** restricted to
//! what the server's `field_changed:<field>` steps derived. The operation writes nothing, takes
//! no lock and is idempotent: the same bytes give the same response bytes (the draft's new row
//! ids are derived from the draft itself, never from a clock or a random source).
//!
//! # What the adapter does
//!
//! 1. Bounds the body and the draft's child rows, rate-limits the caller and starts a deadline
//!    from the request's remaining budget.
//! 2. Decodes the draft with the resource-write codec ([`decode_body`]) so every field, value
//!    and child row is checked exactly as a create body is. The draft's row `idx` is accepted
//!    and checked against the row's position, because the contract carries it; the write codec
//!    itself assigns `idx` from the order.
//! 3. Refuses a caller without `read` on the DocType (`403 permission.denied`) and a `changed`
//!    path the caller cannot read (`400 api.invalid_request`, naming no field).
//! 4. Runs the engine's `field_changed` on the draft. No transaction, no storage write and no
//!    outbox row is created; the tests assert that on the counting fake.
//! 5. Diffs the draft against the result: top-level values and child rows by row id (adds,
//!    removals and reorders included). A field the caller may not read never appears; a masked
//!    field only appears in its masked form. Messages about a hidden field are dropped, and a
//!    step's error that mentions a hidden field's name or label — however it spells it, anywhere
//!    in its text or its details — is reduced to its registry code.
//!
//! # What the endpoint never echoes
//!
//! The draft's creator is the authenticated actor (`stamp_creator`), never the client's value:
//! the draft is not a stored document, and every permission that reads `created_by` would
//! otherwise judge a claim the caller made up.
//!
//! A step's error never carries a string the caller sent. When it does, the error is reduced to
//! its registry code and the field paths the caller may read (`stable_error`), which also
//! removes the comparison: a guess that equals a hidden field's label is answered exactly like a
//! guess that does not, so the reply says nothing about which guess matched. A detail value
//! deeper than `MAX_DETAIL_DEPTH` or larger than `MAX_DETAIL_NODES` is treated as sensitive
//! and the error is redacted, never walked further or skipped.
//!
//! # `base_version`
//!
//! The contract's description is explicit that the server **does not re-read the document** for
//! a form event, so a stale `base_version` cannot be detected here and is not an error: a form
//! event is a pure derivation on the draft the client sent, and a stale base for a pure
//! derivation answers the same diff. The field is validated (an integer at least 1) and used to
//! tell the decision core that the draft names a stored document (its id takes part in share
//! decisions); the save still checks `If-Match` and answers `412 document.stale_version`. No
//! contract code registers a `409` for a form event, which is recorded under the task's open
//! questions rather than invented here.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value as Json};
use spot_app_api::Document;
use spot_app_api::context::{DocumentService, Invocation};
use spot_app_api::document::ChildRow;
use spot_app_api::meta::{DocTypeDef, FieldDef};
use spot_app_api::prelude::{RecordId, Value};
use spot_http::api::codec::{
    ApiDocument, FieldValue, MetaLookup, WriteMode, decode_body, encode_scalar, is_field_path,
};
use spot_http::api::meta::{DocTypeMeta, FieldType, MetaField};
use spot_http::api::registry::Code;
use spot_http::api::service::{
    ApiError, BoxFuture, ErrorCode, FieldIssue, Message as ApiMessage, RequestCtx,
};
use spot_permissions::{Action, DocFacts, FieldLevel, Principal};

use crate::access::{AccessControl, child_doctype, doc_facts, masked};
use crate::conversion::{api_error, messages_of};
use crate::meta_resources::metas_with_children;

mod limits;
mod matcher;
use limits::{Deadline, RateLimiter, rate_key};
use matcher::Terms;

/// The request's bounds, resolved by the host from its registry.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Largest accepted request body in bytes.
    pub max_body_bytes: usize,
    /// Largest number of child rows the draft may carry, across every table.
    pub max_child_rows: usize,
    /// The caller's interactive rate limit.
    pub rate: RateLimit,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            // The development defaults; a host passes the registry's values.
            max_body_bytes: 1024 * 1024,
            max_child_rows: 1000,
            rate: RateLimit {
                burst: 60,
                window: Duration::from_secs(1),
            },
        }
    }
}

/// A token bucket: `burst` form events per `window`, refilled at the start of each window.
#[derive(Clone, Copy, Debug)]
pub struct RateLimit {
    /// Tokens available at the start of a window.
    pub burst: u32,
    /// The refill window.
    pub window: Duration,
}

impl Default for RateLimit {
    fn default() -> Self {
        Limits::default().rate
    }
}

/// Answers whether the DocType's controller has a `field_changed:<point>` step. The seam
/// registry is the production source; the adapter takes this small trait so a host wires it
/// without this crate depending on the seam crate.
pub trait FieldChangedPlan: Send + Sync + 'static {
    /// Whether `field_changed:<point>` has at least one step on `doctype`.
    fn answers(&self, doctype: &str, point: &str) -> bool;
}

/// A plan that answers every field. The engine then runs whatever steps it has; a DocType with
/// none answers an empty diff instead of `form.field_unknown`. Hosts that can see their seams
/// pass a registry-backed plan instead.
#[derive(Clone, Copy, Debug, Default)]
pub struct EveryField;

impl FieldChangedPlan for EveryField {
    fn answers(&self, _doctype: &str, _point: &str) -> bool {
        true
    }
}

/// The diff and messages one form event produced.
#[derive(Clone, Debug, PartialEq)]
pub struct FormEvent {
    /// The document diff, in the merge-patch form `resource_update` takes.
    pub patch: Json,
    /// The engine's non-fatal messages, without any about a field the caller cannot read.
    pub messages: Vec<ApiMessage>,
}

impl FormEvent {
    /// The contract's `FormEventResponse` body: `{"data": {"patch": …}, "messages": …}`.
    /// `messages` is omitted when empty, as every other response envelope does.
    pub fn to_json(&self) -> Json {
        let mut data = Map::new();
        data.insert("patch".to_owned(), self.patch.clone());
        let mut body = Map::new();
        body.insert("data".to_owned(), Json::Object(data));
        if !self.messages.is_empty() {
            body.insert(
                "messages".to_owned(),
                Json::Array(self.messages.iter().map(message_json).collect()),
            );
        }
        Json::Object(body)
    }
}

fn message_json(m: &ApiMessage) -> Json {
    let mut map = Map::new();
    map.insert(
        "level".to_owned(),
        Json::String(m.level.as_str().to_owned()),
    );
    map.insert("code".to_owned(), Json::String(m.code.clone()));
    map.insert("message".to_owned(), Json::String(m.text.clone()));
    if let Some(field) = &m.field {
        map.insert("field".to_owned(), Json::String(field.clone()));
    }
    Json::Object(map)
}

/// Whoever serves the form event. The HTTP layer's route calls this once per request.
pub trait FormEventService: Send + Sync + 'static {
    /// Run one form event. `deadline` is the request's remaining budget; a step that outlives
    /// it answers `504 http.timeout` and the dropped future leaves nothing behind.
    fn form_event<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        body: &'a [u8],
        deadline: Duration,
    ) -> BoxFuture<'a, Result<FormEvent, ApiError>>;
}

/// The form-event adapter over a [`DocumentService`].
pub struct EngineFormEvents<S: DocumentService> {
    engine: Arc<S>,
    access: Arc<AccessControl>,
    limits: Limits,
    plan: Arc<dyn FieldChangedPlan>,
    limiter: RateLimiter,
}

impl<S: DocumentService> EngineFormEvents<S> {
    /// A form-event service over `engine`, bounded by the default [`Limits`] and answering every
    /// field through [`EveryField`].
    pub fn new(engine: Arc<S>, access: Arc<AccessControl>) -> Self {
        let limits = Limits::default();
        Self {
            engine,
            access,
            limits,
            plan: Arc::new(EveryField),
            limiter: RateLimiter::new(limits.rate),
        }
    }

    /// The same service with the host's bounds.
    #[must_use]
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.limits = limits;
        self.limiter = RateLimiter::new(limits.rate);
        self
    }

    /// The same service answering `form.field_unknown` from the host's step plan.
    #[must_use]
    pub fn with_plan(mut self, plan: Arc<dyn FieldChangedPlan>) -> Self {
        self.plan = plan;
        self
    }

    async fn run(
        &self,
        inv: &Invocation,
        principal: &Principal,
        doctype: &str,
        body: &[u8],
    ) -> Result<FormEvent, ApiError> {
        let def = self.access.doctype(doctype)?;
        let metas = metas_with_children(self.access.meta(), doctype)?;
        let wire = metas.get(doctype).ok_or_else(internal)?;

        let json: Json =
            serde_json::from_slice(body).map_err(|_| ApiError::from_code(Code::ApiInvalidJson))?;
        let request = decode_request(&json, wire, &metas, &self.limits)?;
        let mut draft = build_draft(&request.doc, &def);
        stamp_creator(&mut draft, principal, &def);
        let id = request.id.map(|id| id.to_string());
        let facts = doc_facts(&draft, id.as_deref());
        if !self.access.can(principal, Action::Read, &facts) {
            return Err(ApiError::from_code(Code::PermissionDenied));
        }

        let point = dispatch(
            &self.access,
            self.plan.as_ref(),
            &def,
            principal,
            &facts,
            &request,
            &draft,
        )?;
        let outcome = self
            .engine
            .field_changed(inv, draft.clone(), &point)
            .await
            .map_err(|error| {
                filter_error(
                    self.access.as_ref(),
                    principal,
                    &def,
                    &facts,
                    &draft,
                    api_error(error),
                )
            })?;

        let patch = diff(
            self.access.as_ref(),
            principal,
            &def,
            &metas,
            &facts,
            &draft,
            &outcome.value,
        )?;
        let messages = outcome
            .messages
            .into_iter()
            .filter(|m| {
                message_visible(
                    self.access.as_ref(),
                    principal,
                    &def,
                    &facts,
                    &outcome.value,
                    &draft,
                    m,
                )
            })
            .collect();
        Ok(FormEvent {
            patch,
            messages: messages_of(messages),
        })
    }
}

impl<S: DocumentService + 'static> FormEventService for EngineFormEvents<S> {
    fn form_event<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        body: &'a [u8],
        deadline: Duration,
    ) -> BoxFuture<'a, Result<FormEvent, ApiError>> {
        Box::pin(async move {
            if body.len() > self.limits.max_body_bytes {
                return Err(ApiError::new(
                    Code::HttpBodyTooLarge,
                    format!(
                        "The form event body is larger than the server's {} bytes.",
                        self.limits.max_body_bytes
                    ),
                ));
            }
            let inv = self.access.invocation(ctx)?;
            let principal = self.access.principal(&inv).await;
            self.limiter
                .check(&rate_key(&principal), self.access.clock().monotonic())?;
            let at = self.access.clock().monotonic() + deadline;
            let inner = self.run(&inv, &principal, doctype, body);
            Deadline {
                inner: Box::pin(inner),
                clock: self.access.clock().clone(),
                at,
            }
            .await
        })
    }
}

fn internal() -> ApiError {
    ApiError::new(
        Code::HttpInternal,
        "The form-event adapter lost its own metadata.",
    )
}

// --------------------------------------------------------------------------------------
// Decoding

/// One decoded form-event request.
struct Request {
    doc: ApiDocument,
    changed: String,
    /// The document id the draft names, when it came from a stored document.
    id: Option<RecordId>,
}

fn decode_request(
    json: &Json,
    wire: &DocTypeMeta,
    metas: &BTreeMap<String, DocTypeMeta>,
    limits: &Limits,
) -> Result<Request, ApiError> {
    let Json::Object(map) = json else {
        return Err(ApiError::new(
            Code::ApiInvalidRequest,
            "The form-event body must be a JSON object.",
        ));
    };
    for key in map.keys() {
        if !matches!(key.as_str(), "doc" | "changed" | "base_version") {
            return Err(ApiError::new(
                Code::ApiInvalidRequest,
                format!("`{key}` is not a form-event field."),
            ));
        }
    }
    let doc = map
        .get("doc")
        .ok_or_else(|| ApiError::new(Code::ApiInvalidRequest, "`doc` is required."))?;
    let changed = map
        .get("changed")
        .and_then(Json::as_str)
        .ok_or_else(|| ApiError::new(Code::ApiInvalidRequest, "`changed` must be a field path."))?;
    if !is_field_path(changed) {
        return Err(ApiError::new(
            Code::ApiInvalidRequest,
            "`changed` must be a field path: `field` or `table[idx].field` with idx from 1.",
        ));
    }
    // The contract's form event does not re-read the document, so nothing here compares
    // `base_version` with storage: it says the draft came from a stored document, and the
    // save's `If-Match` is what refuses a stale write. It is still validated as the schema says.
    if let Some(value) = map.get("base_version")
        && value.as_u64().filter(|version| *version >= 1).is_none()
    {
        return Err(ApiError::new(
            Code::ApiInvalidRequest,
            "`base_version` must be at least 1.",
        ));
    }

    let sanitized = strip_row_indexes(doc, wire, limits)?;
    let decoded = decode_body(&sanitized, wire, WriteMode::Create, None, MetaLookup(metas))?;
    Ok(Request {
        id: decoded.id,
        doc: decoded,
        changed: changed.to_owned(),
    })
}

/// The draft with each child row's `idx` checked against its position and removed, so the
/// resource-write codec (which assigns `idx` from the order) accepts it. Also enforces the
/// child-row cap.
fn strip_row_indexes(doc: &Json, wire: &DocTypeMeta, limits: &Limits) -> Result<Json, ApiError> {
    let Json::Object(map) = doc else {
        return Err(ApiError::new(
            Code::ApiInvalidRequest,
            "`doc` must be a JSON object.",
        ));
    };
    let mut rows_total: usize = 0;
    let mut out = Map::new();
    for (key, value) in map {
        let table = wire
            .field(key)
            .is_some_and(|f| f.field_type == FieldType::Table);
        if !table {
            out.insert(key.clone(), value.clone());
            continue;
        }
        let Json::Array(rows) = value else {
            out.insert(key.clone(), value.clone());
            continue;
        };
        rows_total = rows_total.saturating_add(rows.len());
        if rows_total > limits.max_child_rows {
            return Err(ApiError::new(
                Code::ApiInvalidRequest,
                format!(
                    "The draft has more than {} child rows.",
                    limits.max_child_rows
                ),
            ));
        }
        let mut clean = Vec::with_capacity(rows.len());
        for (index, row) in rows.iter().enumerate() {
            let Json::Object(row) = row else {
                clean.push(row.clone());
                continue;
            };
            let mut row = row.clone();
            if let Some(idx) = row.remove("idx") {
                let ok = idx.as_u64() == Some(index as u64 + 1);
                if !ok {
                    return Err(ApiError::new(
                        Code::ApiInvalidRequest,
                        format!("`{key}[{}].idx` must equal the row's position.", index + 1),
                    ));
                }
            }
            clean.push(Json::Object(row));
        }
        out.insert(key.clone(), Json::Array(clean));
    }
    Ok(Json::Object(out))
}

/// Build the engine's draft, giving a row (or the document) that carries no id a **stable** one
/// derived from its content, so the same request answers the same diff. The ids never reach a
/// response except in the diff, where the contract needs them.
fn build_draft(api: &ApiDocument, def: &DocTypeDef) -> Document {
    let mut doc = match api.id {
        Some(id) => Document::new(def.name.clone(), id),
        None => Document::new(
            def.name.clone(),
            stable_id(&format!("{}|doc", def.name.as_str())),
        ),
    };
    for (name, value) in &api.fields {
        match value {
            FieldValue::Scalar(v) => doc.set(name.clone(), v.clone()),
            FieldValue::Rows(rows) => {
                let built: Vec<ChildRow> = rows
                    .iter()
                    .enumerate()
                    .map(|(index, row)| ChildRow {
                        id: row.id.unwrap_or_else(|| {
                            stable_id(&format!(
                                "{}|{}|{}|{}",
                                def.name.as_str(),
                                name.as_str(),
                                index,
                                canonical_row(&row.fields)
                            ))
                        }),
                        fields: row.fields.clone(),
                    })
                    .collect();
                *doc.rows_mut(name.clone()) = built;
            }
        }
    }
    doc
}

/// The draft's owner columns. `created_by` is what the decision core reads (an `if_owner` grant
/// compares it with the principal); `owner` is the other spelling a DocType may use.
const OWNER_FIELDS: [&str; 2] = ["created_by", "owner"];

/// Put the authenticated actor in the draft's owner columns, whatever the client sent.
///
/// The draft is not a stored document, so its creator is not the client's to claim: a forged
/// value would be judged by every permission that reads it — an `if_owner` grant on the read that
/// follows this event — and would be stored by the save after it. Only a text column is stamped:
/// the decision core reads `created_by` as text, and an `owner` column of another kind (a link,
/// say) is not this adapter's to rewrite. A field the DocType does not declare is left alone. A
/// system request has no user, so its drafts carry no creator rather than the client's claim.
fn stamp_creator(draft: &mut Document, principal: &Principal, def: &DocTypeDef) {
    let creator = if principal.is_system {
        Value::Null
    } else {
        Value::Text(principal.user.clone())
    };
    for name in OWNER_FIELDS {
        let Some(field) = def.field(name) else {
            continue;
        };
        if !matches!(
            field.kind,
            spot_app_api::meta::FieldKind::Text { .. }
                | spot_app_api::meta::FieldKind::LongText { .. }
        ) {
            continue;
        }
        draft.set(field.name.clone(), creator.clone());
    }
}

fn canonical_row(fields: &BTreeMap<spot_storage_contract::Name, Value>) -> String {
    fields
        .iter()
        .map(|(name, value)| format!("{}={value:?}", name.as_str()))
        .collect::<Vec<_>>()
        .join(";")
}

/// A deterministic UUIDv7-shaped id from `seed` (FNV-1a over two lanes). The version nibble is
/// forced to 7, which is what [`RecordId::from_bytes`] checks.
fn stable_id(seed: &str) -> RecordId {
    let mut a: u64 = 0xcbf2_9ce4_8422_2325;
    let mut b: u64 = 0x8422_2325_cbf2_9ce4;
    for byte in seed.as_bytes() {
        a ^= u64::from(*byte);
        a = a.wrapping_mul(0x0000_0100_0000_01b3);
        b = b.rotate_left(7) ^ u64::from(*byte).wrapping_add(a);
    }
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&a.to_be_bytes());
    bytes[8..].copy_from_slice(&b.to_be_bytes());
    bytes[6] = (bytes[6] & 0x0f) | 0x70;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    match RecordId::from_bytes(bytes) {
        Ok(id) => id,
        // Unreachable: the version nibble above is 7, which is all the constructor checks.
        Err(_) => RecordId::new_v7(),
    }
}

// --------------------------------------------------------------------------------------
// The changed field

/// The `changed` path, resolved against the metadata and the draft.
enum Changed {
    Field(String),
    Row {
        table: String,
        index: usize,
        field: String,
    },
}

fn parse_changed(path: &str, def: &DocTypeDef) -> Result<Changed, ApiError> {
    // Every refusal about the field's existence names the client's own path and nothing else, so
    // an unreadable field answers exactly as an unknown one.
    match path.split_once('[') {
        None => {
            if def.field(path).is_none() {
                return Err(unknown_field(path));
            }
            Ok(Changed::Field(path.to_owned()))
        }
        Some((table, rest)) => {
            let Some((index, field)) = rest.split_once("].") else {
                return Err(ApiError::new(
                    Code::ApiInvalidRequest,
                    "`changed` must be `field` or `table[idx].field`.",
                ));
            };
            let Some(table_field) = def.field(table) else {
                return Err(unknown_field(path));
            };
            if !matches!(
                table_field.kind,
                spot_app_api::meta::FieldKind::Table { .. }
                    | spot_app_api::meta::FieldKind::TableMultiselect { .. }
            ) {
                return Err(unknown_field(path));
            }
            let index: usize = index.parse().map_err(|_| {
                ApiError::new(Code::ApiInvalidRequest, "`changed` must use idx from 1.")
            })?;
            if child_doctype(def, table).is_none() {
                return Err(unknown_field(path));
            }
            Ok(Changed::Row {
                table: table.to_owned(),
                index,
                field: field.to_owned(),
            })
        }
    }
}

fn unknown_field(field: &str) -> ApiError {
    ApiError::new(
        Code::DocumentUnknownField,
        format!("The DocType has no such field: `{field}`."),
    )
    .with_field(FieldIssue::new(
        field,
        "document.unknown_field",
        "The DocType has no such field.",
    ))
}

/// Resolve the engine's step point and check the caller may read the changed field.
fn dispatch(
    access: &AccessControl,
    plan: &dyn FieldChangedPlan,
    def: &DocTypeDef,
    principal: &Principal,
    facts: &DocFacts,
    request: &Request,
    draft: &Document,
) -> Result<String, ApiError> {
    let changed = parse_changed(&request.changed, def)?;
    let candidates: Vec<String> = match &changed {
        Changed::Field(field) => {
            if access.field_level(principal, def.name.as_str(), field, facts) == FieldLevel::Hidden
            {
                return Err(unreadable(&request.changed));
            }
            vec![field.clone()]
        }
        Changed::Row {
            table,
            index,
            field,
        } => {
            if access.field_level(principal, def.name.as_str(), table, facts) == FieldLevel::Hidden
            {
                return Err(unreadable(&request.changed));
            }
            let rows = draft.rows(table);
            if *index == 0 || *index > rows.len() {
                return Err(ApiError::new(
                    Code::ApiInvalidRequest,
                    format!(
                        "`{}` names a row that the draft does not have.",
                        request.changed
                    ),
                ));
            }
            let child = child_doctype(def, table).ok_or_else(|| unreadable(&request.changed))?;
            let child_meta = access
                .meta()
                .doctype(&child)
                .ok_or_else(|| unreadable(&request.changed))?;
            if child_meta.field(field).is_none() {
                return Err(unreadable(&request.changed));
            }
            // The row's own values: a user permission on a child link (`item`) must judge this
            // row, not an empty one.
            let row_facts = row_facts(&child, &rows[*index - 1]);
            if access.field_level(principal, &row_facts.doctype, field, &row_facts)
                == FieldLevel::Hidden
            {
                return Err(unreadable(&request.changed));
            }
            vec![field.clone(), table.clone()]
        }
    };
    candidates
        .into_iter()
        .find(|point| plan.answers(def.name.as_str(), point))
        .ok_or_else(|| ApiError::from_code(Code::FormFieldUnknown))
}

/// The answer for a `changed` path the caller may not read: the one an unknown path gets.
fn unreadable(path: &str) -> ApiError {
    unknown_field(path)
}

/// The decision-core facts of one child row: its own values, so a user permission on a link in
/// the row (an `item`) is judged against this row.
fn row_facts(child: &str, row: &ChildRow) -> DocFacts {
    DocFacts {
        doctype: child.to_owned(),
        values: row
            .fields
            .iter()
            .map(|(name, value)| (name.as_str().to_owned(), value.clone()))
            .collect(),
        ..DocFacts::default()
    }
}

// --------------------------------------------------------------------------------------
// The diff

fn diff(
    access: &AccessControl,
    principal: &Principal,
    def: &DocTypeDef,
    metas: &BTreeMap<String, DocTypeMeta>,
    facts: &DocFacts,
    draft: &Document,
    result: &Document,
) -> Result<Json, ApiError> {
    let mut patch = Map::new();
    for field in &def.fields {
        let name = field.name.as_str();
        match &field.kind {
            spot_app_api::meta::FieldKind::Table { .. }
            | spot_app_api::meta::FieldKind::TableMultiselect { .. } => {
                if access.field_level(principal, def.name.as_str(), name, facts)
                    == FieldLevel::Hidden
                {
                    continue;
                }
                if let Some(rows) = table_diff(access, principal, def, metas, name, draft, result)?
                {
                    patch.insert(name.to_owned(), rows);
                }
            }
            _ => {
                let level = access.field_level(principal, def.name.as_str(), name, facts);
                if level == FieldLevel::Hidden {
                    continue;
                }
                let before = draft.get(name);
                let after = result.get(name);
                let changed = match level {
                    FieldLevel::Masked(format) => masked(before, format) != masked(after, format),
                    _ => before != after,
                };
                if !changed {
                    continue;
                }
                let wire = wire_field(def, metas, name)?;
                let value = match level {
                    FieldLevel::Masked(format) => masked(after, format),
                    _ => after.clone(),
                };
                patch.insert(name.to_owned(), encode_scalar(&value, wire)?);
            }
        }
    }
    Ok(Json::Object(patch))
}

fn wire_field<'a>(
    def: &DocTypeDef,
    metas: &'a BTreeMap<String, DocTypeMeta>,
    field: &str,
) -> Result<&'a MetaField, ApiError> {
    metas
        .get(def.name.as_str())
        .and_then(|meta| meta.field(field))
        .ok_or_else(internal)
}

/// The table's entry in the patch, or `None` when the caller sees no change in it.
///
/// A table is replaced as a whole, so the entry lists every remaining row in order with its
/// `id` and `idx`; an existing row carries only the fields that changed, a new row every field
/// the caller may read. A removed row is simply absent. A change the caller cannot see (a
/// hidden column derived anew) does not put the table in the patch at all.
fn table_diff(
    access: &AccessControl,
    principal: &Principal,
    def: &DocTypeDef,
    metas: &BTreeMap<String, DocTypeMeta>,
    table: &str,
    draft: &Document,
    result: &Document,
) -> Result<Option<Json>, ApiError> {
    let Some(child) = child_doctype(def, table) else {
        return Ok(None);
    };
    let child_def = access.meta().doctype(&child).ok_or_else(internal)?;
    let child_meta = metas.get(&child).ok_or_else(internal)?;
    let before: Vec<&ChildRow> = draft.rows(table).iter().collect();
    let after: Vec<&ChildRow> = result.rows(table).iter().collect();
    let before_ids: Vec<RecordId> = before.iter().map(|r| r.id).collect();
    let after_ids: Vec<RecordId> = after.iter().map(|r| r.id).collect();
    let reordered = before_ids != after_ids;
    let by_id: BTreeMap<RecordId, &ChildRow> = before.iter().map(|r| (r.id, *r)).collect();

    let mut changed_any = false;
    let mut rows: Vec<Json> = Vec::with_capacity(after.len());
    for (index, row) in after.iter().enumerate() {
        let old = by_id.get(&row.id).copied();
        let mut entry = Map::new();
        entry.insert("id".to_owned(), Json::String(row.id.to_string()));
        entry.insert("idx".to_owned(), Json::from(index as u64 + 1));
        let facts_of_row = row_facts(&child, row);
        for field in &child_def.fields {
            let name = field.name.as_str();
            let level = access.field_level(principal, &child, name, &facts_of_row);
            if level == FieldLevel::Hidden {
                continue;
            }
            let field_meta = child_meta.field(name).ok_or_else(internal)?;
            let after_value = row.fields.get(&field.name).unwrap_or(&Value::Null);
            let changed = match old {
                None => true,
                Some(old) => match level {
                    FieldLevel::Masked(format) => {
                        let before_value = old.fields.get(&field.name).unwrap_or(&Value::Null);
                        masked(before_value, format) != masked(after_value, format)
                    }
                    _ => old.fields.get(&field.name).unwrap_or(&Value::Null) != after_value,
                },
            };
            if !changed {
                continue;
            }
            changed_any = true;
            let value = match level {
                FieldLevel::Masked(format) => masked(after_value, format),
                _ => after_value.clone(),
            };
            entry.insert(name.to_owned(), encode_scalar(&value, field_meta)?);
        }
        rows.push(Json::Object(entry));
    }
    if !changed_any && !reordered {
        return Ok(None);
    }
    Ok(Some(Json::Array(rows)))
}

/// Whether a field path (`field` or `table[idx].field`) may be named to the caller. A path into
/// a row is judged against that row's own values, taken from `current` and then `fallback`; a
/// row that neither has is not shown (fail closed). A path the DocType does not know is not
/// hidden by permissions.
fn path_visible(
    access: &AccessControl,
    principal: &Principal,
    def: &DocTypeDef,
    facts: &DocFacts,
    current: &Document,
    fallback: &Document,
    path: &str,
) -> bool {
    match path.split_once('[') {
        None => {
            if def.field(path).is_none() {
                return true;
            }
            access.field_level(principal, def.name.as_str(), path, facts) != FieldLevel::Hidden
        }
        Some((table, rest)) => {
            if access.field_level(principal, def.name.as_str(), table, facts) == FieldLevel::Hidden
            {
                return false;
            }
            let Some((index, field)) = rest.split_once("].") else {
                return true;
            };
            let Some(child) = child_doctype(def, table) else {
                return true;
            };
            let row = index.parse::<usize>().ok().and_then(|index| {
                let at = index.checked_sub(1)?;
                current
                    .rows(table)
                    .get(at)
                    .or_else(|| fallback.rows(table).get(at))
            });
            let Some(row) = row else {
                return false;
            };
            let row_facts = row_facts(&child, row);
            access.field_level(principal, &child, field, &row_facts) != FieldLevel::Hidden
        }
    }
}

/// Whether a message may be shown: one about a field the caller cannot read is dropped, so the
/// messages never say a hidden field exists.
fn message_visible(
    access: &AccessControl,
    principal: &Principal,
    def: &DocTypeDef,
    facts: &DocFacts,
    current: &Document,
    fallback: &Document,
    message: &spot_app_api::Message,
) -> bool {
    let Some(path) = message.field.as_ref().map(|field| field.as_str()) else {
        return true;
    };
    path_visible(access, principal, def, facts, current, fallback, path)
}

/// How deep a chain of tables is walked. The metadata compiler refuses a table field inside a
/// child DocType today, so the depth it allows is one table level; the bound and the path-cycle
/// check keep the walk finite should that rule ever loosen.
const MAX_TABLE_DEPTH: usize = 8;

/// Every term the caller may not read: the name and every label of each field hidden from them,
/// at the top level and in every child table they can see.
///
/// The columns are taken from the child DocType's **definition**, not from the draft's rows, so a
/// hidden column of a table the draft leaves empty is covered as well; a column the decision core
/// hides for any row the draft carries is covered too. A table field that is hidden itself hides
/// every column below it, whatever their own protection says. Fail closed: a term that may be
/// hidden is hidden, because an error must not say a hidden column exists.
fn hidden_terms(
    access: &AccessControl,
    principal: &Principal,
    def: &DocTypeDef,
    facts: &DocFacts,
    draft: &Document,
) -> BTreeSet<String> {
    let mut terms = BTreeSet::new();
    let mut path = Vec::new();
    collect_hidden(
        access,
        principal,
        def,
        std::slice::from_ref(facts),
        Some(draft),
        false,
        &mut terms,
        &mut path,
        0,
    );
    terms
}

/// The recursive half of `hidden_terms`: one DocType's fields, then each child table's fields.
///
/// `facts` are the fact sets this level's fields are judged with — the DocType's own facts and,
/// at the top level only, the draft's rows of each table. `draft` is the document that carries a
/// table's rows; only the root has one, because a child row holds scalars and no nested rows.
#[allow(clippy::too_many_arguments)]
fn collect_hidden(
    access: &AccessControl,
    principal: &Principal,
    def: &DocTypeDef,
    facts: &[DocFacts],
    draft: Option<&Document>,
    inherited: bool,
    terms: &mut BTreeSet<String>,
    path: &mut Vec<String>,
    depth: usize,
) {
    if depth > MAX_TABLE_DEPTH || path.iter().any(|seen| seen == def.name.as_str()) {
        return;
    }
    path.push(def.name.as_str().to_owned());
    for field in &def.fields {
        let name = field.name.as_str();
        let hidden = inherited
            || facts.iter().any(|facts| {
                access.field_level(principal, def.name.as_str(), name, facts) == FieldLevel::Hidden
            });
        if hidden {
            terms.insert(name.to_owned());
            terms.extend(labels_of(field));
        }
        let Some(child) = child_doctype(def, name) else {
            continue;
        };
        let Some(child_def) = access.meta().doctype(&child) else {
            continue;
        };
        // The child DocType's own facts judge its columns when the draft carries no row; each
        // row of the draft adds its own facts, so a column hidden for one of them is caught.
        let mut child_facts = vec![DocFacts {
            doctype: child.clone(),
            ..DocFacts::default()
        }];
        if let Some(draft) = draft {
            child_facts.extend(draft.rows(name).iter().map(|row| row_facts(&child, row)));
        }
        collect_hidden(
            access,
            principal,
            &child_def,
            &child_facts,
            None,
            hidden,
            terms,
            path,
            depth + 1,
        );
    }
    path.pop();
}

/// Every label the metadata carries for `field`.
///
/// The effective metadata carries one label per field today — labels are not localized, and the
/// compiled view's label is the merged definition's label or the field's name — so this is one
/// term. It is a set rather than a string so a metadata that gains a second locale adds terms
/// without touching the matcher.
fn labels_of(field: &FieldDef) -> Vec<String> {
    let mut labels = vec![field.label.clone()];
    // A label is never empty (it defaults to the name), but never match on nothing.
    labels.retain(|label| !label.is_empty());
    labels
}

/// Every string the error carries that the caller can read: its message, its explanation, each
/// issue's field path, message and code, and every key and string value in `details`.
///
/// `None` when a detail value is deeper or has more nodes than the walk's caps. A step's details
/// are JSON it built, so a value past a cap is treated as sensitive: the caller of this function
/// redacts the whole error rather than walking further or skipping the value.
fn texts_of(error: &ApiError) -> Option<Vec<String>> {
    let mut texts = Vec::new();
    texts.push(error.message.clone());
    texts.extend(error.explanation.iter().cloned());
    for issue in &error.fields {
        texts.push(issue.field.clone());
        texts.push(issue.message.clone());
        texts.push(issue.code.clone());
    }
    let mut budget = MAX_DETAIL_NODES;
    for (key, value) in &error.details {
        texts.push(key.clone());
        if !collect_strings(value, 1, &mut budget, &mut texts) {
            return None;
        }
    }
    Some(texts)
}

/// How deep a detail value is walked, and how many nodes one walk may visit.
///
/// A detail is a step's own JSON and the walk recurses over it, so both bounds keep a hostile or
/// runaway value from costing the request its stack or its time. A detail of either kind is
/// bounded by the request's body elsewhere; the error's details are not, because a step builds
/// them.
const MAX_DETAIL_DEPTH: usize = 32;

/// The node budget of one detail walk, at any depth.
const MAX_DETAIL_NODES: usize = 4096;

/// Every key and string value `value` carries, at any depth of arrays and objects: a detail is
/// JSON a step built, so a hidden name can sit in a nested value as easily as a top-level one.
///
/// `false` when the value is past `MAX_DETAIL_DEPTH` or the `budget` is spent, which the caller
/// treats as sensitive.
fn collect_strings(value: &Json, depth: usize, budget: &mut usize, out: &mut Vec<String>) -> bool {
    if depth > MAX_DETAIL_DEPTH || *budget == 0 {
        return false;
    }
    *budget -= 1;
    match value {
        Json::String(text) => {
            out.push(text.clone());
            true
        }
        Json::Array(items) => items
            .iter()
            .all(|item| collect_strings(item, depth + 1, budget, out)),
        Json::Object(map) => map.iter().all(|(key, inner)| {
            out.push(key.clone());
            collect_strings(inner, depth + 1, budget, out)
        }),
        Json::Null | Json::Bool(_) | Json::Number(_) => true,
    }
}

/// Every text value the client sent in the draft: its own fields, and every child row's.
///
/// These are the strings a step may echo into its error, so they are what the error must not
/// give back: a client that sees its own text returned cannot tell whether the text matched a
/// hidden field's name, and a client that does not see it knows nothing either.
fn client_texts(draft: &Document) -> Vec<String> {
    let mut texts = Vec::new();
    for value in draft.fields().values() {
        if let Value::Text(text) = value {
            texts.push(text.clone());
        }
    }
    for rows in draft.tables().values() {
        for row in rows {
            for value in row.fields.values() {
                if let Value::Text(text) = value {
                    texts.push(text.clone());
                }
            }
        }
    }
    texts
}

/// The error with every string a step wrote removed: the registry code, the contract's default
/// message, and — per field issue the caller may read and whose path carries no client string —
/// the field path with that same stable code. No explanation and no detail survives.
fn stable_error(
    access: &AccessControl,
    principal: &Principal,
    def: &DocTypeDef,
    facts: &DocFacts,
    draft: &Document,
    client: &Terms,
    error: &ApiError,
) -> ApiError {
    let code = match error.code {
        ErrorCode::Registry(code) => code,
        _ => Code::HttpInternal,
    };
    let mut stable = ApiError::from_code(code);
    stable.fields = error
        .fields
        .iter()
        .filter(|issue| {
            !client.mentions(&issue.field)
                && path_visible(access, principal, def, facts, draft, draft, &issue.field)
        })
        .map(|issue| FieldIssue::new(issue.field.clone(), code.as_str(), code.default_message()))
        .collect();
    stable
}

/// The error the caller sees when any part of it must not be shown: its registry code with the
/// contract's default message and nothing else.
fn redacted(error: &ApiError) -> ApiError {
    match error.code {
        ErrorCode::Registry(code) => ApiError::from_code(code),
        _ => ApiError::from_code(Code::HttpInternal),
    }
}

/// A step's error, safe to show: when a field issue names a field the caller cannot read, when
/// any text of the error (message, explanation, issue paths, messages and codes, detail keys and
/// values, at any depth) mentions the name or a label of a hidden field, or when a detail value is
/// past the walk's caps, the whole error is replaced by its registry code with the contract's
/// default message. No field, explanation or detail survives, so the error cannot say a hidden
/// field exists.
///
/// A step's error that carries a string the client sent is first reduced to its code and its
/// readable field paths (`stable_error`). That is what keeps the error from reflecting client
/// input, and it is what removes the comparison: a guess that happens to equal a hidden field's
/// label is answered exactly like a guess that does not, so the reply says nothing about whether
/// the guess matched.
///
/// The match is canonical (`Terms::mentions`): `grand_total`, `grandTotal`, `Grand Total`,
/// `GRAND_TOTAL`, their fullwidth, zero-width and accented spellings and their Cyrillic
/// homoglyphs are one field, while a hidden `tax` leaves an error about `taxonomy` alone.
fn filter_error(
    access: &AccessControl,
    principal: &Principal,
    def: &DocTypeDef,
    facts: &DocFacts,
    draft: &Document,
    error: ApiError,
) -> ApiError {
    let terms = Terms::new(hidden_terms(access, principal, def, facts, draft));
    let client = Terms::new(client_texts(draft));

    // A detail past the walk's caps is sensitive: nothing about it may be shown.
    let Some(texts) = texts_of(&error) else {
        return redacted(&error);
    };
    // A client-supplied string in the error: the step reflected the caller's own text, so the
    // error is reduced before the hidden-name check looks at it.
    let (error, texts) = if !client.is_empty() && texts.iter().any(|text| client.mentions(text)) {
        let stable = stable_error(access, principal, def, facts, draft, &client, &error);
        let texts = texts_of(&stable).unwrap_or_default();
        (stable, texts)
    } else {
        (error, texts)
    };

    let path_hidden = error
        .fields
        .iter()
        .any(|issue| !path_visible(access, principal, def, facts, draft, draft, &issue.field));
    let text_hidden = !terms.is_empty() && texts.iter().any(|text| terms.mentions(text));
    if !path_hidden && !text_hidden {
        return error;
    }
    redacted(&error)
}
