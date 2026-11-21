//! What is specific to the PostgreSQL store: the schema, the width, the tenant key, atomic
//! replace, batch delete and paging, the zero query, and the errors that name no value.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod common;

use std::collections::BTreeSet;

use common::{DIM, fixture};
use spot_retrieval::{Chunk, MemoryVectorStore, VectorStore};
use spot_retrieval_pg::conformance::rid;
use spot_retrieval_pg::schema::{HNSW_INDEX, IVFFLAT_INDEX, TABLE};
use spot_retrieval_pg::{IndexKind, PrepareError};
use spot_storage_contract::{RecordId, Version};

fn vec8(first: f32, second: f32) -> Vec<f32> {
    let mut v = vec![0.0_f32; DIM];
    v[0] = first;
    v[1] = second;
    v[DIM - 1] = 1.0;
    v
}

fn chunk(doctype: &str, n: u8, version: u64, chunk_no: u32, vector: Vec<f32>) -> Chunk {
    Chunk {
        doctype: doctype.to_owned(),
        id: rid(n),
        version: Version(version),
        chunk_no,
        fields: vec!["body".to_owned()],
        text: format!("{doctype} {n} v{version} window {chunk_no}"),
        vector,
    }
}

#[test]
fn prepare_is_idempotent_and_the_index_follows_the_requested_kind() {
    let Some(fixture) = fixture() else {
        return;
    };
    let hnsw = fixture.store("t");
    hnsw.prepare().unwrap();
    hnsw.prepare().unwrap();
    assert_eq!(hnsw.index_method().unwrap(), Some("hnsw"));

    let ivfflat = fixture.store_with("t", DIM, IndexKind::Ivfflat { lists: 2 });
    ivfflat.prepare().unwrap();
    assert_eq!(ivfflat.index_method().unwrap(), Some("ivfflat"));

    // Exactly one vector index remains: the planner never chooses between two.
    let client = fixture.client();
    let rows = fixture.rt.block_on(client.query(
        "SELECT indexname FROM pg_indexes WHERE tablename = $1 AND indexdef LIKE '%embedding%'",
        &[&TABLE],
    ));
    let names: Vec<String> = rows
        .unwrap()
        .iter()
        .map(|row| row.get::<_, String>(0))
        .collect();
    assert_eq!(names, vec![IVFFLAT_INDEX.to_owned()]);

    hnsw.prepare().unwrap();
    assert_eq!(hnsw.index_method().unwrap(), Some("hnsw"));
    let rows = fixture
        .rt
        .block_on(client.query(
            "SELECT indexname FROM pg_indexes WHERE tablename = $1 AND indexdef LIKE '%embedding%'",
            &[&TABLE],
        ))
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, String>(0), HNSW_INDEX);
}

#[test]
fn a_table_of_another_width_is_refused_not_mixed() {
    let Some(fixture) = fixture() else {
        return;
    };
    let wide = fixture.store_with("t", DIM * 2, IndexKind::Hnsw);
    match wide.prepare() {
        Err(PrepareError::DimensionMismatch { stored, wanted }) => {
            assert_eq!(stored, i32::try_from(DIM).unwrap());
            assert_eq!(wanted, DIM * 2);
        }
        other => panic!("expected a width mismatch, got {other:?}"),
    }
}

#[test]
fn a_store_refuses_a_width_it_was_not_built_for() {
    let Some(fixture) = fixture() else {
        return;
    };
    let pool = spot_retrieval_pg::pool_for(&fixture.db.config()).unwrap();
    for bad in [0, spot_retrieval_pg::MAX_DIM + 1] {
        assert!(
            spot_retrieval_pg::PgVectorStore::new(
                pool.clone(),
                common::tenant("t"),
                bad,
                IndexKind::Hnsw,
                std::sync::Arc::clone(&fixture.rt),
            )
            .is_err(),
            "a width of {bad} is out of range"
        );
    }
    assert!(
        spot_retrieval_pg::PgVectorStore::new(
            pool,
            common::tenant("t"),
            DIM,
            IndexKind::Ivfflat { lists: 0 },
            std::sync::Arc::clone(&fixture.rt),
        )
        .is_err()
    );
}

#[test]
fn a_wrong_width_or_a_non_finite_value_is_refused_and_writes_nothing() {
    let Some(fixture) = fixture() else {
        return;
    };
    let store = fixture.store("t");
    let wrong = store.upsert(&[chunk("task", 1, 1, 0, vec![1.0, 2.0, 3.0])]);
    assert_eq!(wrong.unwrap_err().code(), "retrieval.store_failed");
    let mut nan = vec8(1.0, 2.0);
    nan[2] = f32::NAN;
    assert!(store.upsert(&[chunk("task", 1, 1, 0, nan)]).is_err());
    assert!(store.search(&[1.0, 2.0], 5).is_err());
    assert_eq!(store.count().unwrap(), 0);
}

#[test]
fn a_failing_batch_leaves_the_previous_chunks_in_place() {
    let Some(fixture) = fixture() else {
        return;
    };
    let store = fixture.store("t");
    store
        .upsert(&[chunk("task", 1, 1, 0, vec8(1.0, 2.0))])
        .unwrap();
    // Document 1 would be replaced by version 2, but document 2 carries a bad vector: the whole
    // call is refused and document 1 keeps version 1.
    let refused = store.upsert(&[
        chunk("task", 1, 2, 0, vec8(3.0, 1.0)),
        chunk("task", 2, 1, 0, vec![1.0]),
    ]);
    assert!(refused.is_err());
    let kept = store.chunks_of("task", rid(1)).unwrap();
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].version, Version(1));
    assert_eq!(store.count().unwrap(), 1);
}

#[test]
fn the_tenant_is_part_of_every_key() {
    let Some(fixture) = fixture() else {
        return;
    };
    let a = fixture.store("tenant-a");
    let b = fixture.store("tenant-b");
    // The same document key and the same vector in both tenants.
    a.upsert(&[chunk("task", 1, 1, 0, vec8(1.0, 2.0))]).unwrap();
    b.upsert(&[
        chunk("task", 1, 1, 0, vec8(1.0, 2.0)),
        chunk("task", 2, 1, 0, vec8(2.0, 1.0)),
    ])
    .unwrap();
    assert_eq!(a.count().unwrap(), 1);
    assert_eq!(b.count().unwrap(), 2);
    let probe = vec8(1.0, 2.0);
    assert_eq!(a.search(&probe, 10).unwrap().len(), 1);
    assert_eq!(b.search(&probe, 10).unwrap().len(), 2);
    assert!(a.chunks_of("task", rid(2)).unwrap().is_empty());

    // A delete and a replace in one tenant never touch the other.
    a.delete_document("task", rid(1)).unwrap();
    assert_eq!(a.count().unwrap(), 0);
    assert_eq!(b.chunks_of("task", rid(1)).unwrap().len(), 1);
    a.upsert(&[chunk("task", 1, 9, 0, vec8(5.0, 1.0))]).unwrap();
    assert_eq!(
        b.chunks_of("task", rid(1)).unwrap()[0].version,
        Version(1),
        "tenant B's version is untouched by tenant A's newer write"
    );
    // A tenant's paging lists only its own documents.
    let page = b.documents_page(None, 10).unwrap();
    assert_eq!(page.len(), 2);
}

#[test]
fn the_table_holds_the_chunk_and_no_permission_data() {
    let Some(fixture) = fixture() else {
        return;
    };
    let client = fixture.client();
    let rows = fixture
        .rt
        .block_on(client.query(
            "SELECT column_name::text FROM information_schema.columns WHERE table_name::text = $1",
            &[&TABLE],
        ))
        .unwrap();
    let columns: BTreeSet<String> = rows.iter().map(|row| row.get::<_, String>(0)).collect();
    let expected: BTreeSet<String> = [
        "tenant",
        "doctype",
        "id",
        "chunk_no",
        "version",
        "fields",
        "body",
        "embedding",
    ]
    .iter()
    .map(|name| (*name).to_owned())
    .collect();
    assert_eq!(
        columns, expected,
        "the index carries no column beyond the chunk"
    );
}

#[test]
fn a_batch_delete_and_paging_walk_the_documents_in_key_order() {
    let Some(fixture) = fixture() else {
        return;
    };
    let store = fixture.store("t");
    let documents: [(&str, u8); 5] = [
        ("task", 2),
        ("note", 7),
        ("task", 1),
        ("Zeta", 4),
        ("note", 3),
    ];
    for (i, (doctype, n)) in documents.iter().enumerate() {
        let f = f32::from(u8::try_from(i).unwrap());
        // Two windows each, so a page lists a document once.
        store
            .upsert(&[
                chunk(doctype, *n, 1, 0, vec8(f, 1.0)),
                chunk(doctype, *n, 1, 1, vec8(1.0, f)),
            ])
            .unwrap();
    }
    assert_eq!(store.count().unwrap(), 10);

    let mut expected: Vec<(String, RecordId)> = documents
        .iter()
        .map(|(doctype, n)| ((*doctype).to_owned(), rid(*n)))
        .collect();
    // Byte order of the DocType, then the record id: "Zeta" sorts before the lower-case names.
    expected.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()).then(a.1.cmp(&b.1)));

    let mut walked: Vec<(String, RecordId)> = Vec::new();
    let mut after: Option<(String, RecordId)> = None;
    loop {
        let page = store
            .documents_page(after.as_ref().map(|(d, id)| (d.as_str(), *id)), 2)
            .unwrap();
        if page.is_empty() {
            break;
        }
        after = page.last().cloned();
        walked.extend(page);
    }
    assert_eq!(walked, expected);

    store
        .delete_documents(&[("task", rid(2)), ("note", rid(3)), ("task", rid(99))])
        .unwrap();
    assert_eq!(store.count().unwrap(), 6);
    assert!(store.chunks_of("task", rid(2)).unwrap().is_empty());
    assert_eq!(store.documents_page(None, 100).unwrap().len(), 3);
}

#[test]
fn a_zero_query_ranks_by_key_with_every_score_zero_like_the_memory_store() {
    let Some(fixture) = fixture() else {
        return;
    };
    let store = fixture.store("t");
    let reference = MemoryVectorStore::new();
    let chunks = [
        chunk("task", 2, 1, 0, vec8(1.0, 2.0)),
        chunk("note", 5, 1, 0, vec8(2.0, 1.0)),
        chunk("task", 1, 1, 0, vec8(4.0, 4.0)),
    ];
    store.upsert(&chunks).unwrap();
    reference.upsert(&chunks).unwrap();
    let zero = vec![0.0_f32; DIM];
    let got = store.search(&zero, 10).unwrap();
    let want = reference.search(&zero, 10).unwrap();
    assert_eq!(got.len(), 3);
    for (g, w) in got.iter().zip(want.iter()) {
        assert_eq!(
            (&g.chunk.doctype, g.chunk.id),
            (&w.chunk.doctype, w.chunk.id)
        );
        assert_eq!(g.score.to_bits(), 0.0_f32.to_bits());
    }
}

#[test]
fn an_error_and_the_debug_output_name_no_value() {
    let Some(fixture) = fixture() else {
        return;
    };
    let store = fixture.store("tenant-secret-name");
    let mut bad = chunk("task", 1, 1, 0, vec![9.0, 9.0]);
    bad.text = "TOP-SECRET-CELL-VALUE".to_owned();
    let error = store.upsert(&[bad]).unwrap_err();
    let text = format!("{error} {error:?}");
    assert!(!text.contains("TOP-SECRET-CELL-VALUE"), "{text}");
    assert!(!text.contains("tenant-secret-name"), "{text}");
    assert!(!text.contains("9.0"), "{text}");
    let debug = format!("{store:?}");
    assert!(debug.contains("PgVectorStore"), "{debug}");
    assert!(!debug.contains("postgres://"), "{debug}");
}

#[test]
fn the_text_comes_back_exactly_as_it_was_given() {
    let Some(fixture) = fixture() else {
        return;
    };
    let store = fixture.store("t");
    let mut written = chunk("note", 3, 4, 1, vec8(2.0, 7.0));
    written.fields = vec!["title".to_owned(), "body".to_owned()];
    written.text =
        "  leading and trailing  \n\ttabs, \"quotes\", 'apostrophes', \\ and $1; --".to_owned();
    store.upsert(std::slice::from_ref(&written)).unwrap();
    let read = store.chunks_of("note", rid(3)).unwrap();
    assert_eq!(read, vec![written]);
}
