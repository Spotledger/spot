//! WP-AI-07.T11: the document lifecycle port over an in-memory `DocumentService`. The fake
//! applies the rules the real guarded engine applies to the actor (a blocked user is denied, a
//! protected field is masked for everyone but the platform, a cancel needs a posted document) and
//! records who did what, so the tests prove: the actor's own identity reaches the engine and the
//! platform's never changes a document; the hard floor holds; a refusal is a fixed text; a
//! recompute writes only the changed derived field and a second one writes nothing; protected
//! fields are never recomputed; another site's document is invisible; and the world's documents
//! part built from the port runs a `document.*` remedy as a rehearsal and for real. The real
//! engine is exercised by the document engine's own tests and by the live-site smoke test of the
//! bundle; nothing here needs PostgreSQL.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use spot_app_api::context::{Actor, DocumentService, Invocation, Outcome};
use spot_app_api::prelude::{Name, RecordId, Value, Version};
use spot_app_api::{
    AppError, AppResult, BoxFuture, DocState, DocTypeDef, Document, DocumentPatch, ErrorClass,
    FieldDef, FieldKind, Tag, TextFormat,
};
use spot_cli::services::document_lifecycle::{
    BAD_STATE, DENIED, EngineSource, HARD_FLOOR, LiveLifecycle, NO_DOCUMENT, NOT_ATTACHED,
    NOT_SUPPORTED, PROTECTED_FIELD, REPOST_FINANCIAL, UNKNOWN_ACTOR, UNKNOWN_FIELD,
};
use spot_cli::services::world_access::documents::{DocumentLifecycle, from_parts};
use spot_core::{Clock, SiteId, SystemClock};
use spot_remedies::{DocOp, DocRef, DocState as RemedyState};
use tokio::runtime::Handle;

const ALLOWED: &str = "0190a7c2-5b1e-7c3a-9d4e-1f2a3b4c5d6e";
const BLOCKED: &str = "0190a7c2-5b1e-7c3a-9d4e-1f2a3b4c5d6f";
const INVOICE_SUBMITTED: &str = "0190a7c2-5b1e-7c3a-9d4e-1f2a3b4c5d01";
const NOTE_DRAFT: &str = "0190a7c2-5b1e-7c3a-9d4e-1f2a3b4c5d02";
const NOTE_POSTED: &str = "0190a7c2-5b1e-7c3a-9d4e-1f2a3b4c5d03";
const INVOICE_DRAFT: &str = "0190a7c2-5b1e-7c3a-9d4e-1f2a3b4c5d04";
const MISSING: &str = "0190a7c2-5b1e-7c3a-9d4e-1f2a3b4c5dff";

fn id(text: &str) -> RecordId {
    spot_bpm_documents::ids::parse_record_id(text).unwrap()
}

fn name(text: &str) -> Name {
    Name::new(text).unwrap()
}

fn text_field(field: &str) -> FieldDef {
    FieldDef::new(
        name(field),
        FieldKind::Text {
            max_chars: None,
            format: TextFormat::Plain,
        },
        false,
        true,
    )
}

fn int_field(field: &str) -> FieldDef {
    FieldDef::new(
        name(field),
        FieldKind::Int {
            format: spot_app_api::IntFormat::Plain,
        },
        false,
        true,
    )
}

/// `invoice` is financial (its `amount` is classified so); `note` is not and has a derived
/// `double` field and a protected `secret`.
fn defs() -> BTreeMap<String, Arc<DocTypeDef>> {
    let mut amount = text_field("amount");
    amount.security.classification = vec![Tag::new("financial").unwrap()];
    let invoice = DocTypeDef::new(
        name("invoice"),
        "Invoice".to_owned(),
        true,
        false,
        None,
        vec![amount],
    );
    let secret = text_field("secret");
    let note = DocTypeDef::new(
        name("note"),
        "Note".to_owned(),
        true,
        false,
        None,
        vec![int_field("qty"), int_field("double"), secret],
    );
    let mut map = BTreeMap::new();
    map.insert("invoice".to_owned(), Arc::new(invoice));
    map.insert("note".to_owned(), Arc::new(note));
    map
}

struct Stored {
    doctype: String,
    state: DocState,
    version: u64,
    fields: BTreeMap<Name, Value>,
}

/// A recorded engine call: `<call>|<actor kind>|<doc id>`.
type Calls = Arc<Mutex<Vec<String>>>;

struct FakeEngine {
    defs: BTreeMap<String, Arc<DocTypeDef>>,
    docs: Mutex<BTreeMap<RecordId, Stored>>,
    calls: Calls,
}

fn app_error(class: ErrorClass) -> AppError {
    AppError::new(
        class,
        "document.validation_failed",
        "the value 42 of secret",
    )
}

impl FakeEngine {
    fn new() -> Arc<Self> {
        Self::with_defs(defs())
    }

    fn with_defs(defs: BTreeMap<String, Arc<DocTypeDef>>) -> Arc<Self> {
        let mut docs = BTreeMap::new();
        let mut put = |text: &str, doctype: &str, state: DocState, qty: Option<i64>| {
            let mut fields = BTreeMap::new();
            fields.insert(name("amount"), Value::Text("100.00".to_owned()));
            if let Some(qty) = qty {
                fields.insert(name("qty"), Value::Int(qty));
            }
            fields.insert(name("secret"), Value::Text("hunter2".to_owned()));
            docs.insert(
                id(text),
                Stored {
                    doctype: doctype.to_owned(),
                    state,
                    version: 1,
                    fields,
                },
            );
        };
        put(INVOICE_SUBMITTED, "invoice", DocState::Posted, None);
        put(INVOICE_DRAFT, "invoice", DocState::Draft, None);
        put(NOTE_DRAFT, "note", DocState::Draft, Some(21));
        put(NOTE_POSTED, "note", DocState::Posted, Some(5));
        Arc::new(Self {
            defs,
            docs: Mutex::new(docs),
            calls: Arc::new(Mutex::new(Vec::new())),
        })
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn who(inv: &Invocation) -> &'static str {
        match &inv.actor {
            Actor::System { .. } => "platform",
            Actor::User { id: user, .. } if *user == id(BLOCKED) => "blocked",
            Actor::User { .. } => "user",
            Actor::Consumer { .. } => "consumer",
        }
    }

    fn record(&self, call: &str, inv: &Invocation, doc: RecordId) {
        self.calls
            .lock()
            .unwrap()
            .push(format!("{call}|{}|{doc}", Self::who(inv)));
    }

    fn document(&self, doc: RecordId, inv: &Invocation) -> AppResult<Document> {
        let docs = self.docs.lock().unwrap();
        let stored = docs
            .get(&doc)
            .ok_or_else(|| app_error(ErrorClass::NotFound))?;
        let mut fields = stored.fields.clone();
        if Self::who(inv) != "platform" {
            // The field guard: the protected field reads masked for everyone but the platform.
            fields.insert(name("secret"), Value::Text("***".to_owned()));
        }
        Ok(Document::loaded(
            name(&stored.doctype),
            doc,
            Version(stored.version),
            Some(stored.state),
            fields,
            BTreeMap::new(),
        ))
    }

    fn deny_blocked(inv: &Invocation) -> AppResult<()> {
        if Self::who(inv) == "blocked" {
            Err(app_error(ErrorClass::PermissionDenied))
        } else {
            Ok(())
        }
    }
}

fn done<T>(value: T) -> AppResult<Outcome<T>> {
    Ok(Outcome {
        value,
        messages: Vec::new(),
    })
}

impl DocumentService for FakeEngine {
    fn meta<'a>(&'a self, doctype: &'a str) -> BoxFuture<'a, AppResult<Arc<DocTypeDef>>> {
        Box::pin(async move {
            self.defs
                .get(doctype)
                .cloned()
                .ok_or_else(|| app_error(ErrorClass::UnknownDocType))
        })
    }

    fn load<'a>(
        &'a self,
        inv: &'a Invocation,
        _doctype: &'a str,
        doc: RecordId,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        Box::pin(async move {
            Self::deny_blocked(inv)?;
            done(self.document(doc, inv)?)
        })
    }

    fn create<'a>(
        &'a self,
        _inv: &'a Invocation,
        _doc: Document,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        Box::pin(async move { Err(app_error(ErrorClass::Internal)) })
    }

    fn update<'a>(
        &'a self,
        inv: &'a Invocation,
        _doctype: &'a str,
        doc: RecordId,
        patch: DocumentPatch,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        Box::pin(async move {
            Self::deny_blocked(inv)?;
            self.record(&format!("update:{}", patch.fields.len()), inv, doc);
            {
                let mut docs = self.docs.lock().unwrap();
                let stored = docs
                    .get_mut(&doc)
                    .ok_or_else(|| app_error(ErrorClass::NotFound))?;
                if Version(stored.version) != expected {
                    return Err(app_error(ErrorClass::StaleVersion));
                }
                for (field, value) in patch.fields {
                    stored.fields.insert(field, value);
                }
                stored.version += 1;
            }
            done(self.document(doc, inv)?)
        })
    }

    fn post<'a>(
        &'a self,
        _inv: &'a Invocation,
        _doctype: &'a str,
        _doc: RecordId,
        _expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        Box::pin(async move { Err(app_error(ErrorClass::Internal)) })
    }

    fn cancel<'a>(
        &'a self,
        inv: &'a Invocation,
        _doctype: &'a str,
        doc: RecordId,
        _expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        Box::pin(async move {
            Self::deny_blocked(inv)?;
            self.record("cancel", inv, doc);
            {
                let mut docs = self.docs.lock().unwrap();
                let stored = docs
                    .get_mut(&doc)
                    .ok_or_else(|| app_error(ErrorClass::NotFound))?;
                if stored.state != DocState::Posted {
                    return Err(app_error(ErrorClass::InvalidState));
                }
                stored.state = DocState::Cancelled;
                stored.version += 1;
            }
            done(self.document(doc, inv)?)
        })
    }

    fn correct<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        doc: RecordId,
        expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        Box::pin(async move {
            self.record("correct", inv, doc);
            self.cancel(inv, doctype, doc, expected).await
        })
    }

    fn delete<'a>(
        &'a self,
        _inv: &'a Invocation,
        _doctype: &'a str,
        _doc: RecordId,
        _expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<()>>> {
        Box::pin(async move { Err(app_error(ErrorClass::Internal)) })
    }

    fn rename<'a>(
        &'a self,
        _inv: &'a Invocation,
        _doctype: &'a str,
        _doc: RecordId,
        _new_name: &'a str,
        _expected: Version,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        Box::pin(async move { Err(app_error(ErrorClass::Internal)) })
    }

    fn call_method<'a>(
        &'a self,
        _inv: &'a Invocation,
        _doctype: &'a str,
        _doc: RecordId,
        _method: &'a str,
        _args: serde_json::Value,
        _expected: Option<Version>,
    ) -> BoxFuture<'a, AppResult<Outcome<(Document, serde_json::Value)>>> {
        Box::pin(async move { Err(app_error(ErrorClass::Internal)) })
    }

    fn field_changed<'a>(
        &'a self,
        inv: &'a Invocation,
        draft: Document,
        field: &'a str,
    ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
        Box::pin(async move {
            self.record(&format!("field_changed:{field}"), inv, draft.id());
            let mut adjusted = draft.clone();
            if field == "double" {
                let qty = draft.int("qty")?.unwrap_or(0);
                adjusted.set(name("double"), Value::Int(qty * 2));
            }
            done(adjusted)
        })
    }
}

struct Engines(Option<Arc<FakeEngine>>);

impl EngineSource for Engines {
    fn service(&self) -> Option<Arc<dyn DocumentService>> {
        self.0
            .as_ref()
            .map(|engine| Arc::clone(engine) as Arc<dyn DocumentService>)
    }
}

fn lifecycle(engine: Option<Arc<FakeEngine>>, site: &str) -> Arc<LiveLifecycle> {
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
    Arc::new(LiveLifecycle::new(
        Arc::new(Engines(engine)),
        SiteId::new(site).unwrap(),
        clock,
    ))
}

fn r(doctype: &str, doc: &str) -> String {
    format!("{doctype}/{doc}")
}

fn cancel() -> DocOp {
    DocOp::Cancel {
        reason: "wrong customer".to_owned(),
    }
}

fn recompute(field: &str) -> DocOp {
    DocOp::Recompute {
        field: field.to_owned(),
    }
}

#[tokio::test]
async fn a_draft_and_a_submitted_document_are_viewed_without_any_value() {
    let engine = FakeEngine::new();
    let lc = lifecycle(Some(Arc::clone(&engine)), "site-a");
    let draft = lc.view(&r("note", NOTE_DRAFT)).await.unwrap().unwrap();
    assert_eq!((draft.state, draft.financial), (RemedyState::Draft, false));
    let posted = lc
        .view(&r("invoice", INVOICE_SUBMITTED))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (posted.state, posted.financial),
        (RemedyState::Submitted, true)
    );
    assert!(lc.view(&r("note", MISSING)).await.unwrap().is_none());
    assert!(lc.view("not a reference").await.unwrap().is_none());
    // Only the platform looked, and only to read.
    assert!(engine.calls().is_empty());
}

#[tokio::test]
async fn cancel_runs_as_the_actor_and_a_user_who_may_not_is_refused() {
    let engine = FakeEngine::new();
    let lc = lifecycle(Some(Arc::clone(&engine)), "site-a");
    let doc = r("note", NOTE_POSTED);
    let after = lc
        .run(&format!("agent:{ALLOWED}"), &doc, &cancel())
        .await
        .unwrap();
    assert_eq!(after, RemedyState::Cancelled);
    assert_eq!(
        engine.calls(),
        vec![format!("cancel|user|{}", id(NOTE_POSTED))]
    );

    let other = r("note", NOTE_DRAFT);
    let refused = lc.run(&format!("agent:{BLOCKED}"), &other, &cancel()).await;
    assert_eq!(refused.unwrap_err(), DENIED);
    // A draft cannot be cancelled: the engine's own state rule, as a fixed text.
    let not_posted = lc.run(ALLOWED, &other, &cancel()).await;
    assert_eq!(not_posted.unwrap_err(), BAD_STATE);
}

#[tokio::test]
async fn a_submitted_financial_record_changes_only_through_cancel_and_amend() {
    let engine = FakeEngine::new();
    let lc = lifecycle(Some(Arc::clone(&engine)), "site-a");
    let doc = r("invoice", INVOICE_SUBMITTED);
    for op in [DocOp::Repost, recompute("amount")] {
        assert_eq!(lc.run(ALLOWED, &doc, &op).await.unwrap_err(), HARD_FLOOR);
    }
    assert!(
        engine.calls().is_empty(),
        "nothing changed, nothing was asked"
    );
    let amended = lc.run(ALLOWED, &doc, &DocOp::Amend).await.unwrap();
    assert_eq!(amended, RemedyState::Cancelled);
    assert_eq!(
        engine.calls(),
        vec![
            format!("correct|user|{}", id(INVOICE_SUBMITTED)),
            format!("cancel|user|{}", id(INVOICE_SUBMITTED)),
        ]
    );
}

#[tokio::test]
async fn an_unknown_actor_and_a_missing_document_are_refused_and_nothing_is_asked() {
    let engine = FakeEngine::new();
    let lc = lifecycle(Some(Arc::clone(&engine)), "site-a");
    let doc = r("note", NOTE_POSTED);
    for actor in ["", "agent:agent-t2", "someone"] {
        assert_eq!(
            lc.run(actor, &doc, &cancel()).await.unwrap_err(),
            UNKNOWN_ACTOR
        );
    }
    let missing = r("note", MISSING);
    assert_eq!(
        lc.run(ALLOWED, &missing, &cancel()).await.unwrap_err(),
        NO_DOCUMENT
    );
    assert_eq!(
        lc.run(ALLOWED, "garbage", &cancel()).await.unwrap_err(),
        NO_DOCUMENT
    );
    assert!(engine.calls().is_empty());
}

#[tokio::test]
async fn recompute_saves_only_the_changed_field_and_a_second_one_writes_nothing() {
    let engine = FakeEngine::new();
    let lc = lifecycle(Some(Arc::clone(&engine)), "site-a");
    let doc = r("note", NOTE_DRAFT);
    let state = lc.run(ALLOWED, &doc, &recompute("double")).await.unwrap();
    assert_eq!(state, RemedyState::Draft);
    let calls = engine.calls();
    assert_eq!(
        calls,
        vec![
            format!("field_changed:double|user|{}", id(NOTE_DRAFT)),
            format!("update:1|user|{}", id(NOTE_DRAFT)),
        ]
    );
    // The derived value is now stored; recomputing again changes nothing, so nothing is saved.
    lc.run(ALLOWED, &doc, &recompute("double")).await.unwrap();
    let after = engine.calls();
    assert_eq!(after.len(), 3, "only the field_changed call was added");
    assert!(after[2].starts_with("field_changed:double|user|"));
}

#[tokio::test]
async fn a_protected_or_unknown_field_is_never_recomputed() {
    // `secret` is in a protection group on this engine's `note` type.
    let mut with_group = defs();
    let mut note = (*with_group["note"]).clone();
    let mut secret = text_field("secret");
    secret.security.protection_group = Some(spot_app_api::GroupName::new("hidden").unwrap());
    note.fields = vec![int_field("qty"), int_field("double"), secret];
    with_group.insert("note".to_owned(), Arc::new(note));
    let engine = FakeEngine::with_defs(with_group);
    let lc = lifecycle(Some(Arc::clone(&engine)), "site-a");
    let doc = r("note", NOTE_DRAFT);
    assert_eq!(
        lc.run(ALLOWED, &doc, &recompute("secret"))
            .await
            .unwrap_err(),
        PROTECTED_FIELD
    );
    assert_eq!(
        lc.run(ALLOWED, &doc, &recompute("nope")).await.unwrap_err(),
        UNKNOWN_FIELD
    );
    assert!(engine.calls().is_empty());
}

#[tokio::test]
async fn repost_runs_the_save_steps_of_a_non_financial_document_and_refuses_a_financial_one() {
    let engine = FakeEngine::new();
    let lc = lifecycle(Some(Arc::clone(&engine)), "site-a");
    lc.run(ALLOWED, &r("note", NOTE_POSTED), &DocOp::Repost)
        .await
        .unwrap();
    assert_eq!(
        engine.calls(),
        vec![format!("update:0|user|{}", id(NOTE_POSTED))],
        "an update with no change: the save steps run, nothing is written"
    );
    let financial_draft = r("invoice", INVOICE_DRAFT);
    assert_eq!(
        lc.run(ALLOWED, &financial_draft, &DocOp::Repost)
            .await
            .unwrap_err(),
        REPOST_FINANCIAL
    );
    assert_eq!(engine.calls().len(), 1);
}

#[tokio::test]
async fn relink_and_journal_entry_are_not_supported() {
    let engine = FakeEngine::new();
    let lc = lifecycle(Some(Arc::clone(&engine)), "site-a");
    let doc = r("note", NOTE_DRAFT);
    let relink = DocOp::Relink {
        link: "customer".to_owned(),
    };
    let entry = DocOp::JournalEntry {
        entry: "JE-1".to_owned(),
    };
    for op in [relink, entry] {
        assert_eq!(lc.run(ALLOWED, &doc, &op).await.unwrap_err(), NOT_SUPPORTED);
    }
    assert!(engine.calls().is_empty());
}

#[tokio::test]
async fn no_attached_engine_is_a_fixed_refusal() {
    let lc = lifecycle(None, "site-a");
    assert_eq!(
        lc.view(&r("note", NOTE_DRAFT)).await.unwrap_err(),
        NOT_ATTACHED
    );
    assert_eq!(
        lc.run(ALLOWED, &r("note", NOTE_DRAFT), &cancel())
            .await
            .unwrap_err(),
        NOT_ATTACHED
    );
}

#[tokio::test]
async fn a_document_of_another_site_is_invisible() {
    // Site B's engine holds none of site A's documents: the engine is the site's own.
    let site_b = FakeEngine::new();
    site_b.docs.lock().unwrap().clear();
    let lc = lifecycle(Some(Arc::clone(&site_b)), "site-b");
    let doc = r("note", NOTE_POSTED);
    assert!(lc.view(&doc).await.unwrap().is_none());
    assert_eq!(
        lc.run(ALLOWED, &doc, &cancel()).await.unwrap_err(),
        NO_DOCUMENT
    );
    assert!(site_b.calls().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_worlds_documents_part_runs_a_document_remedy_as_a_rehearsal_and_for_real() {
    let engine = FakeEngine::new();
    let lc = lifecycle(Some(Arc::clone(&engine)), "site-a");
    let part = from_parts(
        Some(lc as Arc<dyn DocumentLifecycle>),
        Some(Handle::current()),
    )
    .unwrap();
    let doc = DocRef::new(&r("note", NOTE_POSTED));

    let dry = {
        let part = Arc::clone(&part);
        let doc = doc.clone();
        tokio::task::spawn_blocking(move || part.rehearsal().unwrap().run(&doc, &cancel()).unwrap())
            .await
            .unwrap()
    };
    assert_eq!(
        (dry.before, dry.after),
        (RemedyState::Submitted, RemedyState::Cancelled)
    );
    assert!(
        engine.calls().is_empty(),
        "a rehearsal never changes the document"
    );

    let applied = {
        let part = Arc::clone(&part);
        let doc = doc.clone();
        tokio::task::spawn_blocking(move || {
            part.live_as(&format!("agent:{ALLOWED}"))
                .run(&doc, &cancel())
                .unwrap()
        })
        .await
        .unwrap()
    };
    assert_eq!(
        (applied.before, applied.after),
        (RemedyState::Submitted, RemedyState::Cancelled)
    );
    assert_eq!(
        engine.calls(),
        vec![format!("cancel|user|{}", id(NOTE_POSTED))]
    );

    let refused = {
        let part = Arc::clone(&part);
        let doc = DocRef::new(&r("note", NOTE_DRAFT));
        tokio::task::spawn_blocking(move || {
            part.live_as(&format!("agent:{BLOCKED}"))
                .run(&doc, &cancel())
        })
        .await
        .unwrap()
    };
    assert_eq!(refused.unwrap_err().detail, DENIED);
}
