//! In-memory fakes for the contract 0.5.0 services: the permission summary (a table and a
//! snapshot version) and field reveal (a table of revealable values that records what it was
//! asked). They compute nothing the real services will; they hold what a test put there.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value as Json;
use spot_http::api::permissions::{
    AttachLimit, DoctypePermissions, FieldSummary, PermissionSummary, Rights,
};
use spot_http::api::registry::Code;
use spot_http::api::service::{ApiError, BoxFuture, PermissionService, RequestCtx, RevealService};
use spot_storage_contract::{Name, RecordId};

use crate::support::Must;
use crate::support::fake::lock;

/// Everything on, nothing masked: the baseline entry of a DocType.
pub fn open_entry(doctype: &str) -> DoctypePermissions {
    DoctypePermissions {
        doctype: Name::new(doctype).must(),
        rights: Rights {
            read: true,
            create: true,
            write: true,
            delete: true,
            submit: true,
            cancel: true,
            correct: true,
            rename: true,
        },
        fields: FieldSummary::default(),
        attach_limits: BTreeMap::new(),
    }
}

/// The permission fake: a table of DocType entries and a snapshot version.
pub struct FakePermissions {
    table: Mutex<BTreeMap<String, DoctypePermissions>>,
    version: AtomicU64,
}

impl FakePermissions {
    /// Seeds `customer` (a masked, revealable `phone`) and `sales_invoice` (a read-only
    /// total), at snapshot version 42.
    pub fn new() -> FakePermissions {
        let mut customer = open_entry("customer");
        customer.fields.masked = vec!["phone".into()];
        customer.fields.revealable = vec!["phone".into()];
        let mut invoice = open_entry("sales_invoice");
        invoice.fields.read_only = vec!["grand_total".into()];
        invoice.attach_limits.insert(
            "scan".into(),
            AttachLimit {
                max_bytes: 1_048_576,
                accept: Some(vec!["application/pdf".into(), "image/*".into()]),
            },
        );
        let table = [customer, invoice]
            .into_iter()
            .map(|entry| (entry.doctype.as_str().to_owned(), entry))
            .collect();
        FakePermissions {
            table: Mutex::new(table),
            version: AtomicU64::new(42),
        }
    }

    /// Replaces one DocType's entry and moves the snapshot version.
    pub fn set(&self, entry: DoctypePermissions) {
        lock(&self.table).insert(entry.doctype.as_str().to_owned(), entry);
        self.version.fetch_add(1, Ordering::SeqCst);
    }

    /// The current snapshot version.
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::SeqCst)
    }
}

impl PermissionService for FakePermissions {
    fn summary<'a>(
        &'a self,
        _ctx: &'a RequestCtx,
        doctypes: &'a [Name],
    ) -> BoxFuture<'a, Result<PermissionSummary, ApiError>> {
        Box::pin(async move {
            let table = lock(&self.table);
            let entries = if doctypes.is_empty() {
                table.values().cloned().collect()
            } else {
                let mut out = Vec::new();
                for name in doctypes {
                    match table.get(name.as_str()) {
                        Some(entry) => out.push(entry.clone()),
                        None => return Err(ApiError::from_code(Code::MetaUnknownDoctype)),
                    }
                }
                out
            };
            Ok(PermissionSummary {
                doctypes: entries,
                version: self.version(),
            })
        })
    }
}

/// What a reveal call asked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevealCall {
    /// The DocType.
    pub doctype: String,
    /// The document.
    pub id: RecordId,
    /// The field path.
    pub field: String,
    /// The stated reason.
    pub reason: String,
    /// The `Idempotency-Key` the service saw (always `None`: the route ignores it).
    pub idempotency_key: Option<String>,
}

/// The reveal fake. The four refusal causes are modelled as four different service errors so
/// a test can show the router answers them identically.
pub struct FakeReveal {
    values: Mutex<BTreeMap<(String, String), Json>>,
    documents: Mutex<BTreeSet<RecordId>>,
    unreadable: Mutex<BTreeSet<RecordId>>,
    holders: Mutex<BTreeSet<String>>,
    calls: Mutex<Vec<RevealCall>>,
}

impl FakeReveal {
    /// An empty fake: nobody holds the right, nothing is revealable.
    pub fn new() -> FakeReveal {
        FakeReveal {
            values: Mutex::new(BTreeMap::new()),
            documents: Mutex::new(BTreeSet::new()),
            unreadable: Mutex::new(BTreeSet::new()),
            holders: Mutex::new(BTreeSet::new()),
            calls: Mutex::new(Vec::new()),
        }
    }

    /// Makes `doctype.field` revealable with this value.
    pub fn allow_field(&self, doctype: &str, field: &str, value: Json) {
        lock(&self.values).insert((doctype.to_owned(), field.to_owned()), value);
    }

    /// A document that exists.
    pub fn add_document(&self, id: RecordId) {
        lock(&self.documents).insert(id);
    }

    /// A document that exists but the caller may not read.
    pub fn add_unreadable(&self, id: RecordId) {
        lock(&self.documents).insert(id);
        lock(&self.unreadable).insert(id);
    }

    /// Gives the reveal right to the holder of this bearer token.
    pub fn grant(&self, token: &str) {
        lock(&self.holders).insert(token.to_owned());
    }

    /// Every call the service received, in order.
    pub fn calls(&self) -> Vec<RevealCall> {
        lock(&self.calls).clone()
    }
}

impl RevealService for FakeReveal {
    fn reveal<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
        field: &'a str,
        reason: &'a str,
    ) -> BoxFuture<'a, Result<Json, ApiError>> {
        Box::pin(async move {
            lock(&self.calls).push(RevealCall {
                doctype: doctype.to_owned(),
                id,
                field: field.to_owned(),
                reason: reason.to_owned(),
                idempotency_key: ctx.idempotency_key.clone(),
            });
            let token = ctx
                .credential
                .as_ref()
                .map(|c| c.token.clone())
                .unwrap_or_default();
            if !lock(&self.holders).contains(&token) {
                return Err(ApiError::new(Code::PermissionDenied, "no reveal right")
                    .explained("the caller lacks the right"));
            }
            let value = lock(&self.values)
                .get(&(doctype.to_owned(), field.to_owned()))
                .cloned();
            let Some(value) = value else {
                return Err(
                    ApiError::new(Code::PermissionRevealDenied, "not revealable")
                        .detail("why", Json::from("field")),
                );
            };
            if !lock(&self.documents).contains(&id) {
                return Err(ApiError::new(Code::DocumentNotFound, "no such document"));
            }
            if lock(&self.unreadable).contains(&id) {
                return Err(ApiError::new(Code::PermissionFieldDenied, "unreadable"));
            }
            Ok(value)
        })
    }
}
