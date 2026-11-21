//! The shared conformance suite, run against the in-memory store and the PostgreSQL store.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod common;

use spot_retrieval::{MemoryVectorStore, VectorStore};
use spot_retrieval_pg::conformance::{DIM, run_all};

#[test]
fn the_memory_store_passes_the_suite() {
    run_all(&mut || -> Box<dyn VectorStore> { Box::new(MemoryVectorStore::new()) }).unwrap();
}

#[test]
fn the_postgres_store_passes_the_suite() {
    let Some(fixture) = common::fixture() else {
        return;
    };
    // One tenant per case: the tenant is part of every key, so each case starts empty.
    let mut next = 0u32;
    run_all(&mut || -> Box<dyn VectorStore> {
        next += 1;
        let store = fixture.store(&format!("case-{next}"));
        assert_eq!(store.dim(), DIM);
        Box::new(store)
    })
    .unwrap();
}

#[test]
fn the_postgres_store_passes_the_suite_on_an_ivfflat_index() {
    let Some(fixture) = common::fixture() else {
        return;
    };
    // IVFFlat is built from the rows that exist; with a handful of rows a single list is exact.
    let kind = spot_retrieval_pg::IndexKind::Ivfflat { lists: 1 };
    fixture
        .store_with("ivf-setup", DIM, kind)
        .prepare()
        .unwrap();
    let mut next = 0u32;
    run_all(&mut || -> Box<dyn VectorStore> {
        next += 1;
        Box::new(fixture.store_with(&format!("ivf-{next}"), DIM, kind))
    })
    .unwrap();
}
