//! [`contract::run`] against both backends: this is the suite a future S3 provider runs too.

use super::{block_on, folder_at};
use crate::contract;
use crate::folder::FolderStore;
use crate::memory::MemoryStore;
use crate::store::JournalStore;

#[test]
fn the_memory_store_holds_the_contract() {
    if let Err(rule) = block_on(contract::run(MemoryStore::new)) {
        panic!("the memory store breaks the contract: {rule}");
    }
}

#[test]
fn the_folder_store_holds_the_contract() {
    let dir = tempfile::tempdir().unwrap();
    if let Err(rule) = block_on(contract::run(|| folder_at(&dir))) {
        panic!("the folder store breaks the contract: {rule}");
    }
}

/// The standalone profile runs the relay and a recovery in two processes over one directory, so
/// two handles on one root must see each other's objects — including the refusal of a second
/// `put_new` of a key the other handle wrote.
#[test]
fn two_handles_on_one_directory_share_the_objects() {
    let dir = tempfile::tempdir().unwrap();
    let writer = folder_at(&dir);
    let reader = FolderStore::new(dir.path()).unwrap();
    let key = crate::layout::Layout::manifest("acme", 4).unwrap().to_key();

    block_on(writer.put_new(&key, b"manifest four".to_vec())).unwrap();
    let read = block_on(reader.get(&key)).unwrap().unwrap();
    assert_eq!(read.bytes, b"manifest four");
    assert!(matches!(
        block_on(reader.put_new(&key, b"another".to_vec())),
        Err(crate::error::StoreError::AlreadyExists { .. })
    ));
    let listed = block_on(reader.list("acme/manifest/", None, 10)).unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].key, key);
}
