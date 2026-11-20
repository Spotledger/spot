//! `delete_snapshot`: the narrow deletion that lets snapshot retention be applied.

use super::{block_on, folder_at};
use crate::error::StoreError;
use crate::layout::Layout;
use crate::memory::MemoryStore;
use crate::store::JournalStore;

const TENANT: &str = "acme";

fn seeded<S: JournalStore>(store: &S) -> (String, String, String) {
    let image = Layout::snapshot_object(TENANT, 2, 10, "image")
        .unwrap()
        .to_key();
    let manifest = Layout::manifest(TENANT, 2).unwrap().to_key();
    let segment = Layout::segment(TENANT, 2, 1, 5).unwrap().to_key();
    block_on(store.put_new(&image, b"image".to_vec())).unwrap();
    block_on(store.put_new(&manifest, b"manifest".to_vec())).unwrap();
    block_on(store.put_new(&segment, b"segment".to_vec())).unwrap();
    (image, manifest, segment)
}

fn rules<S: JournalStore>(store: &S) {
    let (image, manifest, segment) = seeded(store);

    // `delete` stays as strict as ever.
    assert!(matches!(
        block_on(store.delete(&image)),
        Err(StoreError::DeleteRefused { .. })
    ));
    assert!(block_on(store.head(&image)).unwrap().is_some());

    // Segments, manifests and tail chunks are never removable here.
    for key in [&segment, &manifest] {
        assert!(matches!(
            block_on(store.delete_snapshot(key)),
            Err(StoreError::DeleteRefused { .. })
        ));
        assert!(block_on(store.head(key)).unwrap().is_some());
    }
    let tail = Layout::tail_chunk(TENANT, 2, 1).unwrap().to_key();
    block_on(store.put_new(&tail, b"tail".to_vec())).unwrap();
    assert!(matches!(
        block_on(store.delete_snapshot(&tail)),
        Err(StoreError::DeleteRefused { .. })
    ));

    // The bare prefix, traversal and look-alike keys are refused, and nothing is removed.
    let prefix = Layout::snapshot(TENANT, 2, 10).unwrap().to_key();
    assert!(matches!(
        block_on(store.delete_snapshot(&prefix)),
        Err(StoreError::DeleteRefused { .. })
    ));
    for bad in [
        "acme/snapshots/../segments/x",
        "acme/snapshotsX/00000000000000000002/00000000000000000010/image",
        "acme/snapshots/",
        "../acme/snapshots/00000000000000000002/00000000000000000010/image",
        "",
    ] {
        assert!(
            block_on(store.delete_snapshot(bad)).is_err(),
            "{bad:?} must be refused"
        );
    }
    assert!(block_on(store.head(&image)).unwrap().is_some());

    // A named snapshot object is removed, once.
    block_on(store.delete_snapshot(&image)).unwrap();
    assert!(block_on(store.head(&image)).unwrap().is_none());
    let listed = block_on(store.list("acme/snapshots/", None, 10)).unwrap();
    assert!(listed.iter().all(|entry| entry.key != image));
    assert!(matches!(
        block_on(store.delete_snapshot(&image)),
        Err(StoreError::NotFound { .. })
    ));

    // The journal's sealed record is untouched.
    assert!(block_on(store.head(&segment)).unwrap().is_some());
    assert!(block_on(store.head(&manifest)).unwrap().is_some());
}

#[test]
fn the_memory_store_applies_the_snapshot_deletion_rules() {
    rules(&MemoryStore::new());
}

#[test]
fn the_folder_store_applies_the_snapshot_deletion_rules() {
    let dir = tempfile::tempdir().unwrap();
    rules(&folder_at(&dir));
}

/// A store written before the method existed keeps compiling and keeps every snapshot.
#[test]
fn the_default_body_refuses() {
    struct Bare(MemoryStore);
    impl JournalStore for Bare {
        fn put_new<'a>(
            &'a self,
            key: &'a str,
            bytes: Vec<u8>,
        ) -> crate::store::StoreFuture<'a, crate::store::ObjectMeta> {
            self.0.put_new(key, bytes)
        }
        fn put_replace<'a>(
            &'a self,
            key: &'a str,
            bytes: Vec<u8>,
            expected_etag: &'a str,
        ) -> crate::store::StoreFuture<'a, crate::store::ObjectMeta> {
            self.0.put_replace(key, bytes, expected_etag)
        }
        fn get<'a>(
            &'a self,
            key: &'a str,
        ) -> crate::store::StoreFuture<'a, Option<crate::store::StoredObject>> {
            self.0.get(key)
        }
        fn head<'a>(
            &'a self,
            key: &'a str,
        ) -> crate::store::StoreFuture<'a, Option<crate::store::ObjectMeta>> {
            self.0.head(key)
        }
        fn list<'a>(
            &'a self,
            prefix: &'a str,
            after: Option<&'a str>,
            limit: usize,
        ) -> crate::store::StoreFuture<'a, Vec<crate::store::ObjectEntry>> {
            self.0.list(prefix, after, limit)
        }
        fn delete<'a>(&'a self, key: &'a str) -> crate::store::StoreFuture<'a, ()> {
            self.0.delete(key)
        }
    }
    let store = Bare(MemoryStore::new());
    let (image, _, _) = seeded(&store);
    assert!(matches!(
        block_on(store.delete_snapshot(&image)),
        Err(StoreError::DeleteRefused { .. })
    ));
    assert!(block_on(store.head(&image)).unwrap().is_some());
}
