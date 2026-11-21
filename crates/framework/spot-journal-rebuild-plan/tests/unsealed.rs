//! Tests of the unsealed-tail reader (WP-JRN-05.T8, ADR-061 Appendix A.3, A.4, A.6).
//!
//! The chunk layout is the relay's business and the relay's decoder is plugged in by the caller,
//! so these tests use a small text codec that has the two versions (`T1`, `T2`) and a way to
//! fail each A.3 letter. What is proved is the reader's rules: order, covered, straddling, gaps,
//! damage, a closed generation, and that nothing is ever written or deleted.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::sync::Mutex;

use futures::executor::block_on;
use spot_journal_rebuild_plan::unsealed::{
    ChunkFault, TailChunkReader, TailEntry, TailPosition, UnsealedError, read_after,
};
use spot_journal_store::{
    JournalStore, Layout, MemoryStore, ObjectEntry, ObjectMeta, StoreFuture, StoredObject, TailKey,
};

const TENANT: &str = "acme";

/// A memory store that records every call, so a test can assert that nothing was written or
/// deleted.
struct LoggedStore {
    inner: MemoryStore,
    log: Mutex<Vec<String>>,
}

impl LoggedStore {
    fn new() -> Self {
        Self {
            inner: MemoryStore::new(),
            log: Mutex::new(Vec::new()),
        }
    }

    fn note(&self, entry: String) {
        self.log.lock().unwrap().push(entry);
    }

    fn operations(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    fn forget(&self) {
        self.log.lock().unwrap().clear();
    }

    /// Seeds without logging.
    fn seed(&self, key: &str, bytes: &[u8]) {
        block_on(self.inner.put_new(key, bytes.to_vec())).expect("seed");
    }
}

impl JournalStore for LoggedStore {
    fn put_new<'a>(&'a self, key: &'a str, bytes: Vec<u8>) -> StoreFuture<'a, ObjectMeta> {
        self.note(format!("put_new {key}"));
        self.inner.put_new(key, bytes)
    }
    fn put_replace<'a>(
        &'a self,
        key: &'a str,
        bytes: Vec<u8>,
        expected_etag: &'a str,
    ) -> StoreFuture<'a, ObjectMeta> {
        self.note(format!("put_replace {key}"));
        self.inner.put_replace(key, bytes, expected_etag)
    }
    fn get<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Option<StoredObject>> {
        self.note(format!("get {key}"));
        self.inner.get(key)
    }
    fn head<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Option<ObjectMeta>> {
        self.note(format!("head {key}"));
        self.inner.head(key)
    }
    fn list<'a>(
        &'a self,
        prefix: &'a str,
        after: Option<&'a str>,
        limit: usize,
    ) -> StoreFuture<'a, Vec<ObjectEntry>> {
        self.note(format!("list {prefix}"));
        self.inner.list(prefix, after, limit)
    }
    fn delete<'a>(&'a self, key: &'a str) -> StoreFuture<'a, ()> {
        self.note(format!("delete {key}"));
        self.inner.delete(key)
    }
}

/// The test codec: `T1|gen:seq:text|gen:seq:text`, or `T2|...`. `BAD<letter>` fails that letter.
struct Codec;

impl TailChunkReader for Codec {
    fn decode(
        &self,
        _key: &TailKey,
        _object_key: &str,
        bytes: &[u8],
    ) -> Result<Vec<TailEntry>, ChunkFault> {
        let text = std::str::from_utf8(bytes).map_err(|_| ChunkFault { letter: 'c' })?;
        if let Some(letter) = text.strip_prefix("BAD") {
            return Err(ChunkFault {
                letter: letter.chars().next().unwrap_or('c'),
            });
        }
        let mut parts = text.split('|');
        match parts.next() {
            Some("T1" | "T2") => {}
            _ => return Err(ChunkFault { letter: 'c' }),
        }
        let mut entries = Vec::new();
        for part in parts {
            let mut fields = part.splitn(3, ':');
            let generation = fields.next().and_then(|v| v.parse().ok());
            let seq = fields.next().and_then(|v| v.parse().ok());
            let line = fields.next();
            let (Some(generation), Some(seq), Some(line)) = (generation, seq, line) else {
                return Err(ChunkFault { letter: 'c' });
            };
            entries.push(TailEntry {
                position: TailPosition { generation, seq },
                line: line.as_bytes().to_vec(),
            });
        }
        Ok(entries)
    }
}

fn tail_key(generation: u64, batch_id: u64) -> String {
    Layout::tail_chunk(TENANT, generation, batch_id)
        .expect("key")
        .to_key()
}

/// A chunk object: `version` then the seqs `first..=last` of `generation`.
fn chunk(version: u8, generation: u64, first: u64, last: u64) -> Vec<u8> {
    let mut out = format!("T{version}");
    for seq in first..=last {
        out.push_str(&format!("|{generation}:{seq}:line-{seq}"));
    }
    out.into_bytes()
}

fn put(store: &LoggedStore, generation: u64, batch_id: u64, bytes: &[u8]) {
    store.seed(&tail_key(generation, batch_id), bytes);
}

fn read(
    store: &LoggedStore,
    generation: u64,
    manifest_last: u64,
) -> Result<Vec<u64>, UnsealedError> {
    block_on(read_after(store, TENANT, generation, manifest_last, &Codec)).map(|tail| {
        tail.entries()
            .iter()
            .map(|entry| entry.get().position.seq)
            .collect()
    })
}

#[test]
fn chunks_are_read_in_batch_order_not_listing_order() {
    let store = LoggedStore::new();
    // Batch ids 2 and 10: the lexical order of the unpadded numbers would put 10 first; the
    // padded keys sort numerically anyway, so make the seqs the witness of the order.
    put(&store, 1, 10, &chunk(2, 1, 6, 8));
    put(&store, 1, 2, &chunk(2, 1, 4, 5));
    put(&store, 1, 3, &chunk(2, 1, 5, 5)); // overlaps: must fail, proving batch order is used
    let error = read(&store, 1, 3).expect_err("overlap");
    assert!(matches!(
        error,
        UnsealedError::Gap {
            expected: 6,
            found: 5,
            batch_id: 3
        }
    ));

    let store = LoggedStore::new();
    put(&store, 1, 10, &chunk(2, 1, 6, 8));
    put(&store, 1, 2, &chunk(2, 1, 4, 5));
    assert_eq!(read(&store, 1, 3).expect("tail"), vec![4, 5, 6, 7, 8]);
}

#[test]
fn nothing_beyond_the_manifest_is_an_empty_tail() {
    let store = LoggedStore::new();
    let tail = block_on(read_after(&store, TENANT, 1, 0, &Codec)).expect("tail");
    assert!(tail.is_empty());
    assert_eq!(tail.last_position(), None);
    assert!(tail.covered().is_empty());
}

#[test]
fn entries_carry_their_positions_in_stream_order() {
    let store = LoggedStore::new();
    put(&store, 2, 1, &chunk(1, 2, 1, 2));
    put(&store, 2, 2, &chunk(2, 2, 3, 3));
    let tail = block_on(read_after(&store, TENANT, 2, 0, &Codec)).expect("tail");
    let positions: Vec<TailPosition> = tail
        .entries()
        .iter()
        .map(|entry| entry.get().position)
        .collect();
    assert_eq!(
        positions,
        vec![
            TailPosition {
                generation: 2,
                seq: 1
            },
            TailPosition {
                generation: 2,
                seq: 2
            },
            TailPosition {
                generation: 2,
                seq: 3
            },
        ]
    );
    assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
    assert_eq!(
        tail.last_position(),
        Some(TailPosition {
            generation: 2,
            seq: 3
        })
    );
    assert_eq!(tail.generation(), 2);
    let line = tail.entries()[0].get().line.clone();
    assert_eq!(line, b"line-1".to_vec());
}

#[test]
fn covered_chunks_are_reported_not_deleted() {
    let store = LoggedStore::new();
    put(&store, 1, 1, &chunk(1, 1, 1, 3));
    put(&store, 1, 2, &chunk(1, 1, 4, 6));
    put(&store, 1, 3, &chunk(2, 1, 7, 9));
    store.forget();
    let tail = block_on(read_after(&store, TENANT, 1, 6, &Codec)).expect("tail");
    let covered: Vec<u64> = tail.covered().iter().map(|c| c.batch_id).collect();
    assert_eq!(covered, vec![1, 2]);
    assert_eq!(tail.covered()[0].object_key, tail_key(1, 1));
    let seqs: Vec<u64> = tail
        .entries()
        .iter()
        .map(|e| e.get().position.seq)
        .collect();
    assert_eq!(seqs, vec![7, 8, 9]);
    assert_eq!(tail.manifest_last_seq(), 6);
    // Still there afterwards.
    assert!(
        block_on(store.inner.head(&tail_key(1, 1)))
            .expect("head")
            .is_some()
    );
}

#[test]
fn a_straddling_chunk_is_tampering() {
    let store = LoggedStore::new();
    put(&store, 1, 1, &chunk(2, 1, 3, 7));
    let error = read(&store, 1, 5).expect_err("straddle");
    assert_eq!(error, UnsealedError::Tampered { batch_id: 1 });
    assert_eq!(error.code(), "journal_rebuild_plan.unsealed_tampered");
}

#[test]
fn a_covered_chunk_after_a_beyond_chunk_is_out_of_order() {
    let store = LoggedStore::new();
    put(&store, 1, 1, &chunk(2, 1, 6, 8));
    put(&store, 1, 2, &chunk(2, 1, 1, 2));
    assert_eq!(
        read(&store, 1, 5).expect_err("order"),
        UnsealedError::Tampered { batch_id: 2 }
    );
}

#[test]
fn the_first_chunk_must_start_right_after_the_manifest() {
    let store = LoggedStore::new();
    put(&store, 1, 1, &chunk(2, 1, 8, 9));
    assert_eq!(
        read(&store, 1, 5).expect_err("gap"),
        UnsealedError::Gap {
            expected: 6,
            found: 8,
            batch_id: 1
        }
    );
}

#[test]
fn a_gap_between_chunks_is_a_gap() {
    let store = LoggedStore::new();
    put(&store, 1, 1, &chunk(2, 1, 1, 3));
    put(&store, 1, 2, &chunk(2, 1, 5, 6));
    let error = read(&store, 1, 0).expect_err("gap");
    assert_eq!(
        error,
        UnsealedError::Gap {
            expected: 4,
            found: 5,
            batch_id: 2
        }
    );
    assert_eq!(error.code(), "journal_rebuild_plan.unsealed_gap");
}

#[test]
fn a_damaged_chunk_stops_the_generation_with_its_letter() {
    for letter in ['b', 'c', 'd', 'e'] {
        let store = LoggedStore::new();
        put(&store, 1, 1, &chunk(2, 1, 1, 2));
        put(&store, 1, 2, format!("BAD{letter}").as_bytes());
        put(&store, 1, 3, &chunk(2, 1, 3, 4));
        assert_eq!(
            read(&store, 1, 0).expect_err("damaged"),
            UnsealedError::Damaged {
                batch_id: 2,
                letter
            },
            "letter {letter}"
        );
    }
}

#[test]
fn an_unknown_magic_fails_closed() {
    let store = LoggedStore::new();
    put(&store, 1, 1, b"T9|1:1:x");
    assert_eq!(
        read(&store, 1, 0).expect_err("magic"),
        UnsealedError::Damaged {
            batch_id: 1,
            letter: 'c'
        }
    );
}

#[test]
fn the_reader_checks_generation_count_and_order_itself() {
    // (d): an entry of another generation inside the chunk.
    let store = LoggedStore::new();
    put(&store, 1, 1, b"T2|1:1:a|2:2:b");
    assert_eq!(
        read(&store, 1, 0).expect_err("generation"),
        UnsealedError::Damaged {
            batch_id: 1,
            letter: 'd'
        }
    );
    // (d): a zero-count chunk decodes and is invalid.
    let store = LoggedStore::new();
    put(&store, 1, 1, b"T2");
    assert_eq!(
        read(&store, 1, 0).expect_err("count"),
        UnsealedError::Damaged {
            batch_id: 1,
            letter: 'd'
        }
    );
    // (e): seq not strictly consecutive inside a chunk.
    let store = LoggedStore::new();
    put(&store, 1, 1, b"T2|1:1:a|1:3:b");
    assert_eq!(
        read(&store, 1, 0).expect_err("order"),
        UnsealedError::Damaged {
            batch_id: 1,
            letter: 'e'
        }
    );
}

#[test]
fn version_one_and_two_chunks_mix() {
    let store = LoggedStore::new();
    put(&store, 1, 1, &chunk(1, 1, 1, 2));
    put(&store, 1, 2, &chunk(2, 1, 3, 4));
    put(&store, 1, 3, &chunk(1, 1, 5, 5));
    assert_eq!(read(&store, 1, 0).expect("tail"), vec![1, 2, 3, 4, 5]);
}

#[test]
fn another_generations_chunks_are_not_read() {
    let store = LoggedStore::new();
    put(&store, 1, 1, &chunk(2, 1, 1, 9));
    put(&store, 2, 1, &chunk(2, 2, 1, 2));
    assert_eq!(read(&store, 2, 0).expect("tail"), vec![1, 2]);
    assert_eq!(read(&store, 1, 0).expect("tail").len(), 9);
}

#[test]
fn a_closed_generation_is_reported_and_still_read() {
    let store = LoggedStore::new();
    put(&store, 1, 1, &chunk(2, 1, 1, 2));
    let open = block_on(read_after(&store, TENANT, 1, 0, &Codec)).expect("tail");
    assert!(!open.is_closed());
    // The successor's manifest exists: generation 1 is closed.
    store.seed(
        &Layout::manifest(TENANT, 2).expect("key").to_key(),
        b"manifest",
    );
    let closed = block_on(read_after(&store, TENANT, 1, 0, &Codec)).expect("tail");
    assert!(closed.is_closed());
    assert_eq!(closed.entries().len(), 2);
}

#[test]
fn a_tenant_that_is_not_a_safe_component_is_refused() {
    let store = LoggedStore::new();
    let error = block_on(read_after(&store, "../x", 1, 0, &Codec)).expect_err("tenant");
    assert!(matches!(error, UnsealedError::Store(_)));
    assert_eq!(error.code(), "journal_rebuild_plan.unsealed_store_failed");
}

#[test]
fn a_store_that_fails_is_a_store_error() {
    struct Failing;
    impl JournalStore for Failing {
        fn put_new<'a>(&'a self, _: &'a str, _: Vec<u8>) -> StoreFuture<'a, ObjectMeta> {
            unreachable!("the reader never writes")
        }
        fn put_replace<'a>(
            &'a self,
            _: &'a str,
            _: Vec<u8>,
            _: &'a str,
        ) -> StoreFuture<'a, ObjectMeta> {
            unreachable!("the reader never writes")
        }
        fn get<'a>(&'a self, _: &'a str) -> StoreFuture<'a, Option<StoredObject>> {
            unreachable!("nothing is listed")
        }
        fn head<'a>(&'a self, _: &'a str) -> StoreFuture<'a, Option<ObjectMeta>> {
            unreachable!("the listing failed first")
        }
        fn list<'a>(
            &'a self,
            _: &'a str,
            _: Option<&'a str>,
            _: usize,
        ) -> StoreFuture<'a, Vec<ObjectEntry>> {
            Box::pin(async {
                Err(spot_journal_store::StoreError::Backend {
                    message: "down".to_owned(),
                })
            })
        }
        fn delete<'a>(&'a self, _: &'a str) -> StoreFuture<'a, ()> {
            unreachable!("the reader never deletes")
        }
    }
    let error = block_on(read_after(&Failing, TENANT, 1, 0, &Codec)).expect_err("store");
    assert!(matches!(error, UnsealedError::Store(_)));
}

#[test]
fn the_reader_only_lists_gets_and_heads() {
    let store = LoggedStore::new();
    put(&store, 1, 1, &chunk(1, 1, 1, 3));
    put(&store, 1, 2, &chunk(2, 1, 4, 6));
    put(&store, 1, 3, &chunk(2, 1, 7, 9));
    store.forget();
    for manifest_last in [0, 3, 6, 9] {
        let _ = block_on(read_after(&store, TENANT, 1, manifest_last, &Codec)).expect("tail");
    }
    // A failing read as well.
    put(&store, 1, 4, b"BADb");
    let _ = read(&store, 1, 0);
    let operations = store.operations();
    assert!(!operations.is_empty());
    assert!(
        operations
            .iter()
            .all(|op| op.starts_with("list ") || op.starts_with("get ") || op.starts_with("head ")),
        "{operations:?}"
    );
}
