use spot_journal_store::{JournalStore, MemoryStore};

use super::{KEY, block_on, copy, image, params};
use crate::{
    ImageRecord, SnapshotError, SnapshotReader, SnapshotWriter, list_snapshots, mark_verified,
    verify_deep,
};

fn write(
    store: &MemoryStore,
    tenant: &str,
    generation: u64,
    position: u64,
    count: u64,
    part: usize,
) {
    block_on(async {
        let mut w = SnapshotWriter::new(store, params(tenant, generation, position), &KEY)
            .and_then(|w| w.with_part_bytes(part))
            .unwrap();
        w.write_part((0..count).map(|n| image("account", n)))
            .await
            .unwrap();
        w.finish().await.unwrap();
    });
}

fn read_all(store: &MemoryStore, tenant: &str, generation: u64, position: u64) -> Vec<ImageRecord> {
    block_on(async {
        let reader = SnapshotReader::open(store, tenant, generation, position, &KEY).await?;
        let mut stream = reader.images();
        let mut out = Vec::new();
        while let Some(record) = stream.next().await? {
            out.push(record);
        }
        Ok::<_, SnapshotError>(out)
    })
    .unwrap()
}

fn code<T>(result: Result<T, SnapshotError>) -> &'static str {
    result.err().unwrap().code()
}

#[test]
fn round_trip_ten_thousand_images_over_several_parts() {
    let store = MemoryStore::new();
    write(&store, "acme", 1, 500, 10_000, 200_000);
    let images = read_all(&store, "acme", 1, 500);
    assert_eq!(images.len(), 10_000);
    assert_eq!(images[0], image("account", 0));
    assert_eq!(images[9_999], image("account", 9_999));
    let reader = block_on(SnapshotReader::open(&store, "acme", 1, 500, &KEY)).unwrap();
    let body = &reader.index().body;
    assert!(body.parts.len() > 3, "parts: {}", body.parts.len());
    assert_eq!(body.total_images, 10_000);
    assert_eq!(body.entity_counts["account"], 10_000);
    assert_eq!(body.entity_schemas["account"], vec![1, 2]);
    assert_eq!(
        block_on(verify_deep(&store, "acme", 1, 500, &KEY)).unwrap(),
        10_000
    );
}

#[test]
fn unsorted_or_duplicate_input_is_refused_before_any_write() {
    let store = MemoryStore::new();
    block_on(async {
        let mut w = SnapshotWriter::new(&store, params("acme", 1, 5), &KEY).unwrap();
        let bad = vec![image("a", 2), image("a", 1)];
        assert_eq!(
            w.write_part(bad).await.unwrap_err().code(),
            "journal_snapshot.input_not_sorted"
        );
        let dup = vec![image("a", 1), image("a", 1)];
        assert_eq!(
            w.write_part(dup).await.unwrap_err().code(),
            "journal_snapshot.input_not_sorted"
        );
        // Order is also enforced across batches.
        w.write_part(vec![image("b", 5)]).await.unwrap();
        let back = w.write_part(vec![image("a", 9)]).await.unwrap_err();
        assert_eq!(back, SnapshotError::InputNotSorted { ordinal: 1 });
    });
    assert!(block_on(store.list("", None, 10)).unwrap().is_empty());
}

#[test]
fn empty_entity_or_id_is_refused() {
    let store = MemoryStore::new();
    let mut bad = image("a", 1);
    bad.id.clear();
    block_on(async {
        let mut w = SnapshotWriter::new(&store, params("acme", 1, 5), &KEY).unwrap();
        assert_eq!(
            w.write_part(vec![bad]).await.unwrap_err().code(),
            "journal_snapshot.image_invalid"
        );
    });
}

#[test]
fn a_crash_before_finish_leaves_a_snapshot_nothing_lists() {
    let store = MemoryStore::new();
    block_on(async {
        let mut w = SnapshotWriter::new(&store, params("acme", 1, 9), &KEY)
            .and_then(|w| w.with_part_bytes(100))
            .unwrap();
        w.write_part((0..50).map(|n| image("a", n))).await.unwrap();
        drop(w);
    });
    assert!(
        !block_on(store.list("acme/snapshots/", None, 100))
            .unwrap()
            .is_empty()
    );
    assert!(
        block_on(list_snapshots(&store, "acme", &KEY))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        code(block_on(SnapshotReader::open(&store, "acme", 1, 9, &KEY))),
        "journal_snapshot.index_missing"
    );
}

#[test]
fn a_rerun_after_a_crash_converges_and_a_conflict_is_refused() {
    let store = MemoryStore::new();
    block_on(async {
        let mut w = SnapshotWriter::new(&store, params("acme", 1, 9), &KEY)
            .and_then(|w| w.with_part_bytes(100))
            .unwrap();
        w.write_part((0..50).map(|n| image("account", n)))
            .await
            .unwrap();
        drop(w);
    });
    write(&store, "acme", 1, 9, 50, 100);
    assert_eq!(read_all(&store, "acme", 1, 9).len(), 50);
    // Writing different content to the same snapshot is refused.
    let result = block_on(async {
        let mut w = SnapshotWriter::new(&store, params("acme", 1, 9), &KEY)?;
        w.write_part((0..3).map(|n| image("z", n))).await?;
        w.finish().await
    });
    assert_eq!(code(result), "journal_snapshot.exists");
}

#[test]
fn a_flipped_byte_in_a_part_names_the_part() {
    let store = MemoryStore::new();
    write(&store, "acme", 1, 7, 300, 2_000);
    let target = "acme/snapshots/00000000000000000001/00000000000000000007/part-00000002.jsonl.zst";
    let flipped = copy(&store, |key, mut bytes| {
        if key == target {
            bytes[3] ^= 0x01;
        }
        Some(bytes)
    });
    let err = block_on(verify_deep(&flipped, "acme", 1, 7, &KEY)).unwrap_err();
    assert_eq!(
        err,
        SnapshotError::PartInvalid {
            part: "part-00000002.jsonl.zst".into(),
            reason: "the hash differs from the index"
        }
    );
    let missing = copy(&store, |key, bytes| (key != target).then_some(bytes));
    let err = block_on(verify_deep(&missing, "acme", 1, 7, &KEY)).unwrap_err();
    assert_eq!(err.code(), "journal_snapshot.part_invalid");
    assert!(err.to_string().contains("part-00000002"));
}

#[test]
fn an_index_from_another_tenant_or_position_is_refused() {
    let store = MemoryStore::new();
    write(&store, "acme", 1, 7, 10, 1_000);
    let index = "acme/snapshots/00000000000000000001/00000000000000000007/index.json";
    let bytes = block_on(store.get(index)).unwrap().unwrap().bytes;
    let moved = MemoryStore::new();
    for (key, prefix) in [
        (
            "other/snapshots/00000000000000000001/00000000000000000007/index.json",
            "other",
        ),
        (
            "acme/snapshots/00000000000000000001/00000000000000000008/index.json",
            "acme",
        ),
    ] {
        block_on(moved.put_new(key, bytes.clone())).unwrap();
        let position = if prefix == "acme" { 8 } else { 7 };
        assert_eq!(
            code(block_on(SnapshotReader::open(
                &moved, prefix, 1, position, &KEY
            ))),
            "journal_snapshot.index_misplaced"
        );
    }
    // Listing skips both.
    assert!(
        block_on(list_snapshots(&moved, "other", &KEY))
            .unwrap()
            .is_empty()
    );
    assert!(
        block_on(list_snapshots(&moved, "acme", &KEY))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_tampered_index_or_wrong_key_fails_the_signature() {
    let store = MemoryStore::new();
    write(&store, "acme", 1, 7, 10, 1_000);
    let tampered = copy(&store, |key, bytes| {
        if key.ends_with("index.json") {
            let text = String::from_utf8(bytes)
                .unwrap()
                .replace("\"total_images\":10", "\"total_images\":11");
            return Some(text.into_bytes());
        }
        Some(bytes)
    });
    assert_eq!(
        code(block_on(SnapshotReader::open(
            &tampered, "acme", 1, 7, &KEY
        ))),
        "journal_snapshot.signature_invalid"
    );
    let other = super::Keyed {
        id: "k1",
        secret: "different",
    };
    assert_eq!(
        code(block_on(SnapshotReader::open(&store, "acme", 1, 7, &other))),
        "journal_snapshot.signature_invalid"
    );
}

#[test]
fn two_snapshots_are_listed_in_position_order() {
    let store = MemoryStore::new();
    write(&store, "acme", 1, 900, 5, 1_000);
    write(&store, "acme", 1, 100, 5, 1_000);
    write(&store, "acme", 2, 100, 5, 1_000);
    write(&store, "other", 1, 50, 5, 1_000);
    let listed = block_on(list_snapshots(&store, "acme", &KEY)).unwrap();
    let order: Vec<(u64, u64)> = listed.iter().map(|s| (s.position, s.generation)).collect();
    assert_eq!(order, vec![(100, 1), (100, 2), (900, 1)]);
    assert!(
        listed
            .iter()
            .all(|s| !s.verified && s.taken_at_millis == 1_700_000_000_000)
    );
}

#[test]
fn the_verified_flag_is_set_by_a_signed_marker_bound_to_the_index() {
    let store = MemoryStore::new();
    write(&store, "acme", 1, 100, 5, 1_000);
    write(&store, "acme", 1, 200, 5, 1_000);
    block_on(mark_verified(&store, "acme", 1, 100, &KEY, &KEY)).unwrap();
    block_on(mark_verified(&store, "acme", 1, 100, &KEY, &KEY)).unwrap();
    let listed = block_on(list_snapshots(&store, "acme", &KEY)).unwrap();
    assert_eq!((listed[0].verified, listed[1].verified), (true, false));
    // A marker copied beside another snapshot does not verify it.
    let marker = "acme/snapshots/00000000000000000001/00000000000000000100/verified";
    let bytes = block_on(store.get(marker)).unwrap().unwrap().bytes;
    block_on(store.put_new(
        "acme/snapshots/00000000000000000001/00000000000000000200/verified",
        bytes,
    ))
    .unwrap();
    let listed = block_on(list_snapshots(&store, "acme", &KEY)).unwrap();
    assert!(!listed[1].verified);
    // Marking a snapshot with no index fails.
    assert_eq!(
        code(block_on(mark_verified(&store, "acme", 1, 300, &KEY, &KEY))),
        "journal_snapshot.index_missing"
    );
}

#[test]
fn an_expected_count_must_match() {
    let store = MemoryStore::new();
    let result = block_on(async {
        let mut w = SnapshotWriter::new(&store, params("acme", 1, 5), &KEY)?.with_expected_count(4);
        w.write_part((0..3).map(|n| image("a", n))).await?;
        w.finish().await
    });
    assert_eq!(
        result.unwrap_err(),
        SnapshotError::CountMismatch {
            expected: 4,
            actual: 3
        }
    );
    assert!(
        block_on(list_snapshots(&store, "acme", &KEY))
            .unwrap()
            .is_empty()
    );
    let ok = block_on(async {
        let mut w = SnapshotWriter::new(&store, params("acme", 1, 5), &KEY)?.with_expected_count(3);
        w.write_part((0..3).map(|n| image("a", n))).await?;
        w.finish().await
    });
    assert!(ok.is_ok());
}

#[test]
fn identical_input_gives_byte_identical_objects() {
    let first = MemoryStore::new();
    let second = MemoryStore::new();
    write(&first, "acme", 1, 40, 700, 5_000);
    // The second node writes in different batch sizes.
    block_on(async {
        let mut w = SnapshotWriter::new(&second, params("acme", 1, 40), &KEY)
            .and_then(|w| w.with_part_bytes(5_000))
            .unwrap();
        for chunk in (0..700_u64).collect::<Vec<_>>().chunks(37) {
            w.write_part(chunk.iter().map(|n| image("account", *n)))
                .await
                .unwrap();
        }
        w.finish().await.unwrap();
    });
    let keys = |s: &MemoryStore| block_on(s.list("", None, 10_000)).unwrap();
    assert_eq!(keys(&first), keys(&second));
    for entry in keys(&first) {
        let a = block_on(first.get(&entry.key)).unwrap().unwrap().bytes;
        let b = block_on(second.get(&entry.key)).unwrap().unwrap().bytes;
        assert_eq!(a, b, "{}", entry.key);
    }
}

#[test]
fn bad_parameters_are_refused() {
    let store = MemoryStore::new();
    let mut p = params("acme", 1, 1);
    p.manifest_hash = "xyz".into();
    assert_eq!(
        code(SnapshotWriter::new(&store, p, &KEY)),
        "journal_snapshot.params_invalid"
    );
    assert_eq!(
        code(SnapshotWriter::new(&store, params("a/b", 1, 1), &KEY)),
        "journal_snapshot.params_invalid"
    );
    let w = SnapshotWriter::new(&store, params("acme", 1, 1), &KEY).unwrap();
    assert_eq!(
        code(w.with_part_bytes(0)),
        "journal_snapshot.params_invalid"
    );
}

#[test]
fn an_empty_snapshot_is_valid() {
    let store = MemoryStore::new();
    write(&store, "acme", 1, 1, 0, 1_000);
    assert!(read_all(&store, "acme", 1, 1).is_empty());
    assert_eq!(
        block_on(list_snapshots(&store, "acme", &KEY))
            .unwrap()
            .len(),
        1
    );
}
