//! [`MemoryFileStore`] against the contract suite, on a current-thread runtime — the
//! suite itself must never need runtime threads — plus clone-state semantics.

use spot_files::contract_tests;
use spot_files::key::{FileKey, FileKeyPrefix, Visibility};
use spot_files::memory::MemoryFileStore;
use spot_files::store::{FileStore, PutOptions, body_from_str};

#[tokio::test]
async fn contract_suite() {
    contract_tests::run_all(&MemoryFileStore::new())
        .await
        .assert_all_passed();
}

#[test]
fn clones_share_state_and_separate_instances_do_not() {
    let runtime = match tokio::runtime::Builder::new_current_thread().build() {
        Ok(runtime) => runtime,
        Err(e) => panic!("building a runtime: {e}"),
    };
    runtime.block_on(async {
        let store = MemoryFileStore::new();
        let site = match spot_core::SiteId::new("memory-clones.test") {
            Ok(site) => site,
            Err(e) => panic!("site id: {e}"),
        };
        let key = match FileKey::new(Visibility::Private, "2026/clones/x.bin") {
            Ok(key) => key,
            Err(e) => panic!("key: {e}"),
        };
        let twin = store.clone();
        match twin
            .put(
                &site,
                key.clone(),
                body_from_str("shared through the clone"),
                PutOptions::new("text/plain", 4096, true),
            )
            .await
        {
            Ok(stored) => assert_eq!(stored.size, 24, "the body's bytes are what is stored"),
            Err(e) => panic!("put through the clone: {e}"),
        }
        // The put done through the clone is visible through the original.
        match store.head(&site, key).await {
            Ok(Some(meta)) => assert_eq!(meta.content_type, "text/plain"),
            Ok(None) => panic!("the clone's put vanished"),
            Err(e) => panic!("head on the original: {e}"),
        }
        // A separate instance stays empty.
        let other = MemoryFileStore::default();
        match other.list(&site, &FileKeyPrefix::all(), None, 10).await {
            Ok(listed) => assert!(listed.is_empty(), "instances must not share state"),
            Err(e) => panic!("list on a separate store: {e}"),
        }
    });
}
