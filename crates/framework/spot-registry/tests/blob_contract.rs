#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod common;
use common::*;
use spot_registry::*;
use std::sync::Arc;
fn contract(store: &dyn BlobStore) {
    let b = bytes("demo", "1.0.0", "");
    let d = open(&b).digest();
    assert!(!run(store.exists(d)).unwrap());
    assert_code(run(store.get(d)), "registry.not_found");
    assert_code(run(store.size(d)), "registry.not_found");
    let stream: BytesStream = Box::new(Chunks::new(b.chunks(7).map(<[u8]>::to_vec)));
    run(store.put_if_absent(d, stream)).unwrap();
    assert!(run(store.exists(d)).unwrap());
    assert_eq!(run(store.size(d)).unwrap(), b.len() as u64);
    assert_eq!(
        run(collect_stream(run(store.get(d)).unwrap(), 10000)).unwrap(),
        b
    );
    run(store.put_if_absent(d, bytes_stream(b.clone()))).unwrap();
    assert_eq!(run(store.inventory()).unwrap(), vec![d]);
    let other = bytes("demo", "2.0.0", "");
    assert_code(
        run(store.put_if_absent(d, bytes_stream(other.clone()))),
        "registry.digest_mismatch",
    );
    assert_code(
        run(store.put_if_absent(d, bytes_stream(vec![0; 20]))),
        "registry.digest_mismatch",
    );
    assert_eq!(
        run(collect_stream(run(store.get(d)).unwrap(), 10000)).unwrap(),
        b
    );
    let other_digest = open(&other).digest();
    run(store.put_if_absent(other_digest, bytes_stream(other))).unwrap();
    let mut expected = vec![d, other_digest];
    expected.sort_by_key(Digest::to_hex);
    assert_eq!(run(store.inventory()).unwrap(), expected);
}
#[test]
fn memory_blob_contract() {
    contract(&MemoryBlobStore::default());
}
#[test]
fn filesystem_blob_contract() {
    let dir = tempfile::tempdir().unwrap();
    contract(&FsBlobStore::new(dir.path(), validation()).unwrap());
}
#[test]
fn tampered_file_and_excessive_read_are_refused_by_all_read_operations() {
    let dir = tempfile::tempdir().unwrap();
    let store = FsBlobStore::new(dir.path(), validation()).unwrap();
    let b = bytes("demo", "1.0.0", "");
    let d = open(&b).digest();
    run(store.put_if_absent(d, bytes_stream(b.clone()))).unwrap();
    std::fs::write(dir.path().join(d.to_hex()), bytes("other", "1.0.0", "")).unwrap();
    assert_code(run(store.get(d)), "registry.digest_mismatch");
    assert_code(run(store.exists(d)), "registry.digest_mismatch");
    assert_code(run(store.size(d)), "registry.digest_mismatch");
    assert_code(run(store.inventory()), "registry.digest_mismatch");
    assert_code(
        run(store.put_if_absent(d, bytes_stream(b))),
        "registry.digest_mismatch",
    );
}
#[test]
fn concurrent_same_digest_safe_across_filesystem_instances_and_readers() {
    let dir = tempfile::tempdir().unwrap();
    let b = bytes("demo", "1.0.0", "");
    let d = open(&b).digest();
    let barrier = Arc::new(std::sync::Barrier::new(17));
    let threads: Vec<_> = (0..16)
        .map(|_| {
            let store = FsBlobStore::new(dir.path(), validation()).unwrap();
            let b = b.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                run(store.put_if_absent(d, bytes_stream(b.clone()))).unwrap();
                assert_eq!(
                    run(collect_stream(run(store.get(d)).unwrap(), 10000)).unwrap(),
                    b
                );
            })
        })
        .collect();
    barrier.wait();
    for t in threads {
        t.join().unwrap();
    }
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    assert_eq!(std::fs::read(dir.path().join(d.to_hex())).unwrap(), b);
}
#[test]
fn digest_parser_fuzz_never_creates_traversal_paths() {
    let dangerous = [
        "../",
        "..\\",
        "/tmp/",
        "C:\\",
        "\0",
        "é",
        "%2e%2e%2f",
        ".",
        "g",
    ];
    for input in dangerous {
        for length in 0..130 {
            assert!(input.repeat(length).parse::<Digest>().is_err());
        }
    }
    for byte in 0u8..=255 {
        let text = char::from(byte).to_string().repeat(64);
        if let Ok(digest) = text.parse::<Digest>() {
            let name = digest.to_hex();
            assert_eq!(name.len(), 64);
            assert!(name.bytes().all(|b| b.is_ascii_hexdigit()));
            assert!(!name.contains('/'));
        }
    }
    assert_eq!(
        "A".repeat(64).parse::<Digest>().unwrap().to_hex(),
        "a".repeat(64)
    );
}
#[cfg(unix)]
#[test]
fn filesystem_symlinks_directory_entries_and_noncanonical_names_refused() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let b = bytes("demo", "1.0.0", "");
    let d = open(&b).digest();
    let store = FsBlobStore::new(dir.path(), validation()).unwrap();
    let external = outside.path().join("external");
    std::fs::write(&external, &b).unwrap();
    symlink(&external, dir.path().join(d.to_hex())).unwrap();
    assert_code(run(store.get(d)), "registry.unsafe_path");
    assert_code(
        run(store.put_if_absent(d, bytes_stream(b.clone()))),
        "registry.unsafe_path",
    );
    assert_eq!(std::fs::read(&external).unwrap(), b);
    std::fs::remove_file(dir.path().join(d.to_hex())).unwrap();
    std::fs::create_dir(dir.path().join(d.to_hex())).unwrap();
    assert_code(run(store.get(d)), "registry.unsafe_path");
    std::fs::remove_dir(dir.path().join(d.to_hex())).unwrap();
    std::fs::write(dir.path().join("BAD-NAME"), b).unwrap();
    assert_code(run(store.inventory()), "registry.unsafe_path");
    let link = dir.path().join("root-link");
    symlink(outside.path(), &link).unwrap();
    assert_code(FsBlobStore::new(link, validation()), "registry.unsafe_path");
}
struct BrokenStream {
    first: bool,
}
impl ByteStream for BrokenStream {
    fn next(&mut self) -> BoxFuture<'_, Option<Vec<u8>>> {
        Box::pin(async move {
            if self.first {
                self.first = false;
                Ok(Some(vec![0; 10]))
            } else {
                Err(RegistryError::new("registry.storage", "stream interrupted"))
            }
        })
    }
}
#[test]
fn stream_error_and_size_limit_do_not_store_partial_object() {
    for fs in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let v = BlobValidation {
            limits: spot_bundle::Limits {
                max_compressed_bytes: 10,
                ..Default::default()
            },
            ..validation()
        };
        let store: Box<dyn BlobStore> = if fs {
            Box::new(FsBlobStore::new(dir.path(), v).unwrap())
        } else {
            Box::new(MemoryBlobStore::new(v))
        };
        let d = open(&bytes("demo", "1.0.0", "")).digest();
        assert_code(
            run(store.put_if_absent(d, Box::new(BrokenStream { first: true }))),
            "registry.storage",
        );
        assert_code(
            run(store.put_if_absent(d, bytes_stream(vec![0; 11]))),
            "registry.too_large",
        );
        assert!(run(store.inventory()).unwrap().is_empty());
    }
}

#[test]
fn canonical_repacking_preserves_first_stored_archive_and_size() {
    let b = bytes("demo", "1.0.0", "");
    let d = open(&b).digest();
    let mut repacked = b.clone();
    // ZIP timestamps are not authenticated canonical bundle content.
    for i in 0..repacked.len().saturating_sub(16) {
        if repacked[i..i + 4] == [0x50, 0x4b, 0x03, 0x04] {
            repacked[i + 10] ^= 1;
        }
        if repacked[i..i + 4] == [0x50, 0x4b, 0x01, 0x02] {
            repacked[i + 12] ^= 1;
        }
    }
    assert_ne!(repacked, b);
    assert_eq!(open(&repacked).digest(), d);
    let dir = tempfile::tempdir().unwrap();
    let stores: Vec<Box<dyn BlobStore>> = vec![
        Box::new(MemoryBlobStore::default()),
        Box::new(FsBlobStore::new(dir.path(), validation()).unwrap()),
    ];
    for store in stores {
        run(store.put_if_absent(d, bytes_stream(b.clone()))).unwrap();
        run(store.put_if_absent(d, bytes_stream(repacked.clone()))).unwrap();
        assert_eq!(
            run(collect_stream(run(store.get(d)).unwrap(), 10000)).unwrap(),
            b
        );
        assert_eq!(run(store.size(d)).unwrap(), b.len() as u64);
    }
}

#[test]
fn varied_digest_strings_fuzz_validated_address_alphabet() {
    let mut state = 0x93cb_615e_829a_d071u64;
    for length in 0..128 {
        for _ in 0..40 {
            let text: String = (0..length)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    char::from((state & 255) as u8)
                })
                .collect();
            if let Ok(d) = text.parse::<Digest>() {
                assert_eq!(d.to_hex().len(), 64);
                assert!(
                    d.to_hex()
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                );
            }
        }
    }
}
