//! A `DocumentService` double with the engine's observable behaviour that bulk actions rely on:
//! versions, states, the per-command ledger, stale and invalid-state refusals, injected errors,
//! hidden documents, in-flight counting and call recording. Nothing here is compiled by the
//! writer of the tests; the bundle compiles it.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::Value as Json;
use spot_app_api::context::{Actor, DocumentService, Invocation, Outcome};
use spot_app_api::document::{DocState, Document, DocumentPatch};
use spot_app_api::error::{AppError, AppResult, ErrorClass};
use spot_app_api::prelude::{Name, OffsetDateTime, RecordId, SiteId, Version};
use spot_app_api::{BoxFuture, DocTypeDef};

pub const DOCTYPE: &str = "invoice";

pub fn rid(n: u8) -> RecordId {
    let mut bytes = [0u8; 16];
    bytes[0] = 0x01;
    bytes[6] = 0x70;
    bytes[8] = 0x80;
    bytes[15] = n;
    RecordId::from_bytes(bytes).unwrap()
}

pub fn invocation() -> Invocation {
    Invocation {
        site: SiteId::new("test").unwrap(),
        actor: Actor::System {
            component: Name::new("bulk").unwrap(),
        },
        command_id: None,
        language: None,
        now: OffsetDateTime::UNIX_EPOCH,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Update,
    Post,
    Cancel,
    Correct,
    Delete,
    Method,
}

#[derive(Clone, Copy, Debug)]
struct Rec {
    version: u64,
    state: Option<DocState>,
}

struct Written {
    id: RecordId,
    version: u64,
    state: Option<DocState>,
}

/// A ledger row: the document, its version and state after the call.
type LedgerRow = (RecordId, u64, Option<DocState>);

/// The double. Build with [`Fake::new`], add documents, then run.
pub struct Fake {
    docs: Mutex<BTreeMap<RecordId, Rec>>,
    ledger: Mutex<HashMap<String, LedgerRow>>,
    injected: Mutex<HashMap<RecordId, AppError>>,
    hidden: Mutex<HashSet<RecordId>>,
    /// A write call panics (proves a dry run never writes).
    pub panic_on_write: bool,
    /// Calls complete in reverse order of the last byte of the id.
    pub reverse_completion: bool,
    /// Extra yields every non-injected write does.
    pub yields: usize,
    pub writes: AtomicUsize,
    pub loads: AtomicUsize,
    /// State changes actually applied (a ledger replay does not count).
    pub applied: AtomicUsize,
    in_flight: AtomicUsize,
    pub max_in_flight: AtomicUsize,
    pub command_ids: Mutex<Vec<String>>,
}

impl Fake {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::plain())
    }

    pub fn plain() -> Self {
        Self {
            docs: Mutex::new(BTreeMap::new()),
            ledger: Mutex::new(HashMap::new()),
            injected: Mutex::new(HashMap::new()),
            hidden: Mutex::new(HashSet::new()),
            panic_on_write: false,
            reverse_completion: false,
            yields: 0,
            writes: AtomicUsize::new(0),
            loads: AtomicUsize::new(0),
            applied: AtomicUsize::new(0),
            in_flight: AtomicUsize::new(0),
            max_in_flight: AtomicUsize::new(0),
            command_ids: Mutex::new(Vec::new()),
        }
    }

    pub fn add(&self, n: u8, state: Option<DocState>) -> RecordId {
        let id = rid(n);
        self.docs
            .lock()
            .unwrap()
            .insert(id, Rec { version: 1, state });
        id
    }

    pub fn add_drafts(&self, range: std::ops::RangeInclusive<u8>) -> Vec<RecordId> {
        range.map(|n| self.add(n, Some(DocState::Draft))).collect()
    }

    /// The document exists but the caller may not read it.
    pub fn hide(&self, n: u8) {
        self.add(n, Some(DocState::Draft));
        self.hidden.lock().unwrap().insert(rid(n));
    }

    /// Every write to `id` fails with `error` (checked before any waiting).
    pub fn inject(&self, id: RecordId, error: AppError) {
        self.injected.lock().unwrap().insert(id, error);
    }

    pub fn version_of(&self, id: RecordId) -> Option<u64> {
        self.docs.lock().unwrap().get(&id).map(|r| r.version)
    }

    pub fn state_of(&self, id: RecordId) -> Option<DocState> {
        self.docs.lock().unwrap().get(&id).and_then(|r| r.state)
    }

    pub fn exists(&self, id: RecordId) -> bool {
        self.docs.lock().unwrap().contains_key(&id)
    }

    fn not_found() -> AppError {
        AppError::new(
            ErrorClass::NotFound,
            "document.not_found",
            "the document does not exist or you may not see it",
        )
    }

    fn doc(&self, doctype: &str, id: RecordId, rec: Rec) -> Document {
        Document::loaded(
            Name::new(doctype).unwrap(),
            id,
            Version(rec.version),
            rec.state,
            BTreeMap::new(),
            BTreeMap::new(),
        )
    }

    fn apply(
        &self,
        inv: &Invocation,
        op: Op,
        id: RecordId,
        expected: Option<Version>,
    ) -> AppResult<Written> {
        let key = inv.command_id.as_ref().map(|c| c.as_str().to_owned());
        if let Some(key) = &key {
            self.command_ids.lock().unwrap().push(key.clone());
            if let Some((rid_, v, state)) = self.ledger.lock().unwrap().get(key) {
                return Ok(Written {
                    id: *rid_,
                    version: *v,
                    state: *state,
                });
            }
        }
        if let Some(error) = self.injected.lock().unwrap().get(&id) {
            return Err(error.clone());
        }
        if self.hidden.lock().unwrap().contains(&id) {
            return Err(Self::not_found());
        }
        let mut docs = self.docs.lock().unwrap();
        let Some(rec) = docs.get_mut(&id) else {
            return Err(Self::not_found());
        };
        if let Some(expected) = expected
            && expected.0 != rec.version
        {
            return Err(AppError::new(
                ErrorClass::StaleVersion,
                "document.stale_version",
                "the document changed since it was read",
            ));
        }
        let invalid = || {
            AppError::new(
                ErrorClass::InvalidState,
                "document.invalid_state",
                "the action is not allowed in the document's state",
            )
        };
        let written = match op {
            Op::Post => {
                if rec.state != Some(DocState::Draft) {
                    return Err(invalid());
                }
                rec.state = Some(DocState::Posted);
                rec.version += 1;
                Written {
                    id,
                    version: rec.version,
                    state: rec.state,
                }
            }
            Op::Cancel => {
                if rec.state != Some(DocState::Posted) {
                    return Err(invalid());
                }
                rec.state = Some(DocState::Cancelled);
                rec.version += 1;
                Written {
                    id,
                    version: rec.version,
                    state: rec.state,
                }
            }
            Op::Correct => {
                if rec.state != Some(DocState::Posted) {
                    return Err(invalid());
                }
                rec.state = Some(DocState::Cancelled);
                rec.version += 1;
                let draft = RecordId::new_v7();
                docs.insert(
                    draft,
                    Rec {
                        version: 1,
                        state: Some(DocState::Draft),
                    },
                );
                Written {
                    id: draft,
                    version: 1,
                    state: Some(DocState::Draft),
                }
            }
            Op::Delete => {
                if !matches!(rec.state, None | Some(DocState::Draft)) {
                    return Err(invalid());
                }
                docs.remove(&id);
                Written {
                    id,
                    version: 0,
                    state: None,
                }
            }
            Op::Update | Op::Method => {
                if rec.state == Some(DocState::Cancelled) {
                    return Err(invalid());
                }
                rec.version += 1;
                Written {
                    id,
                    version: rec.version,
                    state: rec.state,
                }
            }
        };
        drop(docs);
        self.applied.fetch_add(1, Ordering::SeqCst);
        if let Some(key) = key {
            self.ledger
                .lock()
                .unwrap()
                .insert(key, (written.id, written.version, written.state));
        }
        Ok(written)
    }

    async fn write(
        &self,
        inv: &Invocation,
        op: Op,
        id: RecordId,
        expected: Option<Version>,
    ) -> AppResult<Written> {
        assert!(!self.panic_on_write, "a write method was called");
        self.writes.fetch_add(1, Ordering::SeqCst);
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_in_flight.fetch_max(now, Ordering::SeqCst);
        let injected = self.injected.lock().unwrap().contains_key(&id);
        if !injected {
            let last = id.as_bytes()[15] as usize;
            let reverse = if self.reverse_completion {
                256 - last
            } else {
                0
            };
            for _ in 0..(self.yields + reverse) {
                tokio::task::yield_now().await;
            }
        }
        let result = self.apply(inv, op, id, expected);
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        result
    }

    fn out(&self, doctype: &str, w: Written) -> Outcome<Document> {
        let rec = Rec {
            version: w.version,
            state: w.state,
        };
        Outcome {
            value: self.doc(doctype, w.id, rec),
            messages: Vec::new(),
        }
    }
}

fn unsupported<T>() -> AppResult<T> {
    Err(AppError::new(
        ErrorClass::Internal,
        "test.unsupported",
        "not used by these tests",
    ))
}

impl DocumentService for Fake {
    fn meta<'a>(&'a self, _doctype: &'a str) -> BoxFuture<'a, AppResult<Arc<DocTypeDef>>> {
        Box::pin(async { unsupported() })
    }

    fn load<'a>(
        &'a self,
        _inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        Box::pin(async move {
            self.loads.fetch_add(1, Ordering::SeqCst);
            if self.hidden.lock().unwrap().contains(&id) {
                return Err(Self::not_found());
            }
            let rec = self.docs.lock().unwrap().get(&id).copied();
            match rec {
                Some(rec) => Ok(Outcome {
                    value: self.doc(doctype, id, rec),
                    messages: Vec::new(),
                }),
                None => Err(Self::not_found()),
            }
        })
    }

    fn create<'a>(
        &'a self,
        _inv: &'a Invocation,
        _doc: Document,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        Box::pin(async { unsupported() })
    }

    fn update<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        _patch: DocumentPatch,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        Box::pin(async move {
            let w = self.write(inv, Op::Update, id, Some(expected)).await?;
            Ok(self.out(doctype, w))
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
            let w = self.write(inv, Op::Post, id, Some(expected)).await?;
            Ok(self.out(doctype, w))
        })
    }

    fn cancel<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        Box::pin(async move {
            let w = self.write(inv, Op::Cancel, id, Some(expected)).await?;
            Ok(self.out(doctype, w))
        })
    }

    fn correct<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        Box::pin(async move {
            let w = self.write(inv, Op::Correct, id, Some(expected)).await?;
            Ok(self.out(doctype, w))
        })
    }

    fn delete<'a>(
        &'a self,
        inv: &'a Invocation,
        _doctype: &'a str,
        id: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<()>>> {
        Box::pin(async move {
            self.write(inv, Op::Delete, id, Some(expected)).await?;
            Ok(Outcome {
                value: (),
                messages: Vec::new(),
            })
        })
    }

    fn rename<'a>(
        &'a self,
        _inv: &'a Invocation,
        _doctype: &'a str,
        _id: RecordId,
        _new_name: &'a str,
        _expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        Box::pin(async { unsupported() })
    }

    fn call_method<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        _method: &'a str,
        _args: Json,
        expected: Option<Version>,
    ) -> BoxFuture<'a, AppResult<Outcome<(Document, Json)>>> {
        Box::pin(async move {
            let w = self.write(inv, Op::Method, id, expected).await?;
            let out = self.out(doctype, w);
            Ok(Outcome {
                value: (out.value, Json::Null),
                messages: Vec::new(),
            })
        })
    }

    fn field_changed<'a>(
        &'a self,
        _inv: &'a Invocation,
        _draft: Document,
        _field: &'a str,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        Box::pin(async { unsupported() })
    }
}
