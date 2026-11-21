//! Document naming (WP-DOC-05.T2): the engine names each new document by its DocType's naming
//! property through `spot-naming`, inside the operation's own transaction, on the fake backend.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;
mod flaky;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use common::{n, now};
use flaky::{Fault, FlakyBackend};
use spot_app_api::context::{Actor, DocumentService, Invocation};
use spot_app_api::prelude::{CommandId, OffsetDateTime, RecordId, SiteId, Value};
use spot_app_api::{AppError, AppResult, BoxFuture, Document, ErrorClass};
use spot_core::ManualClock;
use spot_document::engine::{Engine, FixedZone, TxParticipant, V7Ids, WriteIntent, WriteRecord};
use spot_meta::{Definition, EffectiveMeta, FieldSpec, FieldType, Registry};
use spot_naming::FixedIdSource;
use spot_seams::RegistryBuilder;
use spot_storage_contract::{ErrorKind, Transaction};
use spot_storage_fake::FakeBackend;
use time::{Month, UtcOffset};

fn text(name: &str, required: bool) -> FieldSpec {
    let mut f = FieldSpec::new(name, FieldType::Text);
    f.required = required;
    f.max_chars = Some(40);
    f
}

fn doctype(name: &str, naming: Option<&str>, posted: bool) -> Definition {
    let mut d = Definition::new(name);
    d.is_posted = posted;
    d.fields = vec![
        text("title", false),
        text("code", false),
        FieldSpec::new("amount", FieldType::Int),
    ];
    d.fields[2].required = true;
    d.title_field = Some("title".into());
    d.naming = naming.map(str::to_owned);
    d
}

fn meta() -> Arc<EffectiveMeta> {
    Registry::new(vec![
        doctype("series_doc", Some("INV-.YYYY-.#####"), true),
        doctype("field_doc", Some("field:code"), false),
        doctype("hash_doc", Some("hash"), false),
        doctype("prompt_doc", Some("prompt"), false),
        doctype("plain_doc", None, false),
        doctype("posted_prompt", Some("prompt"), true),
        doctype("posted_field", Some("field:code"), true),
    ])
    .unwrap()
    .effective()
}

fn inv_at(now: OffsetDateTime) -> Invocation {
    Invocation {
        site: SiteId::new("naming.test").unwrap(),
        actor: Actor::System {
            component: n("naming_tests"),
        },
        command_id: CommandId::new("cmd-1").ok(),
        language: None,
        now,
    }
}

fn inv() -> Invocation {
    inv_at(now())
}

fn seams() -> Arc<spot_seams::Registry> {
    Arc::new(RegistryBuilder::new().build().unwrap())
}

fn make<B: spot_storage_contract::StorageBackend>(backend: B) -> Engine<B> {
    Engine::new(
        backend,
        meta(),
        seams(),
        Arc::new(ManualClock::new(now())),
        Arc::new(V7Ids),
    )
    .unwrap()
}

async fn engine() -> Engine<FakeBackend> {
    let engine = make(FakeBackend::new());
    engine.prepare().await.unwrap();
    engine
}

fn doc(doctype: &str) -> Document {
    let mut d = Document::new(n(doctype), RecordId::new_v7());
    d.set(n("amount"), 5);
    d
}

fn title(d: &Document) -> String {
    match d.get("title") {
        Value::Text(t) => t.clone(),
        other => panic!("title is {other:?}"),
    }
}

async fn create<B: spot_storage_contract::StorageBackend>(
    engine: &Engine<B>,
    d: Document,
) -> AppResult<Document> {
    Ok(engine.create(&inv(), d).await?.value)
}

/// A posted document of `doctype`, created from `d`.
async fn posted<B: spot_storage_contract::StorageBackend>(
    engine: &Engine<B>,
    doctype: &str,
    d: Document,
) -> Document {
    let made = create(engine, d).await.unwrap();
    engine
        .post(&inv(), doctype, made.id(), made.version().unwrap())
        .await
        .unwrap()
        .value
}

/// A prompt-named draft called `name`.
fn named(name: &str) -> Document {
    let mut d = doc("posted_prompt");
    d.set(n("title"), name);
    d
}

async fn correct<B: spot_storage_contract::StorageBackend>(
    engine: &Engine<B>,
    doctype: &str,
    d: &Document,
) -> Document {
    engine
        .correct(&inv(), doctype, d.id(), d.version().unwrap())
        .await
        .unwrap()
        .value
}

#[tokio::test]
async fn series_numbers_count_up() {
    let engine = engine().await;
    let first = create(&engine, doc("series_doc")).await.unwrap();
    let second = create(&engine, doc("series_doc")).await.unwrap();
    assert_eq!(title(&first), "INV-2026-00001");
    assert_eq!(title(&second), "INV-2026-00002");
    let stored = engine
        .peek("series_doc", first.id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(title(&stored), "INV-2026-00001");
}

#[tokio::test]
async fn failed_create_returns_its_number() {
    let engine = engine().await;
    assert_eq!(
        title(&create(&engine, doc("series_doc")).await.unwrap()),
        "INV-2026-00001"
    );
    // The required `amount` is missing: the validation error comes after the number was taken.
    let mut bad = doc("series_doc");
    bad.set(n("amount"), Value::Null);
    let err = create(&engine, bad).await.unwrap_err();
    assert_eq!(err.class, ErrorClass::Validation);
    assert_eq!(
        title(&create(&engine, doc("series_doc")).await.unwrap()),
        "INV-2026-00002"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fifty_concurrent_creates_get_consecutive_numbers() {
    let engine = Arc::new(engine().await);
    let mut tasks = Vec::new();
    for _ in 0..50 {
        let engine = engine.clone();
        tasks.push(tokio::spawn(async move {
            title(&create(&engine, doc("series_doc")).await.unwrap())
        }));
    }
    let mut got = Vec::new();
    for t in tasks {
        got.push(t.await.unwrap());
    }
    got.sort();
    let want: Vec<String> = (1..=50).map(|i| format!("INV-2026-{i:05}")).collect();
    assert_eq!(got, want);
}

#[tokio::test]
async fn yearly_series_restarts_by_business_instant_and_zone() {
    // 31 December 2026, 23:30 UTC.
    let at = time::Date::from_calendar_date(2026, Month::December, 31)
        .unwrap()
        .with_hms(23, 30, 0)
        .unwrap()
        .assume_utc();

    let utc = engine().await;
    let d = utc
        .create(&inv_at(at), doc("series_doc"))
        .await
        .unwrap()
        .value;
    assert_eq!(title(&d), "INV-2026-00001");

    // +05:00 it is already 1 January 2027: a new year, a new series, from 1.
    let plus5 = make(FakeBackend::new())
        .with_business_zone(Arc::new(FixedZone(UtcOffset::from_hms(5, 0, 0).unwrap())));
    plus5.prepare().await.unwrap();
    let a = plus5
        .create(&inv_at(at), doc("series_doc"))
        .await
        .unwrap()
        .value;
    assert_eq!(title(&a), "INV-2027-00001");
    let b = plus5
        .create(&inv_at(at), doc("series_doc"))
        .await
        .unwrap()
        .value;
    assert_eq!(title(&b), "INV-2027-00002");
    // The wall clock is not consulted: an earlier instant still lands in 2026, from 1.
    let earlier = at - time::Duration::hours(6);
    let c = plus5
        .create(&inv_at(earlier), doc("series_doc"))
        .await
        .unwrap()
        .value;
    assert_eq!(title(&c), "INV-2026-00001");
}

#[tokio::test]
async fn field_strategy_takes_the_named_field() {
    let engine = engine().await;
    let mut d = doc("field_doc");
    d.set(n("code"), "ACME-7");
    let made = create(&engine, d).await.unwrap();
    assert_eq!(title(&made), "ACME-7");
}

#[tokio::test]
async fn field_strategy_without_the_field_is_refused_on_the_title() {
    let engine = engine().await;
    let err = create(&engine, doc("field_doc")).await.unwrap_err();
    assert_eq!(err.fields.len(), 1);
    assert_eq!(err.fields[0].field.to_string(), "title");
    assert_eq!(err.fields[0].code, "naming.missing_field");
}

#[tokio::test]
async fn hash_strategy_uses_the_id_source() {
    let id = RecordId::new_v7();
    let engine = make(FakeBackend::new()).with_naming_ids(Arc::new(FixedIdSource::new(id)));
    engine.prepare().await.unwrap();
    let made = create(&engine, doc("hash_doc")).await.unwrap();
    let name = title(&made);
    assert_eq!(name.len(), 26);
    // A fixed source gives the same name twice: the second is a duplicate.
    let err = create(&engine, doc("hash_doc")).await.unwrap_err();
    assert_eq!(err.class, ErrorClass::Duplicate);
    // Real ids differ every time.
    let real = self::engine().await;
    let a = title(&create(&real, doc("hash_doc")).await.unwrap());
    let b = title(&create(&real, doc("hash_doc")).await.unwrap());
    assert_ne!(a, b);
}

#[tokio::test]
async fn prompt_strategy_takes_the_callers_name_and_refuses_an_empty_one() {
    let engine = engine().await;
    let mut d = doc("prompt_doc");
    d.set(n("title"), "Opening balance");
    assert_eq!(title(&create(&engine, d).await.unwrap()), "Opening balance");
    for empty in [Value::Null, Value::Text("  ".into())] {
        let mut d = doc("prompt_doc");
        d.set(n("title"), empty);
        let err = create(&engine, d).await.unwrap_err();
        assert_eq!(err.fields[0].field.to_string(), "title");
        assert_eq!(err.fields[0].code, "naming.prompt_required");
    }
}

#[tokio::test]
async fn a_name_already_in_use_is_refused_on_the_title_field() {
    let engine = engine().await;
    let mut d = doc("prompt_doc");
    d.set(n("title"), "Same");
    create(&engine, d.clone()).await.unwrap();
    let mut again = doc("prompt_doc");
    again.set(n("title"), "Same");
    let err = create(&engine, again).await.unwrap_err();
    assert_eq!(err.class, ErrorClass::Duplicate);
    assert_eq!(err.code, "document.duplicate");
    assert_eq!(err.fields[0].code, "document.duplicate_name");
    assert_eq!(err.fields[0].field.to_string(), "title");
    // Nothing was written for the refused one.
    let mut other = doc("prompt_doc");
    other.set(n("title"), "Different");
    create(&engine, other).await.unwrap();
}

#[tokio::test]
async fn a_caller_cannot_choose_a_series_name() {
    let engine = engine().await;
    let mut d = doc("series_doc");
    d.set(n("title"), "INV-2026-00099");
    let err = create(&engine, d).await.unwrap_err();
    assert_eq!(err.fields[0].code, "document.name_not_assignable");
    // The refusal took no number.
    assert_eq!(
        title(&create(&engine, doc("series_doc")).await.unwrap()),
        "INV-2026-00001"
    );
}

#[tokio::test]
async fn a_doctype_without_naming_is_unchanged() {
    let engine = engine().await;
    let mut d = doc("plain_doc");
    d.set(n("title"), "anything");
    assert_eq!(title(&create(&engine, d).await.unwrap()), "anything");
    let untitled = create(&engine, doc("plain_doc")).await.unwrap();
    assert_eq!(untitled.get("title"), &Value::Null);
}

#[tokio::test]
async fn naming_without_a_title_field_is_refused_when_the_snapshot_is_built() {
    let mut d = doctype("orphan", Some("INV-.#####"), false);
    d.title_field = None;
    let bad = Registry::new(vec![d]).unwrap().effective();
    let err = Engine::new(
        FakeBackend::new(),
        bad.clone(),
        seams(),
        Arc::new(ManualClock::new(now())),
        Arc::new(V7Ids),
    )
    .err()
    .expect("refused");
    assert_eq!(err.code, "document.naming_title_field_missing");

    let engine = engine().await;
    let err = engine.set_meta(bad).await.unwrap_err();
    assert_eq!(err.code, "document.naming_title_field_missing");
    // The engine kept its old metadata.
    create(&engine, doc("series_doc")).await.unwrap();
}

/// Refuses every write of `series_doc` while `armed`.
struct Refuse(Arc<AtomicBool>);

impl<Tx: Transaction> TxParticipant<Tx> for Refuse {
    fn before_write<'a>(
        &'a self,
        _tx: &'a mut Tx,
        _write: &'a WriteIntent<'a>,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            if self.0.load(Ordering::SeqCst) {
                Err(AppError::new(
                    ErrorClass::InvalidState,
                    "document.invalid_state",
                    "refused",
                ))
            } else {
                Ok(())
            }
        })
    }

    fn after_write<'a>(
        &'a self,
        _tx: &'a mut Tx,
        _write: &'a WriteRecord<'a>,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn a_failed_document_write_consumes_no_number() {
    let engine = engine().await;
    let armed = Arc::new(AtomicBool::new(true));
    engine.set_participants(vec![Arc::new(Refuse(armed.clone()))]);
    create(&engine, doc("series_doc")).await.unwrap_err();
    create(&engine, doc("series_doc")).await.unwrap_err();
    armed.store(false, Ordering::SeqCst);
    assert_eq!(
        title(&create(&engine, doc("series_doc")).await.unwrap()),
        "INV-2026-00001"
    );
}

#[tokio::test]
async fn a_retried_operation_names_inside_the_retry() {
    let engine = make(FlakyBackend::new(FakeBackend::new()));
    engine.prepare().await.unwrap();
    engine
        .backend()
        .inject(Fault::rolled_back(ErrorKind::SerializationFailure));
    let first = create(&engine, doc("series_doc")).await.unwrap();
    // The rolled-back first attempt gave its number back: the retry took it again.
    assert_eq!(title(&first), "INV-2026-00001");
    assert_eq!(engine.retries().len(), 1);

    // A commit that fails for good also leaves no gap.
    engine
        .backend()
        .inject(Fault::rolled_back(ErrorKind::Internal));
    create(&engine, doc("series_doc")).await.unwrap_err();
    assert_eq!(
        title(&create(&engine, doc("series_doc")).await.unwrap()),
        "INV-2026-00002"
    );
}

#[tokio::test]
async fn the_amendment_of_a_corrected_document_is_named_like_a_new_one() {
    let engine = engine().await;
    let made = create(&engine, doc("series_doc")).await.unwrap();
    let posted = engine
        .post(&inv(), "series_doc", made.id(), made.version().unwrap())
        .await
        .unwrap()
        .value;
    let amended = engine
        .correct(&inv(), "series_doc", posted.id(), posted.version().unwrap())
        .await
        .unwrap()
        .value;
    assert_eq!(title(&posted), "INV-2026-00001");
    assert_eq!(title(&amended), "INV-2026-00002");
}

#[tokio::test]
async fn renaming_a_series_named_document_is_refused_and_changes_nothing() {
    let engine = engine().await;
    let posted = posted(&engine, "series_doc", doc("series_doc")).await;
    assert_eq!(title(&posted), "INV-2026-00001");
    let err = engine
        .rename(
            &inv(),
            "series_doc",
            posted.id(),
            "FREE-NAME",
            posted.version().unwrap(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.class, ErrorClass::Validation);
    assert_eq!(err.fields.len(), 1);
    assert_eq!(err.fields[0].field.to_string(), "title");
    assert_eq!(err.fields[0].code, "document.name_not_renameable");
    // Nothing was written: same title, same version.
    let stored = engine
        .peek("series_doc", posted.id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(title(&stored), "INV-2026-00001");
    assert_eq!(stored.version(), posted.version());
}

#[tokio::test]
async fn renaming_a_hash_named_document_is_refused_and_changes_nothing() {
    let engine = engine().await;
    let made = create(&engine, doc("hash_doc")).await.unwrap();
    let before = title(&made);
    let err = engine
        .rename(
            &inv(),
            "hash_doc",
            made.id(),
            "FREE-NAME",
            made.version().unwrap(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.fields[0].code, "document.name_not_renameable");
    assert_eq!(err.fields[0].field.to_string(), "title");
    let stored = engine.peek("hash_doc", made.id()).await.unwrap().unwrap();
    assert_eq!(title(&stored), before);
    assert_eq!(stored.version(), made.version());
}

#[tokio::test]
async fn renaming_a_prompt_named_document_still_works() {
    let engine = engine().await;
    let made = create(&engine, named("ACME")).await.unwrap();
    let renamed = engine
        .rename(
            &inv(),
            "posted_prompt",
            made.id(),
            "ACME-9",
            made.version().unwrap(),
        )
        .await
        .unwrap()
        .value;
    assert_eq!(title(&renamed), "ACME-9");
    let stored = engine
        .peek("posted_prompt", made.id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(title(&stored), "ACME-9");
}

#[tokio::test]
async fn correcting_a_prompt_named_document_suffixes_its_base() {
    let engine = engine().await;
    let first = posted(&engine, "posted_prompt", named("ACME")).await;
    let one = correct(&engine, "posted_prompt", &first).await;
    assert_eq!(title(&one), "ACME-1");
    assert_eq!(one.get("amended_from"), &Value::Id(first.id()));
    // Posting the amendment and correcting that gives the next suffix, never `ACME-1-1`.
    let posted_one = engine
        .post(&inv(), "posted_prompt", one.id(), one.version().unwrap())
        .await
        .unwrap()
        .value;
    let two = correct(&engine, "posted_prompt", &posted_one).await;
    assert_eq!(title(&two), "ACME-2");
}

#[tokio::test]
async fn a_taken_suffix_moves_the_amendment_on() {
    let engine = engine().await;
    // Another document already holds `ACME-1`.
    create(&engine, named("ACME-1")).await.unwrap();
    let first = posted(&engine, "posted_prompt", named("ACME")).await;
    let amended = correct(&engine, "posted_prompt", &first).await;
    assert_eq!(title(&amended), "ACME-2");
}

#[tokio::test]
async fn a_field_named_amendment_keeps_the_suffix() {
    let engine = engine().await;
    let mut d = doc("posted_field");
    d.set(n("code"), "ACME-7");
    let first = posted(&engine, "posted_field", d).await;
    assert_eq!(title(&first), "ACME-7");
    // The copy's `code` still says ACME-7: the title written for it is the amendment's name.
    let amended = correct(&engine, "posted_field", &first).await;
    assert_eq!(title(&amended), "ACME-1");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_corrections_of_one_base_take_different_suffixes() {
    let engine = Arc::new(engine().await);
    // Two documents whose amendments share the base `ACME`.
    let a = posted(&engine, "posted_prompt", named("ACME")).await;
    let b = posted(&engine, "posted_prompt", named("ACME-05")).await;
    let mut tasks = Vec::new();
    for d in [a, b] {
        let engine = engine.clone();
        tasks.push(tokio::spawn(async move {
            let inv = inv();
            let version = d.version().unwrap();
            let amended = engine
                .correct(&inv, "posted_prompt", d.id(), version)
                .await
                .unwrap()
                .value;
            title(&amended)
        }));
    }
    let mut got = Vec::new();
    for t in tasks {
        got.push(t.await.unwrap());
    }
    got.sort();
    assert_eq!(got, ["ACME-1", "ACME-2"]);
}

#[tokio::test]
async fn a_failed_correct_leaves_no_suffix_behind() {
    let engine = engine().await;
    let first = posted(&engine, "posted_prompt", named("ACME")).await;
    // Every write is refused while armed: the correct rolls back whole.
    engine.set_participants(vec![Arc::new(Refuse(Arc::new(AtomicBool::new(true))))]);
    engine
        .correct(
            &inv(),
            "posted_prompt",
            first.id(),
            first.version().unwrap(),
        )
        .await
        .unwrap_err();
    let still = engine
        .peek("posted_prompt", first.id())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(still.version(), first.version());
    // The name was chosen inside the transaction, never allocated: the next correct takes the
    // first suffix again.
    engine.set_participants(Vec::new());
    let amended = correct(&engine, "posted_prompt", &first).await;
    assert_eq!(title(&amended), "ACME-1");
}
