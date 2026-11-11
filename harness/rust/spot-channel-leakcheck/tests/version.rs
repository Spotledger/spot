//! Channel `Version`: `spot-doc-audit` (`DocAudit::history`, `GuardMasker::redact_diff`) over
//! the version-history diff. Two layers:
//!
//! - the stored audit trail, built by the real `build_event` and `LaneAppender`, read back by
//!   the real `history` for every principal (rule 9: the trail stores the masked form, so no
//!   principal gets a real value from it);
//! - the guard's `Channel::Version` redaction of a diff that still carries real values, through
//!   the producer's own masker, with the positive control that a reader of the group sees them.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::*;
use spot_activity::FieldChange;
use spot_app_api::document::ChildRow;
use spot_app_api::prelude::{Name, OffsetDateTime, RecordId, Value};
use spot_audit_chain::{CheckpointSigner, InMemoryStore, KeyRing, LocalSigner};
use spot_core::ManualClock;
use spot_doc_audit::event::{ActorRef, DocumentImage};
use spot_doc_audit::{
    AppenderStore, AuditSource, DiffValue, DocAudit, DocOperation, EventSizePolicy, FieldMasker,
    GuardMasker, HistoryEntry, InMemoryAppenderStore, LaneAppender, LaneConfig, build_event,
};
use spot_field_guard::leakcheck::{Output, assert_no_leak, find_leaks};

const SITE: &str = "leakcheck.spotledger.app";
const ROW_ONE: &str = "0190c0a0-0000-7000-8000-000000000011";
const ROW_TWO: &str = "0190c0a0-0000-7000-8000-000000000012";

fn n(s: &str) -> Name {
    Name::new(s).unwrap()
}

fn masker(core: &Core) -> Arc<GuardMasker> {
    Arc::new(GuardMasker::new(
        Arc::new(core.guard.clone()),
        core.protection.clone(),
    ))
}

fn text(v: &str) -> Value {
    Value::Text(v.to_owned())
}

/// An image of the invoice with the corpus planted in every protected field and the split
/// secret across two child rows.
fn image(id: &str, version: u64, corpus: &spot_field_guard::leakcheck::Corpus) -> DocumentImage {
    let value = |label: &str| corpus.get(label).unwrap().value().to_owned();
    let split = corpus.get("split_rows").unwrap().planted.clone();
    let mut img = DocumentImage::new(DOCTYPE, id, version);
    img.fields.insert(n("number"), text(PLAIN_NUMBER));
    img.fields.insert(n("notes"), text(PLAIN_NOTE));
    img.fields.insert(n("card_number"), text(&value("card")));
    img.fields.insert(n("holder"), text(&value("lookalike")));
    img.fields
        .insert(n("profile_json"), text(&value("in_json")));
    img.fields.insert(n("salary"), text(&value("salary")));
    img.fields.insert(n("annual_cost"), text(&value("annual")));
    let mut rows = Vec::new();
    for (i, memo) in split.iter().enumerate() {
        let mut fields = std::collections::BTreeMap::new();
        fields.insert(n("iban"), text(&value("iban")));
        fields.insert(n("memo"), text(memo));
        rows.push(ChildRow {
            id: RecordId::from_bytes(uuid_bytes(if i == 0 { ROW_ONE } else { ROW_TWO })).unwrap(),
            fields,
        });
    }
    img.tables.insert(n("items"), rows);
    img
}

fn uuid_bytes(s: &str) -> [u8; 16] {
    let hex: String = s.chars().filter(|c| *c != '-').collect();
    let mut out = [0u8; 16];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap();
    }
    out
}

fn diff_text(v: &Option<DiffValue>) -> String {
    match v {
        None => String::new(),
        Some(DiffValue::Text(t)) => t.clone(),
        Some(other) => format!("{other:?}"),
    }
}

fn render(entries: &[HistoryEntry]) -> Vec<Output> {
    let mut lines = Vec::new();
    let mut json = Vec::new();
    for e in entries {
        for c in &e.changes {
            lines.push(format!(
                "{} {} -> {}",
                c.path,
                diff_text(&c.before),
                diff_text(&c.after)
            ));
            json.push(serde_json::json!({
                "path": c.path,
                "before": diff_text(&c.before),
                "after": diff_text(&c.after),
            }));
        }
    }
    vec![
        Output::Text(lines.join("\n")),
        Output::Json(serde_json::Value::Array(json).to_string()),
    ]
}

fn signer() -> Arc<dyn CheckpointSigner> {
    Arc::new(LocalSigner::from_seed([9u8; 32]))
}

fn keys() -> KeyRing {
    let local = LocalSigner::from_seed([9u8; 32]);
    let mut keys = KeyRing::new();
    keys.add(&local.public_key()).unwrap();
    keys
}

fn actor() -> ActorRef {
    ActorRef {
        actor: "user:seed".to_owned(),
        on_behalf_of: None,
        component: None,
    }
}

#[tokio::test]
async fn version_history_never_leaks() {
    let core = core();
    let corpus = corpus();
    let store = Arc::new(InMemoryStore::new());
    let config = LaneConfig::new(1, 1);
    let state: Arc<dyn AppenderStore> = Arc::new(InMemoryAppenderStore::new());
    let id = "0190c0a0-0000-7000-8000-000000000001";
    let m = masker(&core);

    // A create carrying the whole corpus, then an update that moves every protected value
    // away: the second event's diff holds the corpus on its *before* side.
    let created = image(id, 1, &corpus);
    let mut updated = image(id, 2, &corpus);
    updated
        .fields
        .insert(n("card_number"), text("9999 8888 7777 6666"));
    updated.fields.insert(n("salary"), text("77777.11"));
    let appender = LaneAppender::new(
        Arc::clone(&store),
        SITE,
        0,
        config,
        signer(),
        Arc::clone(&state),
    );
    for (i, (op, after, before)) in [
        (DocOperation::Create, &created, None),
        (DocOperation::Update, &updated, Some(&created)),
    ]
    .into_iter()
    .enumerate()
    {
        let event = build_event(
            &*m,
            SITE,
            op,
            after,
            before,
            actor(),
            1_700_000_000_000_000 + i as i64,
            None,
            AuditSource::Api,
            &EventSizePolicy::default(),
        )
        .unwrap();
        appender
            .append_event(&format!("e{i}"), &event)
            .await
            .unwrap();
    }

    let svc = DocAudit::new(
        Arc::clone(&store),
        SITE,
        config,
        signer(),
        keys(),
        m.clone(),
        Arc::clone(&state),
        Arc::new(ManualClock::new(
            OffsetDateTime::from_unix_timestamp(1_700_000_100).unwrap(),
        )),
    );

    for name in PRINCIPALS {
        let entries = svc.history(&principal(name), DOCTYPE, id).await.unwrap();
        // The trail stores the masked form (rule 9), so nobody gets a real value from it,
        // whatever their right to the field.
        assert_no_leak(&corpus, &render(&entries));
        if name == "nobody" {
            assert!(
                entries.is_empty(),
                "a principal who cannot read gets no history"
            );
        } else {
            // The channel is not empty: the plain field is in the history of a reader.
            let shown = render(&entries);
            let notes = entries
                .iter()
                .flat_map(|e| e.changes.iter())
                .any(|c| c.path == "notes");
            assert!(notes, "{name}: the plain field must appear");
            assert!(!shown.is_empty());
        }
    }
}

/// A diff that still carries the real values (the form before the audit mask), through the
/// producer's own masker: the guard's `Channel::Version` redaction.
fn raw_changes(corpus: &spot_field_guard::leakcheck::Corpus) -> Vec<FieldChange> {
    let value = |label: &str| corpus.get(label).unwrap().value().to_owned();
    let split = corpus.get("split_rows").unwrap().planted.clone();
    let field = |path: &str, before: Option<&str>, after: &str| FieldChange {
        path: path.to_owned(),
        before: before.map(text),
        after: Some(text(after)),
    };
    let mut out = vec![
        field("notes", None, PLAIN_NOTE),
        field("card_number", Some(&value("card")), "9999 8888 7777 6666"),
        field("holder", None, &value("lookalike")),
        field("profile_json", None, &value("in_json")),
        field("salary", Some(&value("salary")), "77777.11"),
        field("annual_cost", None, &value("annual")),
    ];
    // A cell of an existing row, then a whole row added with its snapshot.
    out.push(field(
        &format!("items[id={ROW_ONE}].iban"),
        Some(&value("iban")),
        "GB00NEW",
    ));
    out.push(field(&format!("items[id={ROW_ONE}].memo"), None, &split[0]));
    let snapshot = serde_json::json!({
        "id": ROW_TWO,
        "fields": { "iban": value("iban"), "memo": split[1] },
    })
    .to_string();
    out.push(FieldChange {
        path: format!("items[id={ROW_TWO}]"),
        before: None,
        after: Some(Value::Json(snapshot)),
    });
    out
}

fn render_changes(changes: &[FieldChange]) -> Vec<Output> {
    let json: Vec<_> = changes
        .iter()
        .map(|c| {
            serde_json::json!({
                "path": c.path,
                "before": c.before.as_ref().map(|v| format!("{v:?}")),
                "after": c.after.as_ref().map(|v| format!("{v:?}")),
            })
        })
        .collect();
    vec![Output::Json(serde_json::Value::Array(json).to_string())]
}

#[test]
fn the_version_redaction_masks_both_sides_and_a_reader_still_sees_the_value() {
    let core = core();
    let corpus = corpus();
    let m = masker(&core);
    let changes = raw_changes(&corpus);
    for name in PRINCIPALS {
        let visible = visible_labels(name, false);
        let facts = doc_facts();
        let redacted = m
            .redact_diff(&principal(name), DOCTYPE, &facts, &changes)
            .unwrap();
        let out = render_changes(&redacted.changes);
        assert_no_leak(&corpus.without(&visible), &out);
        // The positive control: the group's reader gets the value through the same call.
        // (The snapshot of a whole added row is JSON inside a value, so the child-column
        // labels are found only when the row cell change carries them.)
        for label in &visible {
            let seen: Vec<String> = find_leaks(&corpus.only(&[label]), &out)
                .into_iter()
                .map(|l| l.secret)
                .collect();
            assert!(
                seen.iter().any(|s| s == label),
                "{name}: `{label}` should be visible in the version diff but is not"
            );
        }
        if name == "nobody" {
            assert!(redacted.hidden_changes, "hidden changes must be marked");
        }
    }
}
