//! A crash between the staged write and its rename must leave no object behind.
//!
//! The folder store stages bytes under `<root>/.staging/`, syncs them, and only then links the
//! staged file into its key ([`crate::folder`]). A process that dies after the sync and before
//! the link leaves a file in `.staging/` — a file that no key names. These tests reproduce that
//! state directly (write the staged file, do not land it) and then check what a reader sees.

use super::{block_on, folder_at};
use crate::folder::{FolderStore, STAGING_DIR};
use crate::layout::Layout;
use crate::store::JournalStore;

#[test]
fn a_staged_write_that_never_landed_is_invisible() {
    let dir = tempfile::tempdir().unwrap();
    let store = folder_at(&dir);
    let key = Layout::manifest("acme", 1).unwrap().to_key();

    // Exactly what a crash in `stage` leaves: a synced, complete file under the staging name,
    // and nothing at the key.
    let staged = dir
        .path()
        .join(STAGING_DIR)
        .join(format!("{}-0.part", std::process::id()));
    std::fs::write(&staged, b"the manifest that never landed").unwrap();

    assert!(
        block_on(store.get(&key)).unwrap().is_none(),
        "a staged file is not readable at the key"
    );
    assert!(
        block_on(store.head(&key)).unwrap().is_none(),
        "a staged file has no metadata at the key"
    );
    assert!(
        block_on(store.list("acme/", None, 100)).unwrap().is_empty(),
        "a staged file is not in a listing"
    );
    assert!(
        block_on(store.list("", None, 100)).unwrap().is_empty(),
        "a staged file is not in a listing of the whole store"
    );
}

#[test]
fn a_half_written_staged_file_is_invisible_too() {
    let dir = tempfile::tempdir().unwrap();
    let store = folder_at(&dir);
    let key = Layout::segment("acme", 1, 1, 10).unwrap().to_key();

    // A crash *during* the write leaves partial bytes; they are just as invisible.
    let staged = dir
        .path()
        .join(STAGING_DIR)
        .join(format!("{}-1.part", std::process::id()));
    std::fs::write(&staged, b"half a seg").unwrap();

    assert!(block_on(store.get(&key)).unwrap().is_none());
    assert!(
        block_on(store.list("acme/segments/", None, 10))
            .unwrap()
            .is_empty()
    );

    // The store still works: the leftover does not block the key it was meant for.
    block_on(store.put_new(&key, b"the whole segment".to_vec())).unwrap();
    let read = block_on(store.get(&key)).unwrap().unwrap();
    assert_eq!(read.bytes, b"the whole segment");
    assert_eq!(
        block_on(store.list("acme/segments/", None, 10))
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn a_staged_file_named_like_a_key_is_still_invisible() {
    let dir = tempfile::tempdir().unwrap();
    let store = folder_at(&dir);
    let key = Layout::tail_chunk("acme", 1, 1).unwrap().to_key();

    // A staged file whose name mirrors the key: the staging directory is not part of the key
    // space, so even this one cannot be mistaken for the object.
    let mirrored = dir
        .path()
        .join(STAGING_DIR)
        .join("00000000000000000001.chunk");
    std::fs::write(&mirrored, b"not the chunk").unwrap();

    assert!(block_on(store.get(&key)).unwrap().is_none());
    assert!(
        block_on(store.list("acme/tail/", None, 10))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_completed_write_leaves_nothing_staged() {
    let dir = tempfile::tempdir().unwrap();
    let store = folder_at(&dir);
    let key = Layout::blob("acme", &"7f".repeat(32)).unwrap().to_key();
    block_on(store.put_new(&key, b"blob bytes".to_vec())).unwrap();

    let leftovers = std::fs::read_dir(dir.path().join(STAGING_DIR))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    assert!(
        leftovers.is_empty(),
        "a landed write removes its staged file, found {leftovers:?}"
    );

    // And the object is a plain file at its key, not a link to a staging file that could vanish.
    let landed = dir.path().join(&key);
    assert!(landed.is_file());
    assert_eq!(std::fs::read(&landed).unwrap(), b"blob bytes");
    assert_eq!(store.root(), dir.path());
}

#[test]
fn a_second_store_on_the_same_root_sees_a_landed_object() {
    let dir = tempfile::tempdir().unwrap();
    let first = folder_at(&dir);
    let key = Layout::manifest("acme", 9).unwrap().to_key();
    block_on(first.put_new(&key, b"generation nine".to_vec())).unwrap();

    // A fresh handle, as a restarted process would open: the staged file of the first write is
    // gone and the object is visible.
    let second = FolderStore::new(dir.path()).unwrap();
    assert_eq!(
        block_on(second.get(&key)).unwrap().unwrap().bytes,
        b"generation nine"
    );
}
