//! WP-SEC-04.T7: the index and the retriever under the field guard's protection facts.
//!
//! The end-to-end proof with a real permission engine and the leak harness is in
//! `spot-channel-leakcheck` (`tests/search.rs`); these tests cover the indexer's own decisions
//! with the static protection source.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::SystemTime;

use spot_core::SiteId;
use spot_field_guard::{DocProtection, StaticProtection};
use spot_permissions::{FieldFacts, GroupName, Snapshot};
use spot_retrieval::testing::{
    FakeDocuments, FakeEmbedding, FakeFeed, FakePositions, block_on, change,
};
use spot_retrieval::{
    CODE_PROTECTION_UNKNOWN, EmbeddingProvider, IndexPolicy, Indexer, IndexerConfig,
    MemoryVectorStore, VectorStore,
};
use spot_storage_contract::{ChangeOp, RecordId, Version};

fn protection(with_doctype: bool) -> Arc<StaticProtection> {
    if !with_doctype {
        return Arc::new(StaticProtection::new(Vec::new()));
    }
    let mut secret = FieldFacts::plain("secret");
    secret.groups = vec![GroupName("pay".to_owned())];
    Arc::new(StaticProtection::new(vec![DocProtection {
        name: "note".to_owned(),
        fields: vec![secret],
        tables: Default::default(),
        trace_groups: Vec::new(),
    }]))
}

/// A policy loaded without a snapshot check, through the permissive path the crate tests use.
fn policy() -> IndexPolicy {
    IndexPolicy::load(
        "[doctypes.note]\nfields = [\"body\", \"secret\"]\nrestricted_ok = [\"secret\"]\n",
        &snapshot(),
    )
    .unwrap()
}

fn snapshot() -> Snapshot {
    // The snapshot the policy is validated against: both fields known, `secret` in a group.
    let meta = {
        use spot_meta::{Definition, FieldSpec, FieldType, ProtectionGroupSpec, Registry};
        let mut note = Definition::new("note");
        let mut secret = FieldSpec::new("secret", FieldType::Text);
        secret.protection_group = Some("pay".into());
        note.fields = vec![FieldSpec::new("body", FieldType::Text), secret];
        note.protection_groups = vec![ProtectionGroupSpec::new("pay", "Pay")];
        Registry::new(vec![note]).unwrap().effective()
    };
    let model = spot_permissions::Model {
        grants: Vec::new(),
        field_grants: Vec::new(),
        groups: vec![spot_permissions::ProtectionGroup {
            name: GroupName("pay".to_owned()),
            trace: false,
        }],
        ..Default::default()
    };
    Snapshot::build(&meta, &model).unwrap()
}

fn run(
    source: Arc<StaticProtection>,
    index_protected: bool,
) -> (Arc<MemoryVectorStore>, Vec<String>) {
    let documents = Arc::new(FakeDocuments::new());
    let id = RecordId::new_v7();
    documents.put_str(
        "note",
        id,
        Version(1),
        &[("body", "plain words here"), ("secret", "TOPSECRET-VALUE")],
    );
    let embeddings = Arc::new(FakeEmbedding::new());
    let store = Arc::new(MemoryVectorStore::new());
    let mut indexer = Indexer::new(
        SiteId::new("t.test").unwrap(),
        policy(),
        documents,
        embeddings.clone(),
        store.clone(),
        Arc::new(FakePositions::new()),
        IndexerConfig::default(),
    )
    .with_protection(source, index_protected);
    let feed = FakeFeed::new(vec![
        change(1, "note", id, ChangeOp::Insert, Version(1)).unwrap(),
    ]);
    let report = block_on(indexer.pump_once(&feed, 10, SystemTime::UNIX_EPOCH)).unwrap();
    let skipped = report.skipped.iter().map(|s| s.code.to_owned()).collect();
    (store, skipped)
}

fn dump(store: &MemoryVectorStore) -> String {
    let probe = FakeEmbedding::new().embed(&["x".to_owned()]).unwrap();
    store
        .search(&probe[0], 1000)
        .unwrap()
        .into_iter()
        .map(|s| s.chunk.text)
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn a_protected_field_stays_out_of_the_index_by_default() {
    let (store, skipped) = run(protection(true), false);
    let text = dump(&store);
    assert!(skipped.is_empty());
    assert!(text.contains("plain words"));
    assert!(!text.contains("TOPSECRET"), "a protected value was indexed");
}

#[test]
fn the_tenant_default_can_opt_a_listed_field_in() {
    let (store, _) = run(protection(true), true);
    assert!(dump(&store).contains("TOPSECRET"));
}

#[test]
fn an_unknown_doctype_is_not_indexed_at_all() {
    let (store, skipped) = run(protection(false), true);
    assert_eq!(skipped, vec![CODE_PROTECTION_UNKNOWN.to_owned()]);
    assert!(dump(&store).is_empty());
}
