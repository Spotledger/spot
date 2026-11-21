#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod common;
use common::*;
use spot_registry::*;
use std::sync::Arc;
fn publisher(id: u8) -> Publisher {
    Publisher {
        id: format!("publisher{id}"),
        display_name: format!("Publisher {id}"),
        certificate_chain_root: root().public().key_id(),
        status: PublisherStatus::Active,
    }
}
fn contract(index: &dyn IndexStore) {
    let source = Setup::new();
    let r1 = source.publish("demo", "1.0.0", "", 12);
    let r2 = source.publish("demo", "2.0.0", "", 12);
    assert!(
        index
            .releases(&r1.app, &Visibility::default(), None)
            .unwrap()
            .is_empty()
    );
    assert!(index.latest(&r1.app, false).unwrap().is_none());
    assert!(index.by_digest(r1.digest).unwrap().is_none());
    index.preflight(&r1).unwrap();
    assert!(index.ownership(&r1.app).unwrap().is_none());
    assert!(index.commit(r1.clone(), publisher(12)).unwrap().inserted);
    assert!(!index.commit(r1.clone(), publisher(12)).unwrap().inserted);
    assert_eq!(
        index.ownership(&r1.app).unwrap().unwrap().publisher,
        "publisher12"
    );
    assert_eq!(index.by_digest(r1.digest).unwrap(), Some(r1.clone()));
    index.commit(r2.clone(), publisher(12)).unwrap();
    assert_eq!(
        index
            .releases(&r1.app, &Visibility::default(), None)
            .unwrap(),
        vec![r2.clone(), r1.clone()]
    );
    assert_eq!(index.latest(&r1.app, false).unwrap(), Some(r2.clone()));
    let mut conflict = r1.clone();
    conflict.digest = r2.digest;
    assert_code(
        index.commit(conflict, publisher(12)),
        "registry.version_conflict",
    );
    let mut wrong = r2.clone();
    wrong.publisher = "publisher13".into();
    wrong.version = version("0.1.0");
    assert_code(index.preflight(&wrong), "registry.not_owner");
    assert_code(index.commit(wrong, publisher(13)), "registry.not_owner");
    assert_code(
        index.remove(
            &r1.app,
            &r1.version,
            "publisher13",
            Removal {
                at: NOW,
                reason: "unauthorized".into(),
            },
            false,
        ),
        "registry.not_owner",
    );
    let marked = index
        .remove(
            &r2.app,
            &r2.version,
            "publisher12",
            Removal {
                at: NOW,
                reason: "yank".into(),
            },
            false,
        )
        .unwrap()
        .unwrap();
    assert!(!marked.active());
    assert_eq!(index.latest(&r1.app, false).unwrap(), Some(r1.clone()));
    assert!(
        index
            .by_digest(r2.digest)
            .unwrap()
            .unwrap()
            .yanked
            .is_some()
    );
    let mut downgrade = r2.clone();
    downgrade.version = version("1.5.0");
    assert_code(
        index.preflight(&downgrade),
        "registry.version_not_increasing",
    );
    assert_code(
        index.releases(
            &r1.app,
            &Visibility {
                include_inactive: true,
                ..Default::default()
            },
            None,
        ),
        "registry.unauthorized",
    );
    index
        .describe(&r1.app, "publisher12", "catalogue".into(), "tools".into())
        .unwrap();
    assert_eq!(
        index
            .search(&SearchQuery {
                text: "catalogue".into(),
                ..Default::default()
            })
            .unwrap()
            .releases
            .len(),
        1
    );
    index
        .set_publisher_status("publisher12", PublisherStatus::Suspended)
        .unwrap();
    assert_code(index.preflight(&r1), "registry.publisher_inactive");
    assert_eq!(index.digests().unwrap().len(), 2);
}
#[test]
fn in_memory_index_contract() {
    contract(&MemoryIndexStore::default());
}
#[test]
fn concurrent_index_commits_enforce_owner_and_single_insert() {
    let index = Arc::new(MemoryIndexStore::default());
    let s = Setup::new();
    let r = s.publish("demo", "1.0.0", "", 12);
    let barrier = Arc::new(std::sync::Barrier::new(17));
    let threads: Vec<_> = (0..16)
        .map(|_| {
            let index = index.clone();
            let r = r.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                index.commit(r, publisher(12)).unwrap().inserted
            })
        })
        .collect();
    barrier.wait();
    assert_eq!(
        threads
            .into_iter()
            .map(|t| t.join().unwrap())
            .filter(|b| *b)
            .count(),
        1
    );
    assert_eq!(index.digests().unwrap(), vec![r.digest]);
}
#[test]
fn shared_index_registries_first_claim_and_audit_are_atomic() {
    let s = Setup::new();
    let barrier = Arc::new(std::sync::Barrier::new(9));
    let threads: Vec<_> = (12..20)
        .map(|id| {
            let registry = Registry::new(
                s.index.clone(),
                s.blobs.clone(),
                RegistryPolicy::default(),
                s.trust.clone(),
                s.audit.clone(),
            );
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let b = bytes("demo", "1.0.0", "");
                let p = proof(&b, id);
                barrier.wait();
                run(registry.publish(b, p.signature, p.chain, NOW))
            })
        })
        .collect();
    barrier.wait();
    let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert!(
        results
            .iter()
            .filter_map(|r| r.as_ref().err())
            .all(|e| e.code == "registry.not_owner")
    );
    assert_eq!(s.index.digests().unwrap().len(), 1);
    assert_eq!(s.audit.records().unwrap().len(), 1);
}
#[test]
fn concurrent_versions_never_allow_downgrade_or_conflicting_identity() {
    let s = Setup::new();
    s.publish("demo", "1.0.0", "", 12);
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let threads: Vec<_> = ["2.0.0", "3.0.0"]
        .into_iter()
        .map(|v| {
            let registry = s.registry.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let b = bytes("demo", v, "");
                let p = proof(&b, 12);
                barrier.wait();
                run(registry.publish(b, p.signature, p.chain, NOW))
            })
        })
        .collect();
    barrier.wait();
    let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert!(results[1].is_ok());
    assert!(
        results[0].is_ok()
            || results[0].as_ref().unwrap_err().code == "registry.version_not_increasing"
    );
    assert_eq!(
        s.registry
            .latest(&app("demo"), false)
            .unwrap()
            .unwrap()
            .version,
        version("3.0.0")
    );
    let rows = s
        .index
        .releases(&app("demo"), &Visibility::default(), None)
        .unwrap();
    assert_eq!(s.audit.records().unwrap().len(), rows.len());
}
