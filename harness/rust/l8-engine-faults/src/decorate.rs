//! Test-only `DocumentService` decorators, so the machinery runs on any machine and a green run
//! can be shown to turn red. None of them changes the engine.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value as Json;
use spot_app_api::context::{DocumentService, Invocation, Outcome};
use spot_app_api::prelude::{RecordId, Version};
use spot_app_api::{
    AppError, AppResult, BoxFuture, DocTypeDef, Document, DocumentPatch, ErrorClass,
};

/// The error a lost commit reply surfaces as.
pub fn lost_reply() -> AppError {
    AppError::new(
        ErrorClass::Unavailable,
        "server.unavailable",
        "The service is temporarily unavailable; retry with the same request id.",
    )
    .detail("storage_code", "storage.commit_outcome_unknown")
}

/// Forwards every call, and for every `every`-th **write** that succeeded throws the reply away
/// and answers a lost-reply error: the write happened, the client never hears.
pub struct LostReplies<S> {
    inner: Arc<S>,
    every: u64,
    counter: AtomicU64,
}

impl<S> LostReplies<S> {
    /// A decorator that loses one reply in `every`.
    pub fn new(inner: Arc<S>, every: u64) -> Self {
        Self {
            inner,
            every: every.max(1),
            counter: AtomicU64::new(0),
        }
    }

    fn lose<T>(&self, r: AppResult<T>) -> AppResult<T> {
        if r.is_ok()
            && self
                .counter
                .fetch_add(1, Ordering::SeqCst)
                .is_multiple_of(self.every)
        {
            Err(lost_reply())
        } else {
            r
        }
    }
}

impl<S: DocumentService + 'static> DocumentService for LostReplies<S> {
    fn meta<'a>(&'a self, doctype: &'a str) -> BoxFuture<'a, AppResult<Arc<DocTypeDef>>> {
        self.inner.meta(doctype)
    }
    fn load<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        self.inner.load(inv, doctype, id)
    }
    fn create<'a>(
        &'a self,
        inv: &'a Invocation,
        doc: Document,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        Box::pin(async move { self.lose(self.inner.create(inv, doc).await) })
    }
    fn update<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        patch: DocumentPatch,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        Box::pin(
            async move { self.lose(self.inner.update(inv, doctype, id, patch, expected).await) },
        )
    }
    fn post<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        Box::pin(async move { self.lose(self.inner.post(inv, doctype, id, expected).await) })
    }
    fn cancel<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        Box::pin(async move { self.lose(self.inner.cancel(inv, doctype, id, expected).await) })
    }
    fn correct<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        Box::pin(async move { self.lose(self.inner.correct(inv, doctype, id, expected).await) })
    }
    fn delete<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<()>>> {
        Box::pin(async move { self.lose(self.inner.delete(inv, doctype, id, expected).await) })
    }
    fn rename<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        new_name: &'a str,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        self.inner.rename(inv, doctype, id, new_name, expected)
    }
    fn call_method<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        method: &'a str,
        args: Json,
        expected: Option<Version>,
    ) -> BoxFuture<'a, AppResult<Outcome<(Document, Json)>>> {
        self.inner
            .call_method(inv, doctype, id, method, args, expected)
    }
    fn field_changed<'a>(
        &'a self,
        inv: &'a Invocation,
        draft: Document,
        field: &'a str,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        self.inner.field_changed(inv, draft, field)
    }
}

/// Forwards every call except every `every`-th **create**, which it answers with a committed
/// document it never sent: the lie of a client that claims a commit it did not make.
pub struct Liar<S> {
    inner: Arc<S>,
    every: u64,
    counter: AtomicU64,
}

impl<S> Liar<S> {
    /// A decorator that lies about one create in `every`.
    pub fn new(inner: Arc<S>, every: u64) -> Self {
        Self {
            inner,
            every: every.max(1),
            counter: AtomicU64::new(0),
        }
    }
}

impl<S: DocumentService + 'static> DocumentService for Liar<S> {
    fn meta<'a>(&'a self, doctype: &'a str) -> BoxFuture<'a, AppResult<Arc<DocTypeDef>>> {
        self.inner.meta(doctype)
    }
    fn load<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        self.inner.load(inv, doctype, id)
    }
    fn create<'a>(
        &'a self,
        inv: &'a Invocation,
        doc: Document,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        Box::pin(async move {
            if self.counter.fetch_add(1, Ordering::SeqCst) % self.every == self.every - 1 {
                return Ok(Outcome {
                    value: doc,
                    messages: Vec::new(),
                });
            }
            self.inner.create(inv, doc).await
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
        self.inner.update(inv, doctype, id, patch, expected)
    }
    fn post<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        self.inner.post(inv, doctype, id, expected)
    }
    fn cancel<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        self.inner.cancel(inv, doctype, id, expected)
    }
    fn correct<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        self.inner.correct(inv, doctype, id, expected)
    }
    fn delete<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<()>>> {
        self.inner.delete(inv, doctype, id, expected)
    }
    fn rename<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        new_name: &'a str,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        self.inner.rename(inv, doctype, id, new_name, expected)
    }
    fn call_method<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        method: &'a str,
        args: Json,
        expected: Option<Version>,
    ) -> BoxFuture<'a, AppResult<Outcome<(Document, Json)>>> {
        self.inner
            .call_method(inv, doctype, id, method, args, expected)
    }
    fn field_changed<'a>(
        &'a self,
        inv: &'a Invocation,
        draft: Document,
        field: &'a str,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        self.inner.field_changed(inv, draft, field)
    }
}
