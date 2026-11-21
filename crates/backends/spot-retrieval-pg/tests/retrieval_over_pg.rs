//! `spot-retrieval`'s indexer and retriever running over the PostgreSQL store: an unreadable
//! document is dropped before a hit leaves the crate (and the moment a permission is revoked),
//! a field the caller may not read never reaches a snippet, and the index follows the change
//! feed: updates re-embed, a shrunk document loses its stale chunks, a delete removes the
//! vectors, and a replay from the start leaves no duplicates.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod common;

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use common::{DIM, fixture};
use spot_core::SiteId;
use spot_meta::{Definition, FieldSpec, FieldType, Registry};
use spot_permissions::{Decision, Model, Principal, Snapshot};
use spot_retrieval::testing::{
    FakeDocuments, FakeEmbedding, FakeFeed, FakePositions, block_on, change,
};
use spot_retrieval::{
    Chunk, DocumentSource, EmbeddingProvider, FieldView, IndexPolicy, Indexer, IndexerConfig,
    Query, ReadCheck, Retriever, RetrieverConfig, VectorStore,
};
use spot_retrieval_pg::PgVectorStore;
use spot_retrieval_pg::conformance::rid;
use spot_storage_contract::{ChangeOp, RecordId, Version};

/// A read check with explicit lists: what this test proves is the retriever's behaviour over the
/// store, not the permission engine (which `spot-retrieval`'s own acceptance test covers).
struct Lists {
    readable: Mutex<BTreeSet<RecordId>>,
    fields: Mutex<BTreeSet<String>>,
}

impl Lists {
    fn new(readable: &[RecordId], fields: &[&str]) -> Self {
        Self {
            readable: Mutex::new(readable.iter().copied().collect()),
            fields: Mutex::new(fields.iter().map(|f| (*f).to_owned()).collect()),
        }
    }

    fn revoke(&self, id: RecordId) {
        self.readable.lock().unwrap().remove(&id);
    }
}

impl ReadCheck for Lists {
    fn can_read(&self, _principal: &Principal, _doctype: &str, id: RecordId) -> Decision {
        Decision {
            allowed: self.readable.lock().unwrap().contains(&id),
            reasons: Vec::new(),
        }
    }

    fn readable_fields(&self, _principal: &Principal, _doctype: &str) -> FieldView {
        FieldView::Only(self.fields.lock().unwrap().clone())
    }
}

fn chunk(n: u8, no: u32, field: &str, text: &str, vector: Vec<f32>) -> Chunk {
    Chunk {
        doctype: "note".to_owned(),
        id: rid(n),
        version: Version(1),
        chunk_no: no,
        fields: vec![field.to_owned()],
        text: text.to_owned(),
        vector,
    }
}

fn unit(axis: usize) -> Vec<f32> {
    let mut v = vec![0.1_f32; DIM];
    v[axis] = 5.0;
    v
}

fn retriever(
    store: &Arc<PgVectorStore>,
    check: &Arc<Lists>,
    embeddings: &Arc<FakeEmbedding>,
) -> Retriever {
    Retriever::new(
        Arc::clone(embeddings) as Arc<dyn spot_retrieval::EmbeddingProvider>,
        Arc::clone(store) as Arc<dyn VectorStore>,
        Arc::clone(check) as Arc<dyn ReadCheck>,
        RetrieverConfig::default(),
    )
}

/// The fake provider's vector for `text`.
fn embed(embeddings: &FakeEmbedding, text: &str) -> Vec<f32> {
    embeddings.embed(&[text.to_owned()]).unwrap().remove(0)
}

fn alice() -> Principal {
    Principal::user("alice", Vec::new())
}

#[test]
fn a_hit_for_a_document_the_caller_cannot_read_never_leaves_the_crate() {
    let Some(fixture) = fixture() else {
        return;
    };
    let store = Arc::new(fixture.store("acme"));
    let embeddings = Arc::new(FakeEmbedding::with_dim(DIM));
    // Document 1 is readable, document 2 is not; the index holds both and decides nothing.
    let query_vector = embed(&embeddings, "budget forecast");
    store
        .upsert(&[
            chunk(
                1,
                0,
                "body",
                "the budget forecast for the quarter",
                query_vector.clone(),
            ),
            chunk(
                2,
                0,
                "body",
                "the budget forecast of the board only",
                query_vector,
            ),
            chunk(3, 0, "body", "unrelated", unit(3)),
        ])
        .unwrap();
    let check = Arc::new(Lists::new(&[rid(1), rid(3)], &["title", "body"]));
    let retriever = retriever(&store, &check, &embeddings);

    let hits = retriever
        .search(&alice(), &Query::new("budget forecast", 5))
        .unwrap();
    let ids: Vec<RecordId> = hits.hits.iter().map(|hit| hit.id).collect();
    assert!(ids.contains(&rid(1)), "the readable document is found");
    assert!(!ids.contains(&rid(2)), "the unreadable document is dropped");
    assert!(
        hits.hits
            .iter()
            .all(|hit| !hit.snippet.contains("board only")),
        "its text never reaches a snippet"
    );
    // The index still holds it: the drop is the retriever's re-check, not the store's.
    assert_eq!(store.chunks_of("note", rid(2)).unwrap().len(), 1);
}

#[test]
fn a_revoked_permission_hides_the_hit_on_the_very_next_query() {
    let Some(fixture) = fixture() else {
        return;
    };
    let store = Arc::new(fixture.store("acme"));
    let embeddings = Arc::new(FakeEmbedding::with_dim(DIM));
    let v = embed(&embeddings, "quarterly plan");
    store
        .upsert(&[chunk(1, 0, "body", "the quarterly plan", v)])
        .unwrap();
    let check = Arc::new(Lists::new(&[rid(1)], &["body"]));
    let retriever = retriever(&store, &check, &embeddings);
    let query = Query::new("quarterly plan", 5);
    assert_eq!(retriever.search(&alice(), &query).unwrap().len(), 1);
    check.revoke(rid(1));
    assert!(
        retriever.search(&alice(), &query).unwrap().is_empty(),
        "no re-index is needed for a revocation to apply"
    );
}

#[test]
fn a_field_the_caller_may_not_read_never_appears_in_a_snippet() {
    let Some(fixture) = fixture() else {
        return;
    };
    let store = Arc::new(fixture.store("acme"));
    let embeddings = Arc::new(FakeEmbedding::with_dim(DIM));
    let v = embed(&embeddings, "salary band");
    store
        .upsert(&[
            chunk(1, 0, "body", "public description", unit(1)),
            chunk(1, 1, "secret_note", "salary band confidential", v),
        ])
        .unwrap();
    let check = Arc::new(Lists::new(&[rid(1)], &["body"]));
    let retriever = retriever(&store, &check, &embeddings);
    let hits = retriever
        .search(&alice(), &Query::new("salary band", 5))
        .unwrap();
    assert!(
        hits.hits
            .iter()
            .all(|hit| !hit.snippet.contains("confidential")),
        "{hits:?}"
    );
}

/// A snapshot that knows the `note` DocType, so the policy can name its fields.
fn snapshot() -> Snapshot {
    let mut note = Definition::new("note");
    note.fields = vec![
        FieldSpec::new("title", FieldType::Text),
        FieldSpec::new("body", FieldType::Text),
    ];
    let meta = Registry::new(vec![note]).unwrap().effective();
    Snapshot::build(&meta, &Model::default()).unwrap()
}

const POLICY: &str = r#"
[defaults]
max_chunk_chars = 120
overlap_chars = 12

[doctypes.note]
fields = ["title", "body"]
"#;

fn long_body() -> String {
    "The quarterly review covers revenue, costs and the forecast. ".repeat(10)
}

#[test]
fn the_index_follows_the_change_feed_without_stale_vectors_or_duplicates() {
    let Some(fixture) = fixture() else {
        return;
    };
    let store = Arc::new(fixture.store("acme"));
    let documents = Arc::new(FakeDocuments::new());
    let long = rid(1);
    let other = rid(2);
    documents.put_str(
        "note",
        long,
        Version(1),
        &[("title", "Review"), ("body", &long_body())],
    );
    documents.put_str(
        "note",
        long,
        Version(2),
        &[("title", "Review"), ("body", "Short now.")],
    );
    documents.put_str(
        "note",
        other,
        Version(1),
        &[("title", "Other"), ("body", "Kept.")],
    );

    let positions = Arc::new(FakePositions::new());
    let embeddings = Arc::new(FakeEmbedding::with_dim(DIM));
    let mut indexer = Indexer::new(
        SiteId::new("acme").unwrap(),
        IndexPolicy::load(POLICY, &snapshot()).unwrap(),
        Arc::clone(&documents) as Arc<dyn DocumentSource>,
        Arc::clone(&embeddings) as Arc<dyn EmbeddingProvider>,
        Arc::clone(&store) as Arc<dyn VectorStore>,
        Arc::clone(&positions) as Arc<dyn spot_retrieval::PositionStore>,
        IndexerConfig::default(),
    );
    let now = SystemTime::UNIX_EPOCH;
    let at = |seq: u64, id: RecordId, op: ChangeOp, version: u64| {
        change(seq, "note", id, op, Version(version)).unwrap()
    };

    // 1. Insert both documents: the long one is windowed into several chunks.
    let feed = FakeFeed::new(vec![
        at(1, long, ChangeOp::Insert, 1),
        at(2, other, ChangeOp::Insert, 1),
    ]);
    let report = block_on(indexer.pump_once(&feed, 10, now)).unwrap();
    assert_eq!(report.indexed, 2);
    let before = store.chunks_of("note", long).unwrap();
    assert!(
        before.len() > 2,
        "the long body is windowed: {}",
        before.len()
    );
    let total = store.count().unwrap();

    // 2. A crash loses the position: replaying from the start changes nothing.
    let replay_positions = Arc::new(FakePositions::new());
    let mut replay = Indexer::new(
        SiteId::new("acme").unwrap(),
        IndexPolicy::load(POLICY, &snapshot()).unwrap(),
        Arc::clone(&documents) as Arc<dyn DocumentSource>,
        Arc::clone(&embeddings) as Arc<dyn EmbeddingProvider>,
        Arc::clone(&store) as Arc<dyn VectorStore>,
        replay_positions,
        IndexerConfig::default(),
    );
    block_on(replay.pump_once(&feed, 10, now)).unwrap();
    assert_eq!(
        store.count().unwrap(),
        total,
        "a replay duplicated or lost chunks"
    );

    // 3. The document is updated to a short body: re-embedded, and the stale windows are gone.
    let feed = FakeFeed::new(vec![at(3, long, ChangeOp::Update, 2)]);
    block_on(indexer.pump_once(&feed, 10, now)).unwrap();
    let after = store.chunks_of("note", long).unwrap();
    assert!(after.iter().all(|c| c.version == Version(2)));
    assert!(
        after.iter().all(|c| !c.text.contains("quarterly review")),
        "a stale vector of the old text survived the update"
    );
    assert!(after.len() < before.len());

    // 4. The document is deleted: its vectors go in the same pump; the other document stays.
    let feed = FakeFeed::new(vec![at(4, long, ChangeOp::Delete, 3)]);
    let report = block_on(indexer.pump_once(&feed, 10, now)).unwrap();
    assert_eq!(report.deleted, 1);
    assert!(store.chunks_of("note", long).unwrap().is_empty());
    assert_eq!(store.chunks_of("note", other).unwrap().len(), 2);
}
