//! Channel `Journal`: `spot-journal-relay` through `Relay::append_batch`, `Relay::seal_now`,
//! `Relay::open_views` and `JournalViewer::view_line`, the public entry points that describe a
//! journal entry to a reader that is not the system. The journal holds whole record images with
//! every protected value planted; the channel carries field **names** only, so the harness must
//! find none of the corpus in anything that leaves, for any principal, and a protected field's
//! name must appear only for a principal who may know the field exists.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeSet;

use common::*;
use ed25519_dalek::SigningKey;
use serde_json::{Value as Json, json};
use spot_events_contract::envelope::Position;
use spot_field_guard::leakcheck::{Output, assert_no_leak, find_leaks};
use spot_field_guard::{Channel, ProtectionSource};
use spot_journal_manifest::{Manifest, SignedManifest, VerifyingKeys, sign};
use spot_journal_relay::{
    Appended, Batch, Durability, JournalView, JournalViewer, ManifestSigner, Relay, RelayConfig,
    RelayError, VIEW_CHANNEL,
};
use spot_journal_segment::{SealLimits, SegmentReader};
use spot_journal_store::{JournalStore, MemoryStore};

const KEY_ID: &str = "leakcheck-journal-key";

fn signing_key() -> SigningKey {
    SigningKey::from_bytes(&[9u8; 32])
}

struct Signer;

impl ManifestSigner for Signer {
    fn sign(&self, manifest: &Manifest) -> SignedManifest {
        sign(manifest, &signing_key(), KEY_ID)
    }
    fn key_id(&self) -> &str {
        KEY_ID
    }
}

fn keys() -> VerifyingKeys {
    let mut keys = VerifyingKeys::new();
    keys.add_key(KEY_ID, signing_key().verifying_key(), 1, u64::MAX)
        .unwrap();
    keys
}

/// The protected paths of the world, as a journal view names them.
const PROTECTED: [&str; 7] = [
    "card_number",
    "holder",
    "profile_json",
    "salary",
    "annual_cost",
    "items.iban",
    "items.memo",
];

/// The protected names a principal may know exist: a field whose every group the principal holds
/// at least `read_masked` on (the `deny` and the missing grants hide the rest).
fn names_known(principal: &str) -> BTreeSet<&'static str> {
    let names: &[&str] = match principal {
        "masked_reader" | "analyst" | "admin" => &PROTECTED,
        "no_cards" => &["salary", "annual_cost", "items.iban", "items.memo"],
        "read_no_report" => &["card_number", "holder", "profile_json", "salary"],
        _ => &[],
    };
    names.iter().copied().collect()
}

/// One `doc_state` envelope line whose image holds the whole corpus.
fn state_line(seq: u64, corpus: &spot_field_guard::leakcheck::Corpus) -> Vec<u8> {
    let value = |label: &str| corpus.get(label).unwrap().value().to_owned();
    let rows: Vec<Json> = corpus
        .get("split_rows")
        .unwrap()
        .planted
        .iter()
        .map(|memo| json!({ "qty": "2.000", "iban": value("iban"), "memo": memo }))
        .collect();
    let envelope = json!({
        "event_id": format!("0192f1c0-0000-7000-8000-0000000001{seq:02}"),
        "topic": "doc_state",
        "schema_version": 1,
        "source_id": "leak.example",
        "position": { "generation": 1, "seq": seq },
        "transaction": "0192f1c0-0000-7000-8000-0000000000f1",
        "ordinal": 0,
        "subject": { "entity": DOCTYPE, "id": "0192f1c0-0000-7000-8000-000000000001" },
        "subject_version": seq,
        "meta": {
            "occurred_at": "2026-11-02T10:00:00Z",
            "actor": { "kind": "system", "component": "capture" },
            "correlation_id": "0192f1c0-0000-7000-8000-0000000000e1"
        },
        "data": {
            "op": "update",
            "entity_schema": 1,
            "image": {
                "id": "0192f1c0-0000-7000-8000-000000000001",
                "number": PLAIN_NUMBER,
                "notes": PLAIN_NOTE,
                "card_number": value("card"),
                "holder": value("lookalike"),
                "profile_json": value("in_json"),
                "salary": value("salary"),
                "annual_cost": value("annual"),
                "items": rows,
            }
        }
    });
    envelope.to_string().into_bytes()
}

fn batch(id: u64, first: u64, last: u64, corpus: &spot_field_guard::leakcheck::Corpus) -> Batch {
    Batch {
        batch_id: id,
        entries: (first..=last)
            .map(|seq| (Position { generation: 1, seq }, state_line(seq, corpus)))
            .collect(),
    }
}

/// A relay over a memory store holding two sealed entries (read back from the segment) and two
/// still in the open segment; every raw line the journal holds is returned too.
struct Journal {
    relay: Relay<MemoryStore>,
    sealed_lines: Vec<String>,
}

async fn journal(corpus: &spot_field_guard::leakcheck::Corpus) -> Journal {
    let tail = spot_secrets::EnvKey::from_base64("AQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQE=")
        .expect("a 32 byte test key");
    let mut config = RelayConfig::new("leak-tenant", "leak.example", Durability::RemoteSync)
        .with_tail_keys(spot_journal_relay::TailKeys::new(std::sync::Arc::new(tail)));
    config.limits = SealLimits {
        max_bytes: u64::MAX,
        max_age_millis: u64::MAX,
    };
    let mut relay = Relay::open(MemoryStore::new(), Box::new(Signer), keys(), config, 1_000)
        .await
        .unwrap();
    let appended = relay
        .append_batch(batch(1, 1, 2, corpus), 2_000)
        .await
        .unwrap();
    assert!(matches!(
        appended,
        Appended::ChunkDurable { up_to_seq: 2, .. }
    ));
    assert!(relay.seal_now(3_000).await.unwrap());
    relay
        .append_batch(batch(2, 3, 4, corpus), 4_000)
        .await
        .unwrap();
    assert_eq!(relay.open_entries(), 2);

    // The sealed entries, as the system reads them back from the store.
    let mut sealed_lines = Vec::new();
    let listing = relay
        .store()
        .list("leak-tenant/segments/", None, 100)
        .await
        .unwrap();
    assert_eq!(listing.len(), 1);
    for object in listing {
        let bytes = relay.store().get(&object.key).await.unwrap().unwrap().bytes;
        let reader = SegmentReader::open(bytes).unwrap();
        for entry in reader.entries() {
            sealed_lines.push(entry.unwrap().line);
        }
    }
    assert_eq!(sealed_lines.len(), 2);
    Journal {
        relay,
        sealed_lines,
    }
}

fn json_of(view: &JournalView) -> Output {
    Output::Json(view.to_json().to_string())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn journal_never_leaks() {
    let core = core();
    let corpus = corpus();
    let journal = journal(&corpus).await;
    let viewer = JournalViewer::new(&core.guard, &*core.protection);
    assert_eq!(VIEW_CHANNEL, Channel::Journal);

    for name in PRINCIPALS {
        let who = principal(name);
        let mut views: Vec<JournalView> = Vec::new();
        let mut outputs: Vec<Output> = Vec::new();

        // The sealed entries, through the single constructor.
        for line in &journal.sealed_lines {
            views.push(viewer.view_line(&who, line.as_bytes()).unwrap());
        }
        // The entries still in the open segment, through the relay.
        for view in journal.relay.open_views(&viewer, &who) {
            views.push(view.unwrap());
        }
        assert_eq!(views.len(), 4);

        for view in &views {
            outputs.push(json_of(view));
            outputs.push(Output::Text(format!("{view:?}")));
            outputs.push(Output::Text(view.changed().join("\n")));

            let seen: BTreeSet<&str> = PROTECTED
                .into_iter()
                .filter(|path| view.changed().iter().any(|n| n == path))
                .collect();
            assert_eq!(
                seen,
                names_known(name),
                "{name}: the protected names a view tells"
            );
            // A plain field follows the DocType right; never a value.
            if matches!(name, "analyst" | "admin" | "masked_reader") {
                assert!(view.changed().iter().any(|n| n == "number"), "{name}");
                assert!(view.changed().iter().any(|n| n == "items.qty"), "{name}");
            }
            assert!(!view.names_withheld(), "{name}");
            assert_eq!(view.kind(), "update");
            assert_eq!(view.entity(), DOCTYPE);
        }

        // The relay's own surfaces: the manifest, and the errors of a refused batch.
        outputs.push(Output::Text(format!("{:?}", journal.relay.manifest())));
        outputs.push(Output::Text(format!(
            "{} {} {}",
            journal.relay.tenant(),
            journal.relay.source(),
            journal.relay.generation()
        )));

        // The value never leaves, for any principal at all, the analyst and the admin included.
        assert_no_leak(&corpus, &outputs);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_batch_and_a_bad_line_never_quote_an_entry() {
    let core = core();
    let corpus = corpus();
    let mut journal = journal(&corpus).await;
    let mut outputs: Vec<Output> = Vec::new();

    // Out of order, a gap, and a line that is not UTF-8 beside a planted value.
    let mut unreadable = corpus.get("salary").unwrap().value().as_bytes().to_vec();
    unreadable.push(0xff);
    let attempts = [
        batch(3, 1, 1, &corpus),
        batch(4, 9, 9, &corpus),
        Batch {
            batch_id: 5,
            entries: vec![(
                Position {
                    generation: 1,
                    seq: 5,
                },
                unreadable.clone(),
            )],
        },
    ];
    for attempt in attempts {
        let error = journal
            .relay
            .append_batch(attempt, 5_000)
            .await
            .unwrap_err();
        outputs.push(Output::Text(format!("{} {error} {error:?}", error.code())));
        // A failed call asks for a resume only after a store failure; this one wrote nothing.
        assert_ne!(error, RelayError::ResumeRequired);
    }
    let viewer = JournalViewer::new(&core.guard, &*core.protection);
    let refused = viewer
        .view_line(&principal("admin"), &unreadable)
        .unwrap_err();
    outputs.push(Output::Text(format!("{refused} {refused:?}")));
    let torn = format!(
        "{{\"data\": \"{}\", broken",
        corpus.get("card").unwrap().value()
    );
    let refused = viewer
        .view_line(&principal("admin"), torn.as_bytes())
        .unwrap_err();
    outputs.push(Output::Text(format!("{refused} {refused:?}")));

    assert_no_leak(&corpus, &outputs);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_entry_of_an_unknown_doctype_is_named_but_none_of_its_fields_are() {
    let core = core();
    let corpus = corpus();
    let line = String::from_utf8(state_line(1, &corpus))
        .unwrap()
        .replace(DOCTYPE, "ghost_doctype");
    let viewer = JournalViewer::new(&core.guard, &*core.protection);
    assert!(core.protection.protection("ghost_doctype").is_none());
    let view = viewer
        .view_line(&principal("admin"), line.as_bytes())
        .unwrap();
    assert_eq!(view.entity(), "ghost_doctype");
    assert!(view.changed().is_empty());
    assert!(view.names_withheld());
    assert_no_leak(&corpus, &[json_of(&view)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_raw_journal_line_would_leak_so_the_harness_is_not_vacuous() {
    let corpus = corpus();
    let journal = journal(&corpus).await;
    // The raw line is the system's own record: it holds every value. Handing it to a reader is
    // what the projection exists to prevent, and the harness must see it.
    let raw: Vec<Output> = journal
        .sealed_lines
        .iter()
        .map(|line| Output::Json(line.clone()))
        .collect();
    let leaks = find_leaks(&corpus, &raw);
    assert!(
        !leaks.is_empty(),
        "the raw journal line holds the planted values"
    );
}

#[test]
fn the_pending_entry_for_the_journal_is_gone() {
    use spot_channel_leakcheck::{Coverage, coverage_of};
    assert_eq!(
        coverage_of(Channel::Journal),
        Some(Coverage::Test {
            test: "journal_never_leaks",
            producer: "spot-journal-relay",
        })
    );
}
