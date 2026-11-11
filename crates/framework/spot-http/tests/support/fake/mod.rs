//! The fake behind every native-API suite: document semantics stay in
//! [`spot_app_api::stub::StubHost`] (lifecycle phases, patch semantics, link/unknown-field/
//! required checks, version bumps); this module holds the state and the DocTypes, and the
//! service-trait implementations live in [`query`] and [`services`].
//!
//! Suite requirements beyond `StubHost`, modelled here and nowhere else:
//! - sessions and users for bearer tokens (the fixed tokens are seeded; login answers `EVE`'s
//!   two-factor flow);
//! - the customer-phone uniqueness rule (`document.duplicate`, 409);
//! - list/query cursors, filtering and ordering (`StubHost` answers `platform.unsupported`
//!   for queries);
//! - the row-id ledger (ADR-055 lets clients choose row ids; a second document claiming one
//!   gives `document.duplicate`);
//! - one whitelisted method (`totals`) and one function, one report;
//! - a fault switch for the 5xx idempotency test (`arm_internal_totals_failure`).

pub mod query;
pub mod services;

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value as Json;
use spot_app_api::context::Invocation;
use spot_app_api::controller::{Controller, MethodDecl, StepDecl, StepName};
use spot_app_api::stub::StubHost;
use spot_app_api::{
    Actor, AppError, AppResult, ChildRow, DecimalFormat, DocTypeDef, Document, ErrorClass,
    FieldDef, FieldHints as ApiFieldHints, FieldKind, HookContext, HookPoint, IntFormat,
    JsonFormat, LookupSpec as ApiLookupSpec, Message, Phase, Target, TextFormat, prelude::*,
};
use spot_http::api::codec::{ApiDocument, ChildRowDoc, FieldValue};
use spot_http::api::meta::{DocTypeMeta, FieldHints, FieldType, MetaField};

use spot_http::api::service::{ApiError, RequestCtx, UserInfo};

use crate::support::Must;

/// Unwraps an infallible test-fixture result by panicking with it. `expect_used` is rejected
/// by the workspace clippy configuration, so fixtures go through here.
pub fn ok<T, E: std::fmt::Debug>(result: Result<T, E>) -> T {
    result.unwrap_or_else(|error| panic!("test fixture is infallible: {error:?}"))
}

/// Locks a plain mutex, recovering from a poisoned guard as the host code does.
pub fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn fn_name(name: &str) -> Name {
    ok(Name::new(name))
}

/// A user's login credential for the fake auth service.
#[derive(Clone)]
struct UserCreds {
    password: &'static str,
    totp_code: Option<&'static str>,
    recovery: Option<&'static str>,
    disabled: bool,
}

struct SessionEntry {
    user: UserInfo,
}

/// The lookup override a test registered for one field: an always-failing seam (`None`
/// page outcome = `lookup.seam_failed`), a fixed page, or a recording spy that echoes what
/// it received so a suite can assert the snapshot reached the service.
pub enum LookupOverride {
    /// The seam fails with this explanation (no fallback to the default list).
    Failing(String),
    /// The seam answers with this page every call.
    Static(spot_http::api::lookup::LookupPage),
    /// The seam echoes its request back as rows: one row per call, `label` marking the
    /// seam so the suites can tell which one answered, plus the exact request body it got.
    Spy,
}

pub(crate) struct LookupEntry {
    /// What a call to this seam answers.
    pub(crate) behaviour: LookupOverride,
    /// The `LookupRequest` of the last call, for the suites to assert on.
    last: Mutex<Option<Json>>,
}

/// The fake application behind `Arc<dyn MetaService>/ResourceService/AuthService/LookupService`.
pub struct FakeApp {
    host: StubHost,
    clock: Arc<dyn Clock>,
    users: BTreeMap<String, UserCreds>,
    sessions: Mutex<HashMap<String, SessionEntry>>,
    phones: Mutex<BTreeMap<String, RecordId>>,
    /// Insertion order of every stored document, newest last; lists scan it newest first.
    order: Mutex<Vec<(String, RecordId)>>,
    token_serial: Mutex<u64>,
    metas: Mutex<BTreeMap<String, DocTypeMeta>>,
    /// Owner document of every child-row id this fake has stored (ADR-055 lets clients choose
    /// row ids; a second document claiming one gives `document.duplicate`). The stub host does
    /// not police row ids across documents, so the fake does — the patch-review scenario 3.
    row_owners: Mutex<HashMap<RecordId, RecordId>>,
    /// Tokens issued and then logged out: a later session probe answers
    /// `auth.session_expired` for these, `auth.invalid_credentials` for never-issued ones.
    revoked: Mutex<std::collections::HashSet<String>>,
    /// Test lookup overrides by seam name (ADR-068 §3.4: the named override stands in for
    /// an app seam; `FakeApp::register_lookup_override` puts one under a name and the DocType
    /// definitions point a field at it).
    lookup_overrides: Mutex<BTreeMap<String, Arc<LookupEntry>>>,
    /// Each DocType's title field, kept from the definition (the fake's link-search labels).
    title_fields: BTreeMap<String, String>,
    fail_next_totals: AtomicBool,
    /// The `peer` of every request context a login or a resource call saw, oldest first.
    // effects-allow: IpAddr is a plain value type, no I/O
    seen_peers: Mutex<Vec<Option<std::net::IpAddr>>>,
    /// Presentation the fake adds to answers (contract 0.5.0): a currency map for every
    /// document, and `multiple`/`control` overrides on served metadata.
    currency: Mutex<BTreeMap<String, spot_http::api::codec::CurrencyInfo>>,
    meta_tweaks: Mutex<Vec<MetaTweak>>,
}

/// One field-level presentation override on served metadata.
#[derive(Clone)]
pub struct MetaTweak {
    /// The DocType.
    pub doctype: String,
    /// The field.
    pub field: String,
    /// Sets `multiple`.
    pub multiple: bool,
    /// Sets `hints.control`.
    pub control: Option<spot_http::api::meta::ControlHint>,
}

impl Default for FakeApp {
    fn default() -> Self {
        FakeApp {
            host: StubHost::default(),
            clock: Arc::new(spot_core::SystemClock::new()),
            users: BTreeMap::new(),
            sessions: Mutex::new(HashMap::new()),
            phones: Mutex::new(BTreeMap::new()),
            order: Mutex::new(Vec::new()),
            token_serial: Mutex::new(0),
            metas: Mutex::new(BTreeMap::new()),
            row_owners: Mutex::new(HashMap::new()),
            revoked: Mutex::new(std::collections::HashSet::new()),
            lookup_overrides: Mutex::new(BTreeMap::new()),
            title_fields: BTreeMap::new(),
            currency: Mutex::new(BTreeMap::new()),
            meta_tweaks: Mutex::new(Vec::new()),
            fail_next_totals: AtomicBool::new(false),
            seen_peers: Mutex::new(Vec::new()),
        }
    }
}

pub const ALICE: &str = "alice@acme.com";
pub const BOB: &str = "bob@acme.com";
pub const DAVE: &str = "dave@acme.com";
pub const EVE: &str = "eve@acme.com";
pub const TOKEN_ALICE: &str = "s3cr3t-alice";
pub const TOKEN_BOB: &str = "s3cr3t-bob";
pub const TOKEN_DAVE: &str = "s3cr3t-dave";
pub const TOKEN_EVE: &str = "s3cr3t-eve";

/// The lookup seams the seeded DocTypes name (ADR-068 §3.2), as the app-api `LOOKUP_VERSION`
/// stamps them. Tests register behaviours for these names before the lookup cases run.
pub const SEAM_FIXED: &str = "test:fixed";
pub const SEAM_BROKEN: &str = "test:broken";
pub const SEAM_SPY: &str = "test:spy";
/// The spy lives on the invoice's `notes` field (a text field's lookup is suggestions-only,
/// ADR-068 §3.1); the seam name is shared with the spy case's constant.
pub const SPARE_SEAM: &str = SEAM_SPY;

/// The invoice type with an override seam on `customer` (the override case), a failing seam
/// on the child item's link (nothing registers for it), and a spy on the customer's `phone`
/// (a text lookup is suggestions-only, ADR-068 §3.1); stamped with the lookup protocol's
/// major by [`ApiLookupSpec::seam`].
fn with_seams(mut base: DocTypeDef, specs: &[(&str, &str)]) -> DocTypeDef {
    let mut fields = Vec::with_capacity(base.fields.len());
    for mut field in base.fields.clone() {
        if let Some((_, seam)) = specs.iter().find(|(name, _)| *name == field.name.as_str()) {
            field.lookup = Some(ApiLookupSpec::seam(seam.to_owned()));
            // A hint beyond the default, so the meta case sees a `hints` property too;
            // `FieldHints` is `#[non_exhaustive]`, so the fields are set one by one.
            field.hints.in_standard_filter = true;
        }
        fields.push(field);
    }
    base.fields = fields;
    base
}

/// The customer with the spy seam on `phone`.
fn customer_type_with_seams() -> DocTypeDef {
    let mut base = customer_type();
    let mut fields = Vec::with_capacity(base.fields.len());
    for mut field in base.fields.clone() {
        if field.name.as_str() == "phone" {
            field.lookup = Some(ApiLookupSpec::seam(SEAM_SPY));
        }
        fields.push(field);
    }
    base.fields = fields;
    base
}

fn fields(defs: &[(&str, FieldKind, bool, bool)]) -> Vec<FieldDef> {
    defs.iter()
        .map(|(name, kind, required, allow_on_posted)| {
            let mut field = FieldDef::new(fn_name(name), kind.clone(), *required, *allow_on_posted);
            field.label = (*name).to_owned();
            field
        })
        .collect()
}

/// A plain DocType with a text title field.
fn customer_type() -> DocTypeDef {
    let mut def = DocTypeDef::new(
        fn_name("customer"),
        "Customer".to_owned(),
        false,
        false,
        Some(fn_name("name")),
        fields(&[
            (
                "name",
                FieldKind::Text {
                    max_chars: Some(140),
                    format: TextFormat::Plain,
                },
                true,
                false,
            ),
            (
                "phone",
                FieldKind::Text {
                    max_chars: None,
                    format: TextFormat::Phone,
                },
                false,
                false,
            ),
            (
                "outstanding",
                FieldKind::Decimal {
                    scale: 2,
                    precision: 18,
                    format: DecimalFormat::Plain,
                },
                false,
                false,
            ),
            (
                "category",
                FieldKind::Text {
                    max_chars: Some(140),
                    format: TextFormat::Plain,
                },
                false,
                false,
            ),
        ]),
    );
    def.version = 1;
    def
}

/// The link target for invoice rows.
fn item_type() -> DocTypeDef {
    let mut def = DocTypeDef::new(
        fn_name("item"),
        "Item".to_owned(),
        false,
        false,
        Some(fn_name("name")),
        fields(&[
            (
                "name",
                FieldKind::Text {
                    max_chars: Some(140),
                    format: TextFormat::Plain,
                },
                true,
                false,
            ),
            (
                "price",
                FieldKind::Decimal {
                    scale: 2,
                    precision: 18,
                    format: DecimalFormat::Plain,
                },
                false,
                false,
            ),
        ]),
    );
    def.version = 1;
    def
}

/// A posted DocType with a child table: lifecycle, `_state`, posted read-only rules.
fn invoice_type() -> DocTypeDef {
    let mut def = DocTypeDef::new(
        fn_name("sales_invoice"),
        "Sales Invoice".to_owned(),
        true,
        false,
        Some(fn_name("name")),
        fields(&[
            (
                "name",
                FieldKind::Text {
                    max_chars: Some(140),
                    format: TextFormat::Plain,
                },
                true,
                false,
            ),
            (
                "customer",
                FieldKind::Link {
                    doctype: fn_name("customer"),
                },
                true,
                false,
            ),
            (
                "notes",
                FieldKind::Text {
                    max_chars: None,
                    format: TextFormat::Plain,
                },
                false,
                true,
            ),
            (
                "total",
                FieldKind::Decimal {
                    scale: 2,
                    precision: 18,
                    format: DecimalFormat::Plain,
                },
                false,
                false,
            ),
            (
                "items",
                FieldKind::Table {
                    doctype: fn_name("sales_invoice_item"),
                },
                false,
                false,
            ),
        ]),
    );
    def.version = 1;
    def
}

/// The child rows of an invoice.
fn invoice_item_type() -> DocTypeDef {
    let mut def = DocTypeDef::new(
        fn_name("sales_invoice_item"),
        "Invoice Item".to_owned(),
        false,
        true,
        None,
        fields(&[
            (
                "item",
                FieldKind::Link {
                    doctype: fn_name("item"),
                },
                true,
                false,
            ),
            (
                "qty",
                FieldKind::Decimal {
                    scale: 2,
                    precision: 18,
                    format: DecimalFormat::Plain,
                },
                true,
                false,
            ),
            (
                "rate",
                FieldKind::Decimal {
                    scale: 2,
                    precision: 18,
                    format: DecimalFormat::Plain,
                },
                false,
                false,
            ),
            (
                "amount",
                FieldKind::Decimal {
                    scale: 2,
                    precision: 18,
                    format: DecimalFormat::Plain,
                },
                false,
                false,
            ),
        ]),
    );
    def.version = 1;
    def
}

/// Every Values-table kind as a field, so the codec suite can exercise each encoding over
/// HTTP without a second host.
fn doc_note_type() -> DocTypeDef {
    let mut def = DocTypeDef::new(
        fn_name("doc_note"),
        "Doc Note".to_owned(),
        false,
        false,
        Some(fn_name("name")),
        fields(&[
            (
                "name",
                FieldKind::Text {
                    max_chars: Some(140),
                    format: TextFormat::Plain,
                },
                true,
                false,
            ),
            ("flag", FieldKind::Bool, false, false),
            (
                "count",
                FieldKind::Int {
                    format: IntFormat::Plain,
                },
                false,
                false,
            ),
            (
                "price",
                FieldKind::Decimal {
                    scale: 4,
                    precision: 18,
                    format: DecimalFormat::Plain,
                },
                false,
                false,
            ),
            ("due", FieldKind::Date, false, false),
            ("logged", FieldKind::Time, false, false),
            ("stamped", FieldKind::DateTime, false, false),
            (
                "extra",
                FieldKind::Json {
                    format: JsonFormat::Plain,
                },
                false,
                false,
            ),
            ("content", FieldKind::Bytes, false, false),
        ]),
    );
    def.version = 1;
    def
}

/// Model of an app controller that derives a field from others (ADR-51 phase 1):
/// `amount = qty * rate` on every save. It exists so `tests/patch_semantics.rs` scenario 8
/// can show what an engine-derived field does to a client patch that sends both.
struct DeriveAmount;

impl Controller for DeriveAmount {
    fn doctype(&self) -> &str {
        "sales_invoice"
    }

    fn steps(&self) -> Vec<StepDecl> {
        vec![StepDecl {
            name: ok(StepName::new("selling:derive_amount")),
            point: HookPoint::of(Phase::Before, Target::Save),
            priority: 0,
            budget: std::time::Duration::from_secs(1),
        }]
    }

    fn methods(&self) -> Vec<MethodDecl> {
        Vec::new()
    }

    fn run_step<'a>(
        &'a self,
        _step: &'a StepName,
        ctx: &'a mut dyn HookContext,
    ) -> spot_app_api::BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            let rows_with_index: Vec<(u32, Decimal, Decimal)> = {
                let doc = ctx.doc_mut()?;
                let qty = fn_name("qty");
                let rate = fn_name("rate");
                let items = fn_name("items");
                doc.rows(items.as_str())
                    .iter()
                    .enumerate()
                    .map(|(idx, row)| {
                        let decimal = |name: &Name| match row.fields.get(name) {
                            Some(Value::Decimal(d)) => *d,
                            _ => Decimal::ZERO,
                        };
                        let index = u32::try_from(idx + 1).unwrap_or(u32::MAX);
                        (index, decimal(&qty), decimal(&rate))
                    })
                    .collect()
            };
            let amount = fn_name("amount");
            let items = fn_name("items");
            {
                let table = ctx.doc_mut()?.rows_mut(items);
                for (row, (index, q, r)) in table.iter_mut().zip(&rows_with_index) {
                    let _ = index;
                    // Real engines quantise derived money; the fixture rounds half-up to scale.
                    let derived = (*q * *r).round_dp(2);
                    row.fields.insert(amount.clone(), Value::Decimal(derived));
                }
            }
            // A non-fatal engine message, so the suites can assert the `messages` envelope.
            for (index, _, _) in rows_with_index {
                ctx.message(
                    Message::info("selling.amount_derived", "The row amount was recomputed.")
                        .on(spot_app_api::FieldPath::row("items", index, "amount")),
                );
            }
            Ok(())
        })
    }
}

/// Whitelists methods without steps (the suites never run pipeline steps).
struct Methods {
    doctype: &'static str,
    list: &'static [(&'static str, bool)],
}

impl Controller for Methods {
    fn doctype(&self) -> &str {
        self.doctype
    }

    fn steps(&self) -> Vec<StepDecl> {
        Vec::new()
    }

    fn methods(&self) -> Vec<MethodDecl> {
        self.list
            .iter()
            .map(|(name, mutates)| MethodDecl::new(fn_name(name), *mutates))
            .collect()
    }

    fn run_step<'a>(
        &'a self,
        _step: &'a StepName,
        _ctx: &'a mut dyn HookContext,
    ) -> spot_app_api::BoxFuture<'a, AppResult<()>> {
        Box::pin(async {
            Err(AppError::new(
                ErrorClass::MethodNotFound,
                "method.not_found",
                "no steps",
            ))
        })
    }
}

fn kind_meta(kind: &FieldKind) -> (FieldType, Option<u8>, Option<Name>, Option<Name>) {
    match kind {
        FieldKind::Bool => (FieldType::Bool, None, None, None),
        FieldKind::Int { .. } => (FieldType::Int, None, None, None),
        FieldKind::Decimal { scale, .. } => {
            (FieldType::Decimal { scale: *scale }, None, None, None)
        }
        FieldKind::Text { .. } | FieldKind::LongText { .. } | FieldKind::Secret => {
            (FieldType::Text, None, None, None)
        }
        FieldKind::Select { .. } => (FieldType::Text, None, None, None),
        FieldKind::Date => (FieldType::Date, None, None, None),
        FieldKind::Time => (FieldType::Time, None, None, None),
        FieldKind::DateTime => (FieldType::DateTime, None, None, None),
        FieldKind::Link { doctype } => (FieldType::Link, None, Some(doctype.clone()), None),
        FieldKind::DynamicLink { .. } | FieldKind::Attach { .. } => {
            (FieldType::Link, None, None, None)
        }
        FieldKind::Json { .. } => (FieldType::Json, None, None, None),
        FieldKind::Bytes => (FieldType::Bytes, None, None, None),
        FieldKind::Table { doctype } => (FieldType::Table, None, None, Some(doctype.clone())),
        FieldKind::TableMultiselect { doctype, .. } => {
            (FieldType::Table, None, None, Some(doctype.clone()))
        }
        // The kind is `#[non_exhaustive]` (ADR-068: later types and formats are additive); a
        // base type this fake does not know is served by its storage shape, which every codec
        // can encode.
        _ => (FieldType::Text, None, None, None),
    }
}

/// One format for every app-side format the contract names, as its 0.2.0 `format` strings,
/// plus the parameter object the format carries (`format_params` in the response).
fn format_of(kind: &FieldKind) -> (Option<String>, BTreeMap<String, Json>) {
    let mut params = BTreeMap::new();
    let format = match kind {
        FieldKind::Text { format, .. } => match format {
            TextFormat::Plain => None,
            TextFormat::Email => Some("email".to_owned()),
            TextFormat::Phone => Some("phone".to_owned()),
            TextFormat::Url => Some("url".to_owned()),
            TextFormat::Color => Some("color".to_owned()),
            TextFormat::Barcode { symbology } => {
                if let Some(symbology) = symbology {
                    params.insert("symbology".to_owned(), Json::String(symbology.clone()));
                }
                Some("barcode".to_owned())
            }
            // The format set grows additively (ADR-068 §2); an unknown one stays absent.
            _ => None,
        },
        FieldKind::LongText { format } => match format {
            spot_app_api::LongTextFormat::Plain => None,
            spot_app_api::LongTextFormat::Markdown => Some("markdown".to_owned()),
            spot_app_api::LongTextFormat::Html => Some("html".to_owned()),
            spot_app_api::LongTextFormat::Code { syntax } => {
                params.insert("syntax".to_owned(), Json::String(syntax.clone()));
                Some("code".to_owned())
            }
            _ => None,
        },
        FieldKind::Int { format } => match format {
            IntFormat::Plain => None,
            IntFormat::Duration => Some("duration".to_owned()),
            IntFormat::Rating { max } => {
                params.insert("max".to_owned(), Json::from(u64::from(*max)));
                Some("rating".to_owned())
            }
            _ => None,
        },
        FieldKind::Decimal { format, .. } => match format {
            DecimalFormat::Plain => None,
            DecimalFormat::Currency { currency_field } => {
                params.insert(
                    "currency_field".to_owned(),
                    Json::String(currency_field.as_str().to_owned()),
                );
                Some("currency".to_owned())
            }
            DecimalFormat::Percent => Some("percent".to_owned()),
            _ => None,
        },
        FieldKind::Json { format } => match format {
            JsonFormat::Plain => None,
            JsonFormat::Geolocation => Some("geolocation".to_owned()),
            _ => None,
        },
        FieldKind::Attach { format } => match format {
            spot_app_api::AttachFormat::File => None,
            spot_app_api::AttachFormat::Image => Some("image".to_owned()),
            spot_app_api::AttachFormat::Signature => Some("signature".to_owned()),
            _ => None,
        },
        _ => None,
    };
    (format, params)
}

fn lookup_meta(spec: &Option<ApiLookupSpec>) -> Option<spot_http::api::lookup::LookupSpec> {
    spec.as_ref()
        .map(|spec| spot_http::api::lookup::LookupSpec {
            seam: spec.seam.clone(),
            version: u64::from(spec.version),
            params: spec.params.clone(),
        })
}

fn hints_meta(hints: &ApiFieldHints) -> (FieldHints, bool) {
    let converted = FieldHints {
        description: hints.description.clone(),
        hidden: hints.hidden,
        in_list_view: hints.in_list_view,
        in_standard_filter: hints.in_standard_filter,
        no_copy: hints.no_copy,
        permlevel: hints.permlevel,
        control: hints
            .control
            .and_then(|c| spot_http::api::meta::ControlHint::parse(c.as_str())),
    };
    let changed = !converted.is_default();
    (converted, changed)
}

/// The contract's `DocTypeMeta` for a `DocTypeDef`: what `MetaService` answers.
pub fn def_meta(def: &DocTypeDef) -> DocTypeMeta {
    let list = def
        .fields
        .iter()
        .map(|field| {
            let (field_type, _, link_to, child_doctype) = kind_meta(&field.kind);
            // The decimal's scale lives in the field type (the codec reads it there) and is
            // mirrored into `scale` for the meta response.
            let scale = match &field_type {
                FieldType::Decimal { scale } => Some(*scale),
                _ => None,
            };
            let precision = match &field.kind {
                FieldKind::Decimal { precision, .. } => Some(*precision),
                _ => None,
            };
            let (link_type_field, link_field, select, max_chars) = match &field.kind {
                FieldKind::DynamicLink { type_field } => {
                    (Some(type_field.clone()), None, None, None)
                }
                FieldKind::TableMultiselect { link_field, .. } => {
                    (None, Some(link_field.clone()), None, None)
                }
                FieldKind::Select { values } => (None, None, Some(values.clone()), None),
                FieldKind::Text { max_chars, .. } => (None, None, None, *max_chars),
                _ => (None, None, None, None),
            };
            // The default, in the value encoding: a decimal default is its string, any other
            // default rides the same Values-table shapes the codec uses for responses.
            let default = field.default.as_ref().map(value_json_of);
            let (format, format_params) = format_of(&field.kind);
            let (hints, has_hints) = hints_meta(&field.hints);
            MetaField {
                name: field.name.clone(),
                label: field.name.as_str().to_owned(),
                field_type,
                required: field.required,
                read_only: def.is_posted && !field.allow_on_posted,
                scale,
                precision,
                link_to,
                link_type_field,
                child_doctype,
                link_field,
                select,
                max_chars,
                default,
                format,
                format_params,
                lookup: lookup_meta(&field.lookup),
                hints,
                has_hints,
                multiple: field.multiple,
            }
        })
        .collect();
    DocTypeMeta {
        doctype: def.name.clone(),
        label: def.label.clone(),
        version: def.version,
        is_posted: def.is_posted,
        fields: list,
    }
}

fn field_type_of(field: &FieldDef) -> FieldType {
    kind_meta(&field.kind).0
}

/// The wire value of a stored default, by the Values table (decimals as strings, everything
/// else as its JSON shape; `Value::Json` reopens its canonical text).
fn value_json_of(value: &Value) -> Json {
    match value {
        Value::Null => Json::Null,
        Value::Bool(b) => Json::from(*b),
        Value::Int(i) => Json::from(*i),
        Value::Decimal(d) => Json::String(d.to_string()),
        Value::Text(t) => Json::String(t.clone()),
        Value::Date(d) => Json::String(d.to_string()),
        Value::Time(t) => Json::String(t.to_string()),
        Value::DateTime(dt) => Json::String(spot_http::api::codec::encode_datetime(dt)),
        Value::Id(id) => Json::String(id.to_string()),
        Value::Json(text) => serde_json::from_str(text).unwrap_or(Json::Null),
        Value::Bytes(b) => Json::String(base64_encode(b)),
        Value::GeoPoint(_) | Value::Range(_) => Json::String(format!("{value:?}")),
    }
}

fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

impl FakeApp {
    /// The `peer` of every request context seen so far, oldest first.
    // effects-allow: IpAddr is a plain value type, no I/O
    pub fn seen_peers(&self) -> Vec<Option<std::net::IpAddr>> {
        self.seen_peers
            .lock()
            .map(|v| v.clone())
            .unwrap_or_default()
    }

    pub(super) fn note_peer(&self, ctx: &RequestCtx) {
        if let Ok(mut peers) = self.seen_peers.lock() {
            peers.push(ctx.peer);
        }
    }

    /// The seeded fake: three logged-in tokens, the DocTypes above, one method whitelist and
    /// the derived-amount controller.
    pub fn new(clock: Arc<dyn Clock>) -> Arc<Self> {
        let mut fake = FakeApp {
            clock,
            ..FakeApp::default()
        };
        for (login, creds) in [
            (
                ALICE,
                UserCreds {
                    password: "correct horse",
                    totp_code: None,
                    recovery: None,
                    disabled: false,
                },
            ),
            (
                BOB,
                UserCreds {
                    password: "battery staple",
                    totp_code: None,
                    recovery: None,
                    disabled: false,
                },
            ),
            (
                DAVE,
                UserCreds {
                    password: "hunter2",
                    totp_code: None,
                    recovery: None,
                    disabled: true,
                },
            ),
            (
                EVE,
                UserCreds {
                    password: "letmein",
                    totp_code: Some("918273"),
                    recovery: Some("4111-1qaz"),
                    disabled: false,
                },
            ),
        ] {
            fake.users.insert(login.to_owned(), creds);
        }
        for def in [
            customer_type_with_seams(),
            item_type(),
            invoice_item_type(),
            with_seams(invoice_type(), &[("customer", SEAM_FIXED)]),
            with_seams(item_type(), &[("name", SEAM_BROKEN)]),
            doc_note_type(),
        ] {
            let meta = def_meta(&def);
            let title = def
                .title_field
                .as_ref()
                .map(|title| title.as_str().to_owned())
                .unwrap_or_default();
            fake.title_fields
                .insert(meta.doctype.as_str().to_owned(), title);
            fake.host.define(def);
            lock(&fake.metas).insert(meta.doctype.as_str().to_owned(), meta);
        }
        fake.host.register(Arc::new(Methods {
            doctype: "sales_invoice",
            list: &[("totals", true)],
        }));
        fake.host.register(Arc::new(DeriveAmount));
        {
            let mut sessions = lock(&fake.sessions);
            for (token, login) in [
                (TOKEN_ALICE, ALICE),
                (TOKEN_BOB, BOB),
                (TOKEN_DAVE, DAVE),
                (TOKEN_EVE, EVE),
            ] {
                sessions.insert(
                    token.to_owned(),
                    SessionEntry {
                        user: FakeApp::user_info(login, RecordId::new_v7()),
                    },
                );
            }
        }
        Arc::new(fake)
    }

    fn user_info(login: &str, id: RecordId) -> UserInfo {
        UserInfo {
            id,
            email: login.to_owned(),
            full_name: login.to_owned(),
        }
    }

    /// The session user behind a seeded token, for the suites to assert identity.
    pub fn session_user(&self, token: &str) -> Option<UserInfo> {
        lock(&self.sessions)
            .get(token)
            .map(|entry| entry.user.clone())
    }

    /// Every served document carries this `_currency` entry from now on.
    pub fn set_currency(&self, path: &str, code: &str, symbol: &str) {
        lock(&self.currency).insert(
            path.to_owned(),
            spot_http::api::codec::CurrencyInfo {
                code: code.to_owned(),
                symbol: symbol.to_owned(),
            },
        );
    }

    /// Serves this field with `multiple` and a control hint.
    pub fn tweak_meta(&self, tweak: MetaTweak) {
        lock(&self.meta_tweaks).push(tweak);
    }

    /// How many documents of a doctype the fake stored (concurrency/idempotency asserts).
    pub fn stored_count_of(&self, doctype: &str) -> usize {
        lock(&self.order)
            .iter()
            .filter(|(kind, _)| kind == doctype)
            .count()
    }

    /// A stored document, straight from the stub host's committed store; suites use it to
    /// assert what really persisted.
    pub fn stored_doc(&self, doctype: &str, id: RecordId) -> Option<Document> {
        self.host.stored(doctype, id)
    }

    /// Every stored document of a DocType (debug dumps).
    pub fn stored_docs(&self, doctype: &str) -> Vec<Document> {
        lock(&self.order)
            .iter()
            .filter(|(kind, _)| kind == doctype)
            .filter_map(|(_, id)| self.host.stored(doctype, *id))
            .collect()
    }

    /// The contract meta of a seeded DocType, for the in-process codec properties.
    pub fn meta_of(&self, doctype: &str) -> DocTypeMeta {
        lock(&self.metas).get(doctype).must().clone()
    }

    /// The contract meta of a seeded DocType, or `None` when unknown: the lookup service
    /// answers `meta.unknown_doctype` for a DocType the host does not know instead of
    /// panicking on a fixture.
    pub fn try_meta_of(&self, doctype: &str) -> Option<DocTypeMeta> {
        lock(&self.metas).get(doctype).cloned()
    }

    fn invocation(&self, ctx: &RequestCtx) -> Invocation {
        self.note_peer(ctx);
        let user = ctx
            .credential
            .as_ref()
            .and_then(|credential| self.session_user(credential.token.as_str()));
        let actor = match user {
            Some(user) => Actor::User {
                id: user.id,
                via: None,
            },
            None => Actor::System {
                component: fn_name("http"),
            },
        };
        let command = ctx
            .idempotency_key
            .clone()
            .unwrap_or_else(|| format!("http-{}", ctx.request_id));
        Invocation {
            site: ctx.site.clone(),
            actor,
            command_id: Some(ok(CommandId::new(command.as_str()))),
            language: ctx.language.clone(),
            now: self.clock.now_utc(),
        }
    }

    /// Arms the injected `http.internal` failure the 5xx idempotency test consumes: the next
    /// `totals` method call fails with 500 instead of running.
    pub fn arm_internal_totals_failure(&self) {
        self.fail_next_totals.store(true, Ordering::SeqCst);
    }

    /// Registers a named lookup override for a test (ADR-068 §3.4): a DocType field whose
    /// spec names `seam` is answered by `override` instead of the platform default. The
    /// suites use one that fails, one that answers a fixed page, and one spy.
    pub fn register_lookup_override(&self, seam: &str, override_: LookupOverride) {
        lock(&self.lookup_overrides).insert(
            seam.to_owned(),
            Arc::new(LookupEntry {
                behaviour: override_,
                last: Mutex::new(None),
            }),
        );
    }

    /// The last `LookupRequestWire` a named override received, as the wire JSON.
    pub fn last_lookup_request(&self, seam: &str) -> Option<Json> {
        let entries = lock(&self.lookup_overrides);
        let entry = entries.get(seam)?;
        lock(&entry.last).clone()
    }

    fn take_armed_totals_failure(&self) -> bool {
        self.fail_next_totals.swap(false, Ordering::SeqCst)
    }
}

impl FakeApp {
    /// One JSON encoding for every scalar kind, straight from the codec's own rules (the
    /// Values table); the fake never invents its own JSON shape.
    fn value_json(&self, value: &Value) -> Json {
        let field_type = match value {
            Value::Null | Value::Text(_) => FieldType::Text,
            Value::Bool(_) => FieldType::Bool,
            Value::Int(_) => FieldType::Int,
            Value::Decimal(_) => FieldType::Decimal { scale: 0 },
            Value::Date(_) => FieldType::Date,
            Value::Time(_) => FieldType::Time,
            Value::DateTime(_) => FieldType::DateTime,
            Value::Id(_) => FieldType::Link,
            Value::Json(_) => FieldType::Json,
            Value::Bytes(_) => FieldType::Bytes,
            Value::GeoPoint(_) | Value::Range(_) => FieldType::Json,
        };
        let field = MetaField {
            name: fn_name("value"),
            label: "value".to_owned(),
            field_type,
            required: false,
            read_only: false,
            scale: None,
            precision: None,
            link_to: None,
            link_type_field: None,
            child_doctype: None,
            link_field: None,
            select: None,
            max_chars: None,
            default: None,
            format: None,
            format_params: BTreeMap::new(),
            lookup: None,
            hints: FieldHints::default(),
            has_hints: false,
            multiple: false,
        };
        spot_http::api::codec::encode_scalar(value, &field).must()
    }

    fn document_of(&self, meta: &DocTypeMeta, incoming: &ApiDocument, id: RecordId) -> Document {
        let mut document = Document::new(meta.doctype.clone(), id);
        for (name, value) in &incoming.fields {
            match value {
                FieldValue::Scalar(value) => {
                    document.set(name.clone(), value.clone());
                }
                FieldValue::Rows(rows) => {
                    *document.rows_mut(name.clone()) = rows
                        .iter()
                        .map(|row| ChildRow {
                            id: row.id.unwrap_or_else(RecordId::new_v7),
                            fields: row.fields.clone(),
                        })
                        .collect();
                }
            }
        }
        document
    }

    /// The API-layer document for a stored `Document` (`ResourceService::get` answers it).
    pub fn to_api(&self, doc: &Document) -> ApiDocument {
        let meta = self.meta_of(doc.doctype().as_str());
        let mut api = ApiDocument {
            id: Some(doc.id()),
            version: doc.version(),
            state: doc.state().map(|state| state.as_str().to_owned()),
            currency: lock(&self.currency).clone(),
            fields: BTreeMap::new(),
        };
        for field in &meta.fields {
            if field.child_doctype.is_some() {
                api.fields.insert(
                    field.name.clone(),
                    FieldValue::Rows(
                        doc.rows(field.name.as_str())
                            .iter()
                            .map(|row| ChildRowDoc {
                                id: Some(row.id),
                                fields: row.fields.clone(),
                            })
                            .collect(),
                    ),
                );
            } else {
                api.fields.insert(
                    field.name.clone(),
                    FieldValue::Scalar(doc.get(field.name.as_str()).clone()),
                );
            }
        }
        api
    }
}

impl FakeApp {
    fn ids_of(&self, doctype: &str) -> Vec<RecordId> {
        lock(&self.order)
            .iter()
            .rev()
            .filter(|(kind, _)| kind == doctype)
            .map(|(_, id)| *id)
            .collect()
    }

    fn track(&self, doctype: &str, document: &Document) {
        lock(&self.order).push((doctype.to_owned(), document.id()));
        if doctype == "customer"
            && let Value::Text(phone) = document.get("phone")
        {
            lock(&self.phones).insert(phone.clone(), document.id());
        }
    }

    fn untrack(&self, doctype: &str, id: RecordId, phone: Option<String>) {
        let mut order = lock(&self.order);
        let before = order.len();
        order.retain(|(kind, own)| !(kind == doctype && own == &id));
        debug_assert!(
            order.len() < before,
            "untracked a document that was never tracked"
        );
        drop(order);
        if let Some(phone) = phone {
            lock(&self.phones).remove(&phone);
        }
    }

    /// ADR-055 lets clients choose row ids but the stub host does not police them across
    /// documents; the fake does: a row id already owned by a *different* document is a
    /// `document.duplicate` (patch scenario 3). The ledger only grows — a deleted row's id
    /// stays claimed, which no test exercises.
    fn check_row_ids(&self, owner: RecordId, ids: &[RecordId]) -> Result<(), ApiError> {
        let owners = lock(&self.row_owners);
        for id in ids {
            if let Some(prev) = owners.get(id)
                && *prev != owner
            {
                return Err(ApiError::app(
                    "document.duplicate",
                    format!("row id `{id}` already belongs to another document"),
                ));
            }
        }
        Ok(())
    }

    fn claim_rows(&self, owner: RecordId, doc: &Document) {
        let mut owners = lock(&self.row_owners);
        for rows in doc.tables().values() {
            for row in rows {
                owners.insert(row.id, owner);
            }
        }
    }

    fn uphold_phone(
        &self,
        doctype: &str,
        id: RecordId,
        patch: &spot_http::api::codec::DocumentPatchApi,
    ) -> Result<(), ApiError> {
        if doctype != "customer" {
            return Ok(());
        }
        if let Some(Value::Text(phone)) = patch.fields.get(&fn_name("phone")) {
            let phones = lock(&self.phones);
            if let Some(other) = phones.get(phone)
                && *other != id
            {
                return Err(ApiError::app(
                    "document.duplicate",
                    format!("phone `{phone}` is already used by another customer"),
                ));
            }
        }
        Ok(())
    }
}
