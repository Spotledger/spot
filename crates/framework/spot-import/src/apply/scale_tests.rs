// The 100k-row run: the writer's own memory bound, measured on a host double that stores
// nothing.
//
// The reference host (`StubHost`) copies its whole store into every transaction, so it is a
// correctness harness, not a scale one (a 100k-row run through it is quadratic); the counting
// double stands in for a host that answers one call at a time. What the test proves is the
// writer's side: it pulls one parent, applies it, and only then pulls the next, and it never
// buffers a parent because the streaming policy does not spool.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use spot_app_api::context::{Actor, DocumentService, Invocation, Outcome};
use spot_app_api::document::DocumentPatch;
use spot_app_api::error::{AppError, AppResult, ErrorClass};
use spot_app_api::prelude::Version;
use spot_app_api::prelude::{Name, RecordId, SiteId, Value};
use spot_app_api::{BoxFuture, DocTypeDef, Document};
use time::OffsetDateTime;

use super::*;
use crate::group::CellValue;
use crate::mapping::Target;
use crate::source::Row;
use crate::stream::ImportDocument;
use crate::testmeta;

/// A fixed invocation (the tests' business time is the epoch).
fn invocation() -> Invocation {
    Invocation {
        site: SiteId::new("test").unwrap(),
        actor: Actor::System {
            component: Name::new("importer").unwrap(),
        },
        command_id: None,
        language: None,
        now: OffsetDateTime::UNIX_EPOCH,
    }
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    use std::task::{Context, Poll, Waker};
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
    let mut future = Box::pin(future);
    for _ in 0..1_000 {
        if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
            return value;
        }
    }
    panic!("the counting host's futures are ready on the first poll");
}

/// One `customer` parent, built by hand so the run measures the writer, not the file reader.
fn customer_item(row: u64) -> ImportItem {
    ImportItem::Document(ImportDocument {
        doctype: "customer".to_owned(),
        fields: vec![(
            "name".to_owned(),
            CellValue {
                row,
                header: "name".to_owned(),
                column: 0,
                target: Target::Field("name".to_owned()),
                value: Some(Value::Text(format!("C{row}"))),
                text: String::new(),
            },
        )],
        children: Vec::new(),
        rows: vec![Row {
            number: row,
            cells: Vec::new(),
        }],
    })
}

/// A lazy item stream that counts how many parents were pulled.
struct CountingItems {
    next: u64,
    end: u64,
    pulls: Arc<AtomicU64>,
}

impl Iterator for CountingItems {
    type Item = ImportItem;

    fn next(&mut self) -> Option<ImportItem> {
        if self.next >= self.end {
            return None;
        }
        let row = self.next;
        self.next += 1;
        self.pulls.fetch_add(1, Ordering::SeqCst);
        Some(customer_item(row))
    }
}

/// A host that stores nothing: it counts creates and records how many parents the writer had
/// pulled when the first create arrived.
struct CountingService {
    creates: AtomicU64,
    pulls: Arc<AtomicU64>,
    pulled_at_first_create: Mutex<Option<u64>>,
}

impl CountingService {
    fn new(pulls: Arc<AtomicU64>) -> Self {
        Self {
            creates: AtomicU64::new(0),
            pulls,
            pulled_at_first_create: Mutex::new(None),
        }
    }
}

fn unsupported<T: Send + 'static>() -> BoxFuture<'static, AppResult<T>> {
    Box::pin(async {
        Err(AppError::new(
            ErrorClass::Internal,
            "platform.unsupported",
            "the counting host only creates",
        ))
    })
}

impl DocumentService for CountingService {
    fn meta<'a>(&'a self, _doctype: &'a str) -> BoxFuture<'a, AppResult<Arc<DocTypeDef>>> {
        unsupported()
    }

    fn load<'a>(
        &'a self,
        _inv: &'a Invocation,
        _doctype: &'a str,
        _id: RecordId,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        unsupported()
    }

    fn create<'a>(
        &'a self,
        _inv: &'a Invocation,
        doc: Document,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        Box::pin(async move {
            let created = self.creates.fetch_add(1, Ordering::SeqCst) + 1;
            if created == 1 {
                let pulled = self.pulls.load(Ordering::SeqCst);
                *self.pulled_at_first_create.lock().unwrap() = Some(pulled);
            }
            Ok(Outcome {
                value: doc,
                messages: Vec::new(),
            })
        })
    }

    fn update<'a>(
        &'a self,
        _inv: &'a Invocation,
        _doctype: &'a str,
        _id: RecordId,
        _patch: DocumentPatch,
        _expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        unsupported()
    }

    fn post<'a>(
        &'a self,
        _inv: &'a Invocation,
        _doctype: &'a str,
        _id: RecordId,
        _expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        unsupported()
    }

    fn cancel<'a>(
        &'a self,
        _inv: &'a Invocation,
        _doctype: &'a str,
        _id: RecordId,
        _expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        unsupported()
    }

    fn correct<'a>(
        &'a self,
        _inv: &'a Invocation,
        _doctype: &'a str,
        _id: RecordId,
        _expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        unsupported()
    }

    fn delete<'a>(
        &'a self,
        _inv: &'a Invocation,
        _doctype: &'a str,
        _id: RecordId,
        _expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<()>>> {
        unsupported()
    }

    fn rename<'a>(
        &'a self,
        _inv: &'a Invocation,
        _doctype: &'a str,
        _id: RecordId,
        _new_name: &'a str,
        _expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        unsupported()
    }

    fn call_method<'a>(
        &'a self,
        _inv: &'a Invocation,
        _doctype: &'a str,
        _id: RecordId,
        _method: &'a str,
        _args: serde_json::Value,
        _expected: Option<Version>,
    ) -> BoxFuture<'a, AppResult<Outcome<(Document, serde_json::Value)>>> {
        unsupported()
    }

    fn field_changed<'a>(
        &'a self,
        _inv: &'a Invocation,
        _draft: Document,
        _field: &'a str,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        unsupported()
    }
}

/// Progress calls, counted (the writer reports every `progress_every` documents and at the end).
#[derive(Default)]
struct Calls {
    calls: Vec<(u64, Option<u64>)>,
}

impl ProgressSink for Calls {
    fn progress(&mut self, done: u64, total: Option<u64>) {
        self.calls.push((done, total));
    }
}

/// 100k parents, one at a time: one parent held, no spool, and the id list as the only growth.
#[test]
fn one_hundred_thousand_rows_stream_in_bounded_memory() {
    const ROWS: u64 = 100_000;
    let pulls = Arc::new(AtomicU64::new(0));
    let service = CountingService::new(Arc::clone(&pulls));
    let mut options = ApplyOptions::new(
        testmeta::meta(),
        std::sync::Arc::new(crate::apply::testing::AllowAll),
    );
    options.mode = ApplyMode::Insert;
    options.batch_label = "scale".to_owned();
    options.progress_every = 10_000;
    let mut progress = Calls::default();
    let outcome = block_on(apply_import(
        &service,
        &invocation(),
        CountingItems {
            next: 2,
            end: 2 + ROWS,
            pulls: Arc::clone(&pulls),
        },
        &options,
        &mut progress,
    ));
    let summary = outcome.summary();
    assert_eq!(summary.created, ROWS, "{summary:?}");
    assert_eq!(summary.failed, 0);
    assert_eq!(summary.skipped, 0);
    assert_eq!(
        summary.created_drafts.len() as u64,
        ROWS,
        "the id list is the only growth (a rollback needs it)"
    );
    assert_eq!(
        options.spool.len(),
        0,
        "the streaming policy never buffers a parent"
    );
    assert_eq!(
        pulls.load(Ordering::SeqCst),
        ROWS,
        "every parent pulled once"
    );
    assert_eq!(
        *service.pulled_at_first_create.lock().unwrap(),
        Some(1),
        "the writer pulls the next parent only after applying the previous one"
    );
    assert_eq!(progress.calls.len(), 11, "{:?}", progress.calls);
    assert_eq!(progress.calls.last(), Some(&(ROWS, None)));
}
