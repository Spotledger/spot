//! The conformance suite every [`VectorStore`] must pass, shared by the in-memory store and
//! [`PgVectorStore`](crate::PgVectorStore).
//!
//! Each case takes a **fresh, empty** store of vector width [`DIM`] and returns `Err(reason)`
//! instead of panicking, so the suite is ordinary library code and a host can run it against a
//! store of its own. [`run_all`] asks a factory for one fresh store per case.
//!
//! The central case, `nearest_neighbours_equal_the_reference`, writes a fixed corpus into the
//! store under test and into a [`MemoryVectorStore`], then asks both the same queries and
//! requires the same chunks in the same order with scores equal within [`TOLERANCE`]. The corpus
//! is built so no two chunks are collinear (different float rounding in two cosine
//! implementations cannot reorder them) except where a case wants an exact tie.
//!
//! What the suite does **not** cover, because the stores differ on purpose: a vector of the
//! wrong width (the in-memory store scores it zero; the PostgreSQL store refuses it) and a
//! stored zero vector. Those are tested per store.
#![allow(clippy::float_arithmetic)] // score comparison within a tolerance

use std::collections::BTreeSet;

use spot_retrieval::{Chunk, MemoryVectorStore, ScoredChunk, VectorStore, cosine};
use spot_storage_contract::{RecordId, Version};

/// The vector width every case uses.
pub const DIM: usize = 8;

/// How far two scores of the same pair may differ (`f32` cosine versus the database's).
pub const TOLERANCE: f32 = 1e-4;

/// One case: its name and the function that runs it on a fresh store.
pub type Case = (&'static str, fn(&dyn VectorStore) -> Result<(), String>);

/// Every case, in order.
pub const CASES: &[Case] = &[
    ("an_empty_store_finds_nothing", empty_store),
    ("a_zero_limit_returns_nothing", zero_limit),
    ("a_chunk_reads_back_exactly", round_trip),
    (
        "nearest_neighbours_equal_the_reference",
        nearest_neighbours_equal_the_reference,
    ),
    ("a_newer_version_replaces_the_older", newer_replaces_older),
    ("an_older_version_is_a_no_op", older_is_a_no_op),
    ("replaying_a_version_changes_nothing", replay_is_idempotent),
    (
        "an_older_chunk_in_a_newer_batch_is_dropped",
        older_chunk_in_batch_dropped,
    ),
    (
        "delete_removes_every_chunk_of_one_document",
        delete_document,
    ),
    ("deleting_an_absent_document_is_fine", delete_absent),
    (
        "the_same_id_in_two_doctypes_is_two_documents",
        doctype_is_part_of_the_key,
    ),
    ("exact_ties_break_by_doctype_id_chunk", ties_break_by_key),
    ("the_limit_bounds_the_result", limit_bounds_result),
];

/// Runs every case, each on a fresh store from `fresh`.
///
/// # Errors
///
/// The first failing case's name and reason.
pub fn run_all(fresh: &mut dyn FnMut() -> Box<dyn VectorStore>) -> Result<(), String> {
    for (name, case) in CASES {
        let store = fresh();
        case(store.as_ref()).map_err(|reason| format!("{name}: {reason}"))?;
    }
    Ok(())
}

/// Fails with `message` unless `condition` holds.
fn check(condition: bool, message: &str) -> Result<(), String> {
    if condition {
        Ok(())
    } else {
        Err(message.to_owned())
    }
}

/// A deterministic UUIDv7 record id from a counter.
#[must_use]
pub fn rid(n: u8) -> RecordId {
    let mut bytes = [0u8; 16];
    bytes[6] = 0x70;
    bytes[8] = 0x80;
    bytes[15] = n;
    // A fixed byte pattern with the version nibble set is always a valid UUIDv7.
    RecordId::from_bytes(bytes).unwrap_or_else(|_| RecordId::new_v7())
}

/// A vector of [`DIM`] components from small integers (no float arithmetic in the corpus).
fn vector(components: [i8; DIM]) -> Vec<f32> {
    components.iter().map(|c| f32::from(*c)).collect()
}

/// A chunk of `doctype`/`n` at `version`, window `chunk_no`, in one field.
fn chunk(doctype: &str, n: u8, version: u64, chunk_no: u32, vec: [i8; DIM]) -> Chunk {
    Chunk {
        doctype: doctype.to_owned(),
        id: rid(n),
        version: Version(version),
        chunk_no,
        fields: vec!["body".to_owned()],
        text: format!("{doctype} {n} v{version} window {chunk_no}"),
        vector: vector(vec),
    }
}

/// The identity of a chunk, without its text or vector.
type Key = (String, RecordId, u32, u64);

fn key_of(hit: &ScoredChunk) -> Key {
    (
        hit.chunk.doctype.clone(),
        hit.chunk.id,
        hit.chunk.chunk_no,
        hit.chunk.version.0,
    )
}

/// Every chunk of one document the store holds, found by a wide search.
fn chunks_of(
    store: &dyn VectorStore,
    doctype: &str,
    id: RecordId,
) -> Result<Vec<ScoredChunk>, String> {
    let probe = vector([1, 1, 1, 1, 1, 1, 1, 1]);
    let all = store
        .search(&probe, 1_000)
        .map_err(|e| format!("search failed: {e}"))?;
    Ok(all
        .into_iter()
        .filter(|hit| hit.chunk.doctype == doctype && hit.chunk.id == id)
        .collect())
}

fn put(store: &dyn VectorStore, chunks: &[Chunk]) -> Result<(), String> {
    store
        .upsert(chunks)
        .map_err(|e| format!("upsert failed: {e}"))
}

fn empty_store(store: &dyn VectorStore) -> Result<(), String> {
    let hits = store
        .search(&vector([1, 0, 0, 0, 0, 0, 0, 0]), 10)
        .map_err(|e| e.to_string())?;
    check(hits.is_empty(), "an empty store returned a hit")
}

fn zero_limit(store: &dyn VectorStore) -> Result<(), String> {
    put(store, &[chunk("task", 1, 1, 0, [1, 2, 3, 4, 5, 6, 7, 8])])?;
    let hits = store
        .search(&vector([1, 2, 3, 4, 5, 6, 7, 8]), 0)
        .map_err(|e| e.to_string())?;
    check(hits.is_empty(), "a limit of zero returned a hit")
}

fn round_trip(store: &dyn VectorStore) -> Result<(), String> {
    let mut written = chunk("note", 3, 7, 2, [3, -1, 4, -1, 5, -9, 2, -6]);
    written.fields = vec!["title".to_owned(), "body".to_owned()];
    written.text = "Zażółć gęślą jaźń\nline two, \"quoted\" and 'single' \\ done".to_owned();
    put(store, std::slice::from_ref(&written))?;
    let hits = store
        .search(&written.vector, 5)
        .map_err(|e| e.to_string())?;
    check(hits.len() == 1, "the chunk is not the only hit")?;
    let read = &hits[0].chunk;
    check(read.doctype == written.doctype, "the DocType changed")?;
    check(read.id == written.id, "the record id changed")?;
    check(read.version == written.version, "the version changed")?;
    check(
        read.chunk_no == written.chunk_no,
        "the chunk number changed",
    )?;
    check(read.fields == written.fields, "the field names changed")?;
    check(read.text == written.text, "the text changed")?;
    check(read.vector == written.vector, "the vector changed")?;
    check(
        (hits[0].score - 1.0).abs() <= TOLERANCE,
        "a chunk is not its own nearest neighbour at cosine one",
    )
}

/// The fixed corpus: three DocTypes, two documents with several windows.
fn corpus() -> Vec<Chunk> {
    vec![
        chunk("task", 1, 1, 0, [9, 1, 0, 0, 2, 0, 0, 1]),
        chunk("task", 1, 1, 1, [0, 8, 1, 0, 0, 3, 0, 0]),
        chunk("task", 2, 4, 0, [1, 0, 7, 2, 0, 0, 5, 0]),
        chunk("note", 3, 2, 0, [0, 0, 1, 9, 1, 0, 0, 4]),
        chunk("note", 4, 1, 0, [4, 4, 0, 0, 6, 1, 0, 0]),
        chunk("project", 5, 3, 0, [0, 2, 0, 3, 0, 8, 1, 0]),
        chunk("project", 5, 3, 1, [1, 1, 1, 1, 0, 0, 0, 9]),
        chunk("project", 6, 1, 0, [0, 0, 6, 0, 3, 0, 2, 5]),
    ]
}

fn nearest_neighbours_equal_the_reference(store: &dyn VectorStore) -> Result<(), String> {
    let reference = MemoryVectorStore::new();
    let corpus = corpus();
    put(store, &corpus)?;
    reference.upsert(&corpus).map_err(|e| e.to_string())?;
    let queries = [
        [9, 1, 0, 0, 2, 0, 0, 1],
        [0, 3, 1, 0, 1, 6, 0, 0],
        [2, 0, 5, 3, 0, 0, 4, 1],
        [1, 2, 3, 4, 5, 6, 7, 8],
        [0, 0, 0, 7, 0, 1, 0, 5],
        [-3, 1, 0, 2, -1, 4, 0, 1],
    ];
    for (q, query) in queries.iter().enumerate() {
        let query = vector(*query);
        for limit in [1, 3, 5, 100] {
            let got = store.search(&query, limit).map_err(|e| e.to_string())?;
            let want = reference.search(&query, limit).map_err(|e| e.to_string())?;
            check(
                got.len() == want.len(),
                &format!(
                    "query {q} limit {limit}: {} hits, reference {}",
                    got.len(),
                    want.len()
                ),
            )?;
            for (rank, (g, w)) in got.iter().zip(want.iter()).enumerate() {
                check(
                    key_of(g) == key_of(w),
                    &format!("query {q} limit {limit} rank {rank}: a different chunk"),
                )?;
                check(
                    (g.score - w.score).abs() <= TOLERANCE,
                    &format!("query {q} limit {limit} rank {rank}: score differs"),
                )?;
                check(
                    (g.score - cosine(&query, &g.chunk.vector)).abs() <= TOLERANCE,
                    &format!("query {q} limit {limit} rank {rank}: not the cosine"),
                )?;
            }
        }
    }
    Ok(())
}

fn newer_replaces_older(store: &dyn VectorStore) -> Result<(), String> {
    put(
        store,
        &[
            chunk("task", 1, 1, 0, [1, 2, 0, 0, 0, 0, 0, 0]),
            chunk("task", 1, 1, 1, [0, 1, 2, 0, 0, 0, 0, 0]),
            chunk("task", 1, 1, 2, [0, 0, 1, 2, 0, 0, 0, 0]),
        ],
    )?;
    put(store, &[chunk("task", 1, 2, 0, [5, 0, 0, 0, 0, 0, 0, 1])])?;
    let left = chunks_of(store, "task", rid(1))?;
    check(left.len() == 1, "the shrunk document kept a stale chunk")?;
    check(
        left[0].chunk.version == Version(2),
        "the old version survived",
    )
}

fn older_is_a_no_op(store: &dyn VectorStore) -> Result<(), String> {
    put(store, &[chunk("task", 1, 5, 0, [1, 2, 0, 0, 0, 0, 0, 0])])?;
    put(
        store,
        &[
            chunk("task", 1, 3, 0, [0, 0, 0, 0, 0, 0, 3, 1]),
            chunk("task", 1, 3, 1, [0, 0, 0, 0, 0, 3, 1, 0]),
        ],
    )?;
    let left = chunks_of(store, "task", rid(1))?;
    check(left.len() == 1, "an older version added chunks")?;
    check(left[0].chunk.version == Version(5), "an older version won")
}

fn replay_is_idempotent(store: &dyn VectorStore) -> Result<(), String> {
    let batch = [
        chunk("task", 1, 2, 0, [1, 2, 0, 0, 0, 0, 0, 0]),
        chunk("task", 1, 2, 1, [0, 1, 2, 0, 0, 0, 0, 0]),
    ];
    put(store, &batch)?;
    put(store, &batch)?;
    put(store, &batch)?;
    let left = chunks_of(store, "task", rid(1))?;
    check(left.len() == 2, "a replay duplicated or lost a chunk")
}

fn older_chunk_in_batch_dropped(store: &dyn VectorStore) -> Result<(), String> {
    put(
        store,
        &[
            chunk("task", 1, 2, 0, [1, 2, 0, 0, 0, 0, 0, 0]),
            chunk("task", 1, 3, 1, [0, 1, 2, 0, 0, 0, 0, 0]),
        ],
    )?;
    let left = chunks_of(store, "task", rid(1))?;
    check(left.len() == 1, "a batch kept a chunk of an older version")?;
    check(left[0].chunk.chunk_no == 1, "the wrong chunk was kept")
}

fn delete_document(store: &dyn VectorStore) -> Result<(), String> {
    put(store, &corpus())?;
    store
        .delete_document("project", rid(5))
        .map_err(|e| e.to_string())?;
    check(
        chunks_of(store, "project", rid(5))?.is_empty(),
        "a deleted document still has chunks",
    )?;
    check(
        !chunks_of(store, "project", rid(6))?.is_empty(),
        "a delete removed another document",
    )?;
    check(
        !chunks_of(store, "task", rid(1))?.is_empty(),
        "a delete removed a document of another DocType",
    )
}

fn delete_absent(store: &dyn VectorStore) -> Result<(), String> {
    store
        .delete_document("task", rid(99))
        .map_err(|e| e.to_string())?;
    put(store, &[chunk("task", 1, 1, 0, [1, 2, 0, 0, 0, 0, 0, 0])])?;
    store
        .delete_document("task", rid(99))
        .map_err(|e| e.to_string())?;
    check(
        chunks_of(store, "task", rid(1))?.len() == 1,
        "deleting an absent document removed another",
    )
}

fn doctype_is_part_of_the_key(store: &dyn VectorStore) -> Result<(), String> {
    put(
        store,
        &[
            chunk("task", 1, 1, 0, [1, 0, 0, 0, 0, 0, 0, 2]),
            chunk("note", 1, 1, 0, [0, 1, 0, 0, 0, 0, 2, 0]),
        ],
    )?;
    store
        .delete_document("task", rid(1))
        .map_err(|e| e.to_string())?;
    check(
        chunks_of(store, "task", rid(1))?.is_empty(),
        "the task survived its delete",
    )?;
    check(
        chunks_of(store, "note", rid(1))?.len() == 1,
        "deleting a task removed the note with the same id",
    )
}

fn ties_break_by_key(store: &dyn VectorStore) -> Result<(), String> {
    let same = [1, 2, 3, 4, 0, 0, 0, 0];
    put(
        store,
        &[
            chunk("task", 2, 1, 0, same),
            chunk("note", 9, 1, 0, same),
            chunk("task", 1, 1, 1, same),
            chunk("task", 1, 1, 0, same),
        ],
    )?;
    let hits = store.search(&vector(same), 10).map_err(|e| e.to_string())?;
    let order: Vec<(String, RecordId, u32)> = hits
        .iter()
        .map(|hit| (hit.chunk.doctype.clone(), hit.chunk.id, hit.chunk.chunk_no))
        .collect();
    let expected = vec![
        ("note".to_owned(), rid(9), 0),
        ("task".to_owned(), rid(1), 0),
        ("task".to_owned(), rid(1), 1),
        ("task".to_owned(), rid(2), 0),
    ];
    check(
        order == expected,
        "equal scores are not ordered by DocType, id, chunk",
    )
}

fn limit_bounds_result(store: &dyn VectorStore) -> Result<(), String> {
    put(store, &corpus())?;
    let query = vector([1, 2, 3, 4, 5, 6, 7, 8]);
    let two = store.search(&query, 2).map_err(|e| e.to_string())?;
    check(two.len() == 2, "a limit of two did not return two")?;
    let all = store.search(&query, 1_000).map_err(|e| e.to_string())?;
    check(all.len() == corpus().len(), "a wide limit lost chunks")?;
    let distinct: BTreeSet<Key> = all.iter().map(key_of).collect();
    check(distinct.len() == all.len(), "a chunk was returned twice")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn the_in_memory_store_passes_every_case() {
        run_all(&mut || -> Box<dyn VectorStore> { Box::new(MemoryVectorStore::new()) }).unwrap();
    }

    #[test]
    fn the_case_names_are_unique() {
        let names: BTreeSet<&str> = CASES.iter().map(|(name, _)| *name).collect();
        assert_eq!(names.len(), CASES.len());
    }
}
