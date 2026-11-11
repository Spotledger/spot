//! A document service that answers a repeated command id as a replay, the way the command
//! ledger above the document engine does: the write is not run again and the committed document
//! is returned, so no `TxParticipant` runs.
#![allow(dead_code, missing_docs)]

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use serde_json::Value as Json;
use spot_app_api::context::{DocumentService, Invocation, Outcome};
use spot_app_api::document::{Document, DocumentPatch};
use spot_app_api::meta::DocTypeDef;
use spot_app_api::prelude::{RecordId, Version};
use spot_app_api::{AppResult, BoxFuture};

pub struct Replaying<S> {
    inner: Arc<S>,
    seen: Mutex<BTreeSet<String>>,
}

impl<S> Replaying<S> {
    /// A service whose ledger already holds `committed` command ids (a previous process).
    pub fn seeded(inner: Arc<S>, committed: &[&str]) -> Self {
        Self {
            inner,
            seen: Mutex::new(committed.iter().map(|k| (*k).to_owned()).collect()),
        }
    }

    /// True when this call is a replay; otherwise records the command id.
    fn replay(&self, inv: &Invocation) -> bool {
        let Some(command) = inv.command_id.as_ref() else {
            return false;
        };
        !self
            .seen
            .lock()
            .unwrap()
            .insert(command.as_str().to_owned())
    }
}

impl<S: DocumentService> DocumentService for Replaying<S> {
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
        if self.replay(inv) {
            return Box::pin(async move {
                let doctype = doc.doctype().clone();
                self.inner.load(inv, doctype.as_str(), doc.id()).await
            });
        }
        self.inner.create(inv, doc)
    }

    fn update<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        patch: DocumentPatch,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        if self.replay(inv) {
            return self.inner.load(inv, doctype, id);
        }
        self.inner.update(inv, doctype, id, patch, expected)
    }

    fn post<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        if self.replay(inv) {
            return self.inner.load(inv, doctype, id);
        }
        self.inner.post(inv, doctype, id, expected)
    }

    fn cancel<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        if self.replay(inv) {
            return self.inner.load(inv, doctype, id);
        }
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
