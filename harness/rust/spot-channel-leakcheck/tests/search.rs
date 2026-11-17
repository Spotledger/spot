//! Channel `Search`: `spot-retrieval` through `Indexer::pump_once` (the index) and
//! `Retriever::search` (the results and snippets), the real public entry points, over the real
//! permission engine and field guard. The vector store's whole content and every response go to
//! the harness.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::time::SystemTime;

use common::*;
use spot_core::SiteId;
use spot_field_guard::leakcheck::{Corpus, Output, assert_no_leak, find_leaks};
use spot_permissions::PermissionEngine;
use spot_retrieval::testing::{
    FakeDocuments, FakeEmbedding, FakeFeed, FakePositions, FakeReadCheck, block_on, change,
};
use spot_retrieval::{
    EmbeddingProvider, IndexPolicy, Indexer, IndexerConfig, MemoryVectorStore, Query, Retriever,
    RetrieverConfig, VectorStore,
};
use spot_storage_contract::{ChangeOp, RecordId, Version};

/// The flat fields of the invoice that retrieval indexes (a child table is not a flat field).
const FIELDS: [&str; 7] = [
    "number",
    "notes",
    "card_number",
    "holder",
    "profile_json",
    "salary",
    "annual_cost",
];
const PROTECTED: [&str; 5] = [
    "card_number",
    "holder",
    "profile_json",
    "salary",
    "annual_cost",
];

struct Index {
    store: Arc<MemoryVectorStore>,
    retriever: Retriever,
    embeddings: Arc<FakeEmbedding>,
}

fn policy_text() -> String {
    let fields: Vec<String> = FIELDS.iter().map(|f| format!("\"{f}\"")).collect();
    let ok: Vec<String> = PROTECTED.iter().map(|f| format!("\"{f}\"")).collect();
    format!(
        "[doctypes.{DOCTYPE}]\nfields = [{}]\nrestricted_ok = [{}]\n",
        fields.join(", "),
        ok.join(", ")
    )
}

/// Builds the index over three planted invoices. `index_protected` is the tenant default;
/// `guarded` puts the field guard in front of the retriever.
fn build(core: &Core, corpus: &Corpus, index_protected: bool, guarded: bool) -> Index {
    let snap = snapshot(&core.meta, &*core.protection);
    let policy = IndexPolicy::load(&policy_text(), &snap).unwrap();
    let documents = Arc::new(FakeDocuments::new());
    let value = |label: &str| corpus.get(label).unwrap().value().to_owned();
    let mut changes = Vec::new();
    for i in 0..3u64 {
        let id = RecordId::new_v7();
        let number = format!("INV-{:04}", i + 1);
        documents.put_str(
            DOCTYPE,
            id,
            Version(1),
            &[
                ("number", number.as_str()),
                ("notes", PLAIN_NOTE),
                ("card_number", &value("card")),
                ("holder", &value("lookalike")),
                ("profile_json", &value("in_json")),
                ("salary", &value("salary")),
                ("annual_cost", &value("annual")),
            ],
        );
        changes.push(change(i + 1, DOCTYPE, id, ChangeOp::Insert, Version(1)).unwrap());
    }
    let embeddings = Arc::new(FakeEmbedding::new());
    let store = Arc::new(MemoryVectorStore::new());
    let mut indexer = Indexer::new(
        SiteId::new("leak.test").unwrap(),
        policy,
        documents,
        embeddings.clone(),
        store.clone(),
        Arc::new(FakePositions::new()),
        IndexerConfig::default(),
    )
    .with_protection(core.protection.clone(), index_protected);
    let feed = FakeFeed::new(changes);
    block_on(indexer.pump_once(&feed, 100, SystemTime::UNIX_EPOCH)).unwrap();
    let read_check = Arc::new(FakeReadCheck::new(PermissionEngine::new(Arc::new(snap))));
    let mut retriever = Retriever::new(
        embeddings.clone(),
        store.clone(),
        read_check,
        RetrieverConfig::default(),
    );
    if guarded {
        retriever = retriever.with_search_guard(Arc::new(core.guard.clone()));
    }
    Index {
        store,
        retriever,
        embeddings,
    }
}

/// Everything the store holds, as text (the index is a copy of data too).
fn store_dump(index: &Index) -> Output {
    let probe = index.embeddings.embed(&["x".to_owned()]).unwrap();
    let all = index.store.search(&probe[0], 100_000).unwrap();
    let mut text = String::new();
    for scored in all {
        text.push_str(&scored.chunk.text);
        text.push('\n');
    }
    Output::Text(text)
}

fn queries(corpus: &Corpus) -> Vec<String> {
    let mut out: Vec<String> = SECRETS
        .iter()
        .filter(|(_, _, path)| !path.contains('.'))
        .map(|(label, _, _)| corpus.get(label).unwrap().value().to_owned())
        .collect();
    out.push(format!("{PLAIN_NOTE} {PLAIN_NUMBER}"));
    out
}

#[test]
fn search_never_leaks() {
    let core = core();
    let corpus = corpus();
    // The tenant default is on, so the index DOES hold protected values: the retriever's guard
    // is what is under test. A principal sees a value only when `report` allows it.
    let index = build(&core, &corpus, true, true);
    for who in PRINCIPALS {
        let visible = visible_labels(who, true);
        let forbidden = corpus.without(&visible);
        let p = principal(who);
        let mut outputs: Vec<Output> = vec![];
        let mut seen_any = false;
        for text in queries(&corpus) {
            let hits = index.retriever.search(&p, &Query::new(&text, 20)).unwrap();
            seen_any |= !hits.is_empty();
            outputs.push(Output::Json(serde_json::to_string(&hits).unwrap()));
        }
        // The positive control: a principal who may report a field finds its value.
        for label in &visible {
            if SECRETS
                .iter()
                .any(|(l, _, p)| l == label && !p.contains('.'))
            {
                let text = corpus.get(label).unwrap().value().to_owned();
                let hits = index.retriever.search(&p, &Query::new(&text, 20)).unwrap();
                let out = [Output::Json(serde_json::to_string(&hits).unwrap())];
                let found: Vec<String> = find_leaks(&corpus.only(&[label]), &out)
                    .into_iter()
                    .map(|l| l.secret)
                    .collect();
                assert!(
                    found.iter().any(|s| s == label),
                    "{who}: `{label}` should be searchable but is not"
                );
            }
        }
        if plain_visible(who, true) {
            assert!(seen_any, "{who}: a plain hit is expected");
        }
        assert_no_leak(&forbidden, &outputs);
    }
}

#[test]
fn the_index_never_holds_a_protected_value_by_default() {
    // Tenant default off: the policy opts the fields in, the guard still keeps them out.
    let core = core();
    let corpus = corpus();
    let index = build(&core, &corpus, false, true);
    assert_no_leak(&corpus, &[store_dump(&index)]);
    // A plain field is indexed.
    let Output::Text(dump) = store_dump(&index) else {
        unreachable!()
    };
    assert!(dump.contains(PLAIN_NOTE), "the plain field must be indexed");
}

#[test]
fn an_unguarded_retriever_over_a_permissive_index_would_leak() {
    // The negative control that proves the test has teeth: with the guard absent a principal
    // holding `read` but not `report` is shown protected snippets, and the harness finds them.
    let core = core();
    let corpus = corpus();
    let index = build(&core, &corpus, true, false);
    let p = principal("read_no_report");
    let text = corpus.get("salary").unwrap().value().to_owned();
    let hits = index.retriever.search(&p, &Query::new(&text, 20)).unwrap();
    let out = [Output::Json(serde_json::to_string(&hits).unwrap())];
    let found = find_leaks(&corpus.only(&["salary"]), &out);
    assert!(
        !found.is_empty(),
        "without the guard the read-only caller sees the value (the gap this task closes)"
    );
}
