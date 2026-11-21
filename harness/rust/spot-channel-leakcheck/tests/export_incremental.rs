//! Channel `Export`, second producer: incremental extraction (`spot-bulk-data`
//! `IncrementalBatch`). Rows changed since a watermark leave "in their current state (guarded and
//! masked)"; the gaps this file covers are the ones a full export cannot show: a row whose only
//! change is a protected field, a field that became protected between two batches, a right
//! revoked between the watermark and the extract, and deleted rows (tombstones carry ids only).
//!
//! The batch type has no `changed_fields` list in this version of the crate: the destructuring
//! of `BatchChunk` below stops compiling the day one is added, which is the cue to cover it.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::*;
use spot_bulk_data::{
    AllowAllExport, BackendChangeReader, BackendPageReader, BatchChunk, BulkError, BulkLimits,
    ExportPlan, ExportSpec, FieldSet, Format, IncrementalBatch, IncrementalSpec, Watermark,
};
use spot_field_guard::leakcheck::{Corpus, Output, Secret, assert_no_leak, find_leaks};
use spot_permissions::model::Principal;
use spot_storage_contract::{
    ChangeFeed, DocumentStore, FeedPosition, Isolation, RecordId, RowLock, StorageBackend,
    Transaction, TxOptions, Value,
};

const RAISE: &str = "98765.43";
const NOTE_SECRET: &str = "TOP-SECRET-NOTE-4471";

fn corpus_with_extras() -> Corpus {
    corpus()
        .with(Secret::whole("salary_raise", RAISE))
        .with(Secret::whole("note_secret", NOTE_SECRET))
}

/// The labels the principal sees unmasked; the extras follow the field they are planted in.
fn visible(name: &str) -> Vec<&'static str> {
    let mut labels = visible_labels(name, true);
    if labels.contains(&"salary") {
        labels.push("salary_raise");
    }
    labels
}

async fn feed_end(store: &Store) -> FeedPosition {
    let changes = store
        .backend
        .read_changes(FeedPosition::START, 100_000)
        .await
        .unwrap();
    changes
        .last()
        .map(|c| c.position)
        .unwrap_or(FeedPosition::START)
}

async fn update_field(store: &Store, id: RecordId, field: &str, value: &str) {
    let mut t = store
        .backend
        .begin(
            store.catalog.clone(),
            TxOptions::new(name("leak_upd"), Isolation::ReadCommitted),
        )
        .await
        .unwrap();
    let doc = t
        .load(&name(DOCTYPE), id, RowLock::None)
        .await
        .unwrap()
        .unwrap();
    let mut data = doc.data;
    data.fields.insert(name(field), Value::Text(value.into()));
    t.update(&data, doc.version).await.unwrap();
    t.commit().await.unwrap();
}

async fn delete_doc(store: &Store, id: RecordId) {
    let mut t = store
        .backend
        .begin(
            store.catalog.clone(),
            TxOptions::new(name("leak_del"), Isolation::ReadCommitted),
        )
        .await
        .unwrap();
    let doc = t
        .load(&name(DOCTYPE), id, RowLock::None)
        .await
        .unwrap()
        .unwrap();
    t.delete(&name(DOCTYPE), id, doc.version).await.unwrap();
    t.commit().await.unwrap();
}

/// What one incremental extract delivered, or the refusal.
struct Extract {
    rows: Vec<Vec<Value>>,
    deletes: Vec<RecordId>,
    watermark: Option<Watermark>,
}

async fn extract(
    core: &Core,
    store: &Store,
    who: &Principal,
    since: Watermark,
) -> Result<Extract, BulkError> {
    let fields = FieldSet::new(DOCTYPE, "owner", &PARENT_COLUMNS);
    let spec = ExportSpec::new(DOCTYPE, Format::Csv).with_columns(&PARENT_COLUMNS);
    let plan = ExportPlan::build(
        &spec,
        who,
        &core.guard,
        &AllowAllExport,
        &fields,
        &*core.protection,
    )?;
    let reader = Arc::new(BackendPageReader::new(
        store.backend.clone(),
        store.catalog.clone(),
    ));
    let changes = BackendChangeReader::new(store.backend.clone());
    let incremental = IncrementalSpec {
        doctype: DOCTYPE.into(),
        columns: Vec::new(),
        since,
    };
    let limits = BulkLimits {
        rows_per_chunk: 2,
        ..BulkLimits::default()
    };
    let mut batch = IncrementalBatch::extract(
        Arc::new(plan),
        &incremental,
        reader,
        &changes,
        since.epoch,
        limits,
    )
    .await?;
    let mut rows = Vec::new();
    let mut deletes = Vec::new();
    while let Some(chunk) = batch.next_chunk().await? {
        // Exhaustive on purpose: a new field of the chunk must be covered here.
        let BatchChunk {
            upserts,
            deletes: gone,
        } = chunk;
        rows.extend(upserts);
        deletes.extend(gone);
    }
    Ok(Extract {
        rows,
        deletes,
        watermark: batch.new_watermark(),
    })
}

fn cell(value: &Value) -> String {
    match value {
        Value::Text(text) => text.clone(),
        Value::Decimal(d) => d.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Id(id) => id.to_string(),
        Value::Null => String::new(),
        other => format!("{other:?}"),
    }
}

fn quote(text: &str) -> String {
    format!("\"{}\"", text.replace('"', "\"\""))
}

/// Everything an extract put on the wire, in the encodings a consumer would see: CSV lines, a
/// JSON array and the Rust debug text of the whole delivery.
fn outputs(extract: &Extract) -> Vec<Output> {
    let csv: String = extract
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|v| quote(&cell(v)))
                .collect::<Vec<_>>()
                .join(",")
                + "\n"
        })
        .collect();
    let json = serde_json::Value::Array(
        extract
            .rows
            .iter()
            .map(|row| serde_json::Value::Array(row.iter().map(|v| cell(v).into()).collect()))
            .collect(),
    )
    .to_string();
    let tombstones: Vec<String> = extract.deletes.iter().map(ToString::to_string).collect();
    vec![
        Output::Csv(csv),
        Output::Json(json),
        Output::Text(format!(
            "{:?} deletes={tombstones:?} watermark={:?}",
            extract.rows, extract.watermark
        )),
    ]
}

fn refusal(error: &BulkError) -> Output {
    Output::Text(format!("{} {}", error.code(), error.message()))
}

async fn fresh(copies: usize) -> (Core, Store) {
    let core = core();
    let store = store(&core.meta, &corpus_with_extras(), copies).await;
    (core, store)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn incremental_export_never_leaks() {
    let corpus = corpus_with_extras();
    let mut cells = 0;
    for name_of in PRINCIPALS {
        let (core, store) = fresh(3).await;
        let since = Watermark {
            position: feed_end(&store).await,
            epoch: 0,
        };
        // A plain change, a change whose only content is a protected field, and a delete.
        update_field(&store, store.ids[0], "notes", "ship late").await;
        update_field(&store, store.ids[1], "salary", RAISE).await;
        delete_doc(&store, store.ids[2]).await;

        let who = principal(name_of);
        let seen = visible(name_of);
        let forbidden = corpus.without(&seen);
        let mut out: Vec<Output> = Vec::new();
        match extract(&core, &store, &who, since).await {
            Ok(done) => {
                assert!(
                    done.watermark.is_some(),
                    "{name_of}: the watermark comes back after the last chunk"
                );
                assert_eq!(done.deletes, vec![store.ids[2]], "{name_of}: one tombstone");
                assert_eq!(done.rows.len(), 2, "{name_of}: two upserts");
                let wire = outputs(&done);
                if seen.contains(&"salary_raise") {
                    // The positive control: the protected-only change is delivered to a
                    // principal who may see it, so a clean result is not an empty one.
                    let found: Vec<String> = find_leaks(&corpus.only(&["salary_raise"]), &wire)
                        .into_iter()
                        .map(|l| l.secret)
                        .collect();
                    assert!(
                        found.iter().any(|s| s == "salary_raise"),
                        "{name_of}: the changed salary should be delivered"
                    );
                }
                out.extend(wire);
            }
            Err(e) => {
                assert!(
                    matches!(e.code(), "permission.field_denied" | "bulk.export_denied"),
                    "{name_of}: unexpected refusal {}",
                    e.code()
                );
                assert!(
                    !seen.contains(&"salary_raise") || name_of == "no_cards",
                    "{name_of}: may see the salary but the extract was refused ({})",
                    e.code()
                );
                out.push(refusal(&e));
            }
        }
        assert_no_leak(&forbidden, &out);
        cells += 1;
    }
    assert_eq!(cells, PRINCIPALS.len());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_field_that_became_protected_is_withheld_from_the_next_batch() {
    // Batch one: `notes` is a plain field and the masked reader sees it. Between the batches the
    // field joins the `hr` group; batch two continues from batch one's watermark and must not
    // carry the value, although the row it belongs to changed again.
    let (open_core, store) = fresh(2).await;
    let reader = principal("masked_reader");
    let since = Watermark {
        position: feed_end(&store).await,
        epoch: 0,
    };
    update_field(&store, store.ids[0], "notes", NOTE_SECRET).await;
    let first = extract(&open_core, &store, &reader, since).await.unwrap();
    let first_wire = outputs(&first);
    assert!(
        find_leaks(&corpus_with_extras().only(&["note_secret"]), &first_wire)
            .iter()
            .any(|l| l.secret == "note_secret"),
        "the positive control: while the field is plain the value is delivered"
    );
    let watermark = first.watermark.expect("batch one is complete");

    let tightened = core_with(|defs| {
        for def in defs.iter_mut().filter(|d| d.name == DOCTYPE) {
            for field in def.fields.iter_mut().filter(|f| f.name == "notes") {
                field.protection_group = Some("hr".to_owned());
                field.masking = Some(spot_meta::MaskSpec::placeholder());
            }
        }
    });
    update_field(&store, store.ids[0], "number", "INV-0001-B").await;
    let mut out = Vec::new();
    match extract(&tightened, &store, &reader, watermark).await {
        Ok(second) => {
            assert_eq!(second.rows.len(), 1, "the changed row is still delivered");
            out.extend(outputs(&second));
        }
        Err(e) => out.push(refusal(&e)),
    }
    assert_no_leak(&corpus_with_extras().only(&["note_secret"]), &out);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_right_revoked_between_the_watermark_and_the_extract_is_honoured() {
    let (core, store) = fresh(2).await;
    let since = Watermark {
        position: feed_end(&store).await,
        epoch: 0,
    };
    update_field(&store, store.ids[0], "salary", RAISE).await;

    // Before the revocation the analyst receives the new salary.
    let before = extract(&core, &store, &principal("analyst"), since)
        .await
        .unwrap();
    assert!(
        find_leaks(
            &corpus_with_extras().only(&["salary_raise"]),
            &outputs(&before)
        )
        .iter()
        .any(|l| l.secret == "salary_raise"),
        "the positive control"
    );

    // The same user with no roles any more asks for the same watermark again.
    let revoked = Principal::with_roles("analyst", &[]);
    let mut out = Vec::new();
    match extract(&core, &store, &revoked, since).await {
        Ok(after) => out.extend(outputs(&after)),
        Err(e) => out.push(refusal(&e)),
    }
    assert_no_leak(&corpus_with_extras(), &out);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_deleted_row_is_an_id_and_nothing_else() {
    let (core, store) = fresh(1).await;
    let since = Watermark {
        position: feed_end(&store).await,
        epoch: 0,
    };
    delete_doc(&store, store.ids[0]).await;
    let done = extract(&core, &store, &principal("analyst"), since)
        .await
        .unwrap();
    assert!(done.rows.is_empty());
    assert_eq!(done.deletes, vec![store.ids[0]]);
    assert_no_leak(&corpus_with_extras(), &outputs(&done));
}

/// The negative control: an extract that copies the stored values without the guard is caught,
/// and the failure names the label and the encoding, never the value.
#[test]
fn an_unguarded_incremental_extract_fails_the_harness_without_printing_the_value() {
    let corpus = corpus_with_extras();
    let stub = serde_json::json!([[corpus.get("card").unwrap().value(), RAISE]]).to_string();
    let result = std::panic::catch_unwind(|| {
        assert_no_leak(&corpus_with_extras(), &[Output::Json(stub)]);
    });
    let payload = result.expect_err("the unguarded extract must fail the harness");
    let message = payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
        .unwrap_or_default();
    assert!(message.contains("card"), "{message}");
    assert!(message.contains("salary_raise"), "{message}");
    assert!(message.contains("json"), "{message}");
    for secret in &corpus.secrets {
        for part in &secret.planted {
            assert!(
                !message.contains(part.as_str()),
                "the failure printed a value"
            );
        }
    }
}
