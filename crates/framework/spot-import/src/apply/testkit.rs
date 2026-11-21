// Shared fixtures of the back half's tests: the stub host, invocations, items, the double
// hosts and the key index. Split from the test files so none passes the size cap.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rust_decimal::Decimal;
use spot_app_api::context::{Actor, DocumentService, Invocation, Outcome};
use spot_app_api::document::DocumentPatch;
use spot_app_api::error::{AppError, AppResult, ErrorClass};
use spot_app_api::prelude::{CommandId, Name, RecordId, SiteId, Value, Version};
use spot_app_api::stub::StubHost;
use spot_app_api::{BoxFuture, DocTypeDef, Document};
use time::OffsetDateTime;

use super::*;
use crate::convert::{ConversionOptions, Formats};
use crate::csv::{CsvSource, temp_file};
use crate::group::CellValue;
use crate::mapping::{MappingPlan, Target};
use crate::report::{ImportReport, ImportRowError};
use crate::source::{Limits, Row, RowSource};
use crate::stream::{ImportDocument, import_stream};
use crate::testmeta;

// ---------------------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------------------

/// The stub host with the test DocTypes (customer, posted sales invoice, its item child).
pub(super) fn host() -> StubHost {
    let host = StubHost::new();
    for doctype in testmeta::meta().doctypes() {
        host.define((**doctype).clone());
    }
    host
}

/// The host plus the `customer` document `invoice_item` links to (the stub checks links).
pub(super) fn host_with_link_target() -> StubHost {
    let host = host();
    let mut doc = Document::new(Name::new("customer").unwrap(), rid(9));
    doc.set(
        Name::new("name").unwrap(),
        Value::Text("Link Target".to_owned()),
    );
    doc.set(Name::new("rating").unwrap(), Value::Text("A".to_owned()));
    block_on(host.create(&invocation(), doc)).unwrap();
    host
}

/// A fixed invocation: the tests' business time is the epoch, as in the stream's fixtures.
pub(super) fn invocation() -> Invocation {
    Invocation {
        site: SiteId::new("test").unwrap(),
        actor: Actor::System {
            component: Name::new("importer").unwrap(),
        },
        command_id: Some(CommandId::new("caller-key").unwrap()),
        language: None,
        now: OffsetDateTime::UNIX_EPOCH,
    }
}

/// A deterministic record id (`n` in the last byte), so fixtures and assertions agree.
pub(super) fn rid(n: u8) -> RecordId {
    let mut bytes = [0u8; 16];
    bytes[0] = 0x01;
    bytes[6] = 0x70;
    bytes[8] = 0x80;
    bytes[15] = n;
    RecordId::from_bytes(bytes).unwrap()
}

pub(super) fn dec(text: &str) -> Decimal {
    Decimal::from_str_exact(text).unwrap()
}

/// Polls a ready future on a no-op waker: every host here answers without I/O, so a future
/// that stays pending is a bug the test should see.
pub(super) fn block_on<F: std::future::Future>(future: F) -> F::Output {
    use std::task::{Context, Poll, Waker};
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
    let mut future = Box::pin(future);
    for _ in 0..1_000 {
        if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
            return value;
        }
    }
    panic!("the stub host's futures are ready on the first poll");
}

pub(super) fn cell(row: u64, header: &str, column: usize, value: Value) -> CellValue {
    CellValue {
        row,
        header: header.to_owned(),
        column,
        target: Target::Field(header.to_owned()),
        value: Some(value),
        text: String::new(),
    }
}

/// One `customer` parent: the file's row, its name and rating.
pub(super) fn customer_item(row: u64, name: &str, rating: Option<&str>) -> ImportItem {
    let mut fields = vec![(
        "name".to_owned(),
        cell(row, "name", 0, Value::Text(name.to_owned())),
    )];
    if let Some(rating) = rating {
        fields.push((
            "rating".to_owned(),
            cell(row, "rating", 1, Value::Text(rating.to_owned())),
        ));
    }
    ImportItem::Document(ImportDocument {
        doctype: "customer".to_owned(),
        fields,
        children: Vec::new(),
        rows: vec![Row {
            number: row,
            cells: Vec::new(),
        }],
    })
}

/// One `sales_invoice` parent with a single item row.
pub(super) fn invoice_item(row: u64, number: &str, status: &str) -> ImportItem {
    let customer = rid(9);
    ImportItem::Document(ImportDocument {
        doctype: "sales_invoice".to_owned(),
        fields: vec![
            (
                "number".to_owned(),
                cell(row, "number", 0, Value::Text(number.to_owned())),
            ),
            (
                "customer".to_owned(),
                cell(row, "customer", 1, Value::Id(customer)),
            ),
            (
                "status".to_owned(),
                cell(row, "status", 2, Value::Text(status.to_owned())),
            ),
            (
                "total".to_owned(),
                cell(row, "total", 3, Value::Decimal(dec("10.00"))),
            ),
        ],
        children: vec![(
            "items".to_owned(),
            vec![vec![
                cell(row, "item", 4, Value::Text("bolt".to_owned())),
                cell(row, "qty", 5, Value::Decimal(dec("1.5"))),
            ]],
        )],
        rows: vec![Row {
            number: row,
            cells: Vec::new(),
        }],
    })
}

/// A mapping plan for `doctype` over auto-matched `headers`, as the stream's tests build one.
pub(super) fn plan(doctype: &str, headers: &[&str]) -> MappingPlan {
    let meta = testmeta::meta();
    let headers: Vec<String> = headers.iter().map(|h| (*h).to_owned()).collect();
    let (dates, datetimes, times) = Formats::default_lists();
    let formats = Formats::build(&dates, &datetimes, &times).unwrap();
    MappingPlan::build(
        &meta,
        doctype,
        &headers,
        &[],
        ConversionOptions::default(),
        formats,
    )
    .unwrap()
}

/// Streams a CSV of `customer` rows: the items plus the report the stream filled.
pub(super) fn stream_customers(name: &str, bytes: &[u8]) -> (Vec<ImportItem>, ImportReport) {
    let path = temp_file(name, bytes);
    let mut source = CsvSource::open(&path, &Limits::default()).unwrap();
    let meta = testmeta::meta();
    let mut report = ImportReport::default();
    let items: Vec<ImportItem> = import_stream(
        &mut source,
        plan("customer", &["name", "rating"]),
        &meta,
        &mut report,
    )
    .collect();
    (items, report)
}

/// The options for a run over the test metadata.
pub(super) fn options(mode: ApplyMode) -> ApplyOptions {
    let mut options = ApplyOptions::new(
        testmeta::meta(),
        std::sync::Arc::new(crate::apply::testing::AllowAll),
    );
    options.mode = mode;
    options.batch_label = "batch-test".to_owned();
    options.progress_every = 1;
    options
}

/// A `KeyIndex` over (doctype, field, value) → id, as a caller that already ran the query has.
#[derive(Default)]
pub(super) struct MapIndex {
    pub(super) rows: Mutex<Vec<(String, String, Value, RecordId)>>,
}

impl MapIndex {
    pub(super) fn add(&self, doctype: &str, field: &str, value: Value, id: RecordId) {
        self.rows
            .lock()
            .unwrap()
            .push((doctype.to_owned(), field.to_owned(), value, id));
    }
}

impl KeyIndex for MapIndex {
    fn find(&self, doctype: &str, key_field: &str, value: &Value) -> Option<RecordId> {
        self.rows
            .lock()
            .unwrap()
            .iter()
            .find(|(kind, field, held, _)| kind == doctype && field == key_field && held == value)
            .map(|(_, _, _, id)| *id)
    }
}

/// The progress recorder: every call, and an optional cancellation at a document count.
#[derive(Default)]
pub(super) struct Recorder {
    pub(super) calls: Vec<(u64, Option<u64>)>,
    pub(super) cancel_at: Option<(u64, CancelToken)>,
}

impl ProgressSink for Recorder {
    fn progress(&mut self, done: u64, total: Option<u64>) {
        self.calls.push((done, total));
        if let Some((at, token)) = &self.cancel_at
            && done >= *at
        {
            token.cancel();
        }
    }
}

/// A host double: delegates to the reference host, and can dedupe by command id (the engine's
/// promise, which the stub itself does not model) or fail a chosen write.
pub(super) struct Wrapped<'a> {
    pub(super) host: &'a StubHost,
    pub(super) fail_at: Option<usize>,
    pub(super) writes: AtomicUsize,
    pub(super) dedupe: bool,
    pub(super) creates: Mutex<BTreeMap<String, Document>>,
}

impl<'a> Wrapped<'a> {
    pub(super) fn failing(host: &'a StubHost, at: usize) -> Self {
        Self {
            host,
            fail_at: Some(at),
            writes: AtomicUsize::new(0),
            dedupe: false,
            creates: Mutex::new(BTreeMap::new()),
        }
    }

    pub(super) fn deduping(host: &'a StubHost) -> Self {
        Self {
            host,
            fail_at: None,
            writes: AtomicUsize::new(0),
            dedupe: true,
            creates: Mutex::new(BTreeMap::new()),
        }
    }

    pub(super) fn before_write(&self) -> AppResult<()> {
        let count = self.writes.fetch_add(1, Ordering::SeqCst) + 1;
        match self.fail_at {
            Some(at) if at == count => Err(AppError::new(
                ErrorClass::Unavailable,
                "server.unavailable",
                "the test host refused this write",
            )),
            _ => Ok(()),
        }
    }
}

impl DocumentService for Wrapped<'_> {
    fn meta<'a>(&'a self, doctype: &'a str) -> BoxFuture<'a, AppResult<Arc<DocTypeDef>>> {
        self.host.meta(doctype)
    }

    fn load<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        self.host.load(inv, doctype, id)
    }

    fn create<'a>(
        &'a self,
        inv: &'a Invocation,
        doc: Document,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        Box::pin(async move {
            let key = inv.command_id.as_ref().map(|id| id.as_str().to_owned());
            if self.dedupe
                && let Some(key) = &key
            {
                let first = self.creates.lock().unwrap().get(key).cloned();
                if let Some(first) = first {
                    return Ok(Outcome {
                        value: first,
                        messages: Vec::new(),
                    });
                }
            }
            self.before_write()?;
            let outcome = self.host.create(inv, doc).await?;
            if self.dedupe
                && let Some(key) = key
            {
                self.creates
                    .lock()
                    .unwrap()
                    .insert(key, outcome.value.clone());
            }
            Ok(outcome)
        })
    }

    fn update<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        patch: DocumentPatch,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        Box::pin(async move {
            self.before_write()?;
            self.host.update(inv, doctype, id, patch, expected).await
        })
    }

    fn post<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        Box::pin(async move {
            self.before_write()?;
            self.host.post(inv, doctype, id, expected).await
        })
    }

    fn cancel<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        self.host.cancel(inv, doctype, id, expected)
    }

    fn correct<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        self.host.correct(inv, doctype, id, expected)
    }

    fn delete<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<()>>> {
        Box::pin(async move {
            self.before_write()?;
            self.host.delete(inv, doctype, id, expected).await
        })
    }

    fn rename<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        new_name: &'a str,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        self.host.rename(inv, doctype, id, new_name, expected)
    }

    fn call_method<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        method: &'a str,
        args: serde_json::Value,
        expected: Option<Version>,
    ) -> BoxFuture<'a, AppResult<Outcome<(Document, serde_json::Value)>>> {
        self.host
            .call_method(inv, doctype, id, method, args, expected)
    }

    fn field_changed<'a>(
        &'a self,
        inv: &'a Invocation,
        draft: Document,
        field: &'a str,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        self.host.field_changed(inv, draft, field)
    }
}

/// Seeds one stored `customer` through the host; returns it as saved.
pub(super) fn seed_customer(host: &StubHost, n: u8, name: &str, rating: &str) -> Document {
    let mut doc = Document::new(Name::new("customer").unwrap(), rid(n));
    doc.set(Name::new("name").unwrap(), Value::Text(name.to_owned()));
    doc.set(Name::new("rating").unwrap(), Value::Text(rating.to_owned()));
    block_on(host.create(&invocation(), doc)).unwrap().value
}

/// The report's entry for `code`, if any.
pub(super) fn entry<'a>(summary: &'a ApplySummary, code: &str) -> Option<&'a ImportRowError> {
    summary.errors.iter().find(|error| error.code == code)
}
