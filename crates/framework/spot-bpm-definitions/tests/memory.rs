//! The reference store passes the conformance suite, `load_into` rebuilds a registry and reports
//! what no longer compiles, and every code has a catalogue entry.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_app_api::prelude::OffsetDateTime;
use spot_bpm_definitions::conformance::{run, sample_text};
use spot_bpm_definitions::{
    DefinitionError, DefinitionStore, LoadFailureKind, MemoryDefinitionStore, codes, load_into,
};
use spot_bpm_engine::ProcessRegistry;
use spot_bpm_model::VersionStatus;

fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
}

#[tokio::test]
async fn the_memory_store_passes_the_conformance_suite() {
    run(&MemoryDefinitionStore::new(), now()).await;
}

#[tokio::test]
async fn running_pins_are_what_the_host_recorded() {
    let store = MemoryDefinitionStore::new();
    assert!(store.running_pins("order").await.unwrap().is_empty());
    store.set_running_pins("order", [3, 1, 3]);
    assert_eq!(store.running_pins("order").await.unwrap(), vec![1, 3]);
}

#[tokio::test]
async fn load_into_registers_every_published_version_with_the_stored_hash() {
    let store = MemoryDefinitionStore::new();
    for (n, marker) in ["Sales Order", "Purchase Order"].iter().enumerate() {
        let expected = u32::try_from(n).ok().filter(|n| *n > 0);
        store
            .publish(
                "order",
                &sample_text("order", marker),
                expected,
                "alice",
                None,
                now(),
            )
            .await
            .unwrap();
    }
    store
        .publish(
            "invoice",
            &sample_text("invoice", "Sales Invoice"),
            None,
            "alice",
            None,
            now(),
        )
        .await
        .unwrap();
    store.retire("order", 1, "alice", now()).await.unwrap();

    let registry = ProcessRegistry::new();
    let report = load_into(&registry, &store).await.unwrap();
    assert!(report.is_clean(), "{report:?}");
    assert_eq!(
        report.loaded,
        vec![("invoice".to_owned(), 1), ("order".to_owned(), 2)]
    );
    assert!(
        registry.get("order", 1).is_none(),
        "a retired version nobody pins is not loaded"
    );
    let loaded = registry.get("order", 2).unwrap();
    let stored = store.get("order", 2).await.unwrap().unwrap();
    assert_eq!(loaded.compiled.hash.as_hex(), stored.content_hash);
    assert_eq!(loaded.number(), 2);
    assert_eq!(registry.latest("order").unwrap().number(), 2);
}

/// A store that hands back rows as a test shaped them, to stand for rows written by an older
/// build whose texts no longer compile.
struct Shaped(Vec<spot_bpm_definitions::StoredVersion>, Pins);

/// What the shaped store answers to `running_pins`.
enum Pins {
    None,
    Some(Vec<u32>),
    Fails,
}

impl Shaped {
    fn new(rows: Vec<spot_bpm_definitions::StoredVersion>) -> Self {
        Self(rows, Pins::None)
    }
}

impl DefinitionStore for Shaped {
    fn publish<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: Option<u32>,
        _: &'a str,
        _: Option<&'a str>,
        _: OffsetDateTime,
    ) -> spot_bpm_definitions::DefFuture<'a, spot_bpm_definitions::StoredVersion> {
        Box::pin(async { unreachable!() })
    }
    fn get<'a>(
        &'a self,
        _: &'a str,
        _: u32,
    ) -> spot_bpm_definitions::DefFuture<'a, Option<spot_bpm_definitions::StoredVersion>> {
        Box::pin(async { unreachable!() })
    }
    fn latest_published<'a>(
        &'a self,
        _: &'a str,
    ) -> spot_bpm_definitions::DefFuture<'a, Option<spot_bpm_definitions::StoredVersion>> {
        Box::pin(async { unreachable!() })
    }
    fn list<'a>(
        &'a self,
        name: &'a str,
        after: Option<u32>,
        _: usize,
    ) -> spot_bpm_definitions::DefFuture<'a, Vec<spot_bpm_definitions::StoredVersion>> {
        Box::pin(async move {
            Ok(self
                .0
                .iter()
                .filter(|v| v.name == name && v.version > after.unwrap_or(0))
                .cloned()
                .collect())
        })
    }
    fn names<'a>(&'a self) -> spot_bpm_definitions::DefFuture<'a, Vec<String>> {
        Box::pin(async { Ok(vec!["order".to_owned()]) })
    }
    fn retire<'a>(
        &'a self,
        _: &'a str,
        _: u32,
        _: &'a str,
        _: OffsetDateTime,
    ) -> spot_bpm_definitions::DefFuture<'a, spot_bpm_definitions::StoredVersion> {
        Box::pin(async { unreachable!() })
    }
    fn running_pins<'a>(&'a self, _: &'a str) -> spot_bpm_definitions::DefFuture<'a, Vec<u32>> {
        Box::pin(async move {
            match &self.1 {
                Pins::None => Ok(Vec::new()),
                Pins::Some(pins) => Ok(pins.clone()),
                Pins::Fails => Err(DefinitionError::StoreFailed {
                    store: "shaped",
                    class: "pool".into(),
                }),
            }
        })
    }
}

#[tokio::test]
async fn a_stored_version_that_no_longer_compiles_is_reported_by_name_and_number() {
    let good = MemoryDefinitionStore::new();
    let text = sample_text("order", "Sales Order");
    let stored = good
        .publish("order", &text, None, "alice", None, now())
        .await
        .unwrap();
    let mut rows = vec![stored.clone()];
    let mut bad_hash = stored.clone();
    bad_hash.version = 2;
    bad_hash.content_hash = "0".repeat(64);
    rows.push(bad_hash);
    let mut bad_text = stored.clone();
    bad_text.version = 3;
    bad_text.text = "this is not a process".to_owned();
    rows.push(bad_text);
    // Parses, but the graph has no terminal node, so it does not compile.
    let mut bad_graph = stored;
    bad_graph.version = 4;
    bad_graph.text = text.replace("terminal = true", "terminal = false");
    rows.push(bad_graph);

    let registry = ProcessRegistry::new();
    let report = load_into(&registry, &Shaped::new(rows)).await.unwrap();
    assert_eq!(report.loaded, vec![("order".to_owned(), 1)]);
    let found: Vec<(u32, LoadFailureKind)> = report
        .failures
        .iter()
        .map(|f| (f.version, f.kind))
        .collect();
    assert_eq!(found.len(), 3, "{found:?}");
    assert_eq!(found[0], (2, LoadFailureKind::HashMismatch));
    assert_eq!(found[1], (3, LoadFailureKind::TextInvalid));
    assert!(matches!(found[2], (4, LoadFailureKind::CompileFailed { problems }) if problems > 0));
    assert!(report.failures.iter().all(|f| f.name == "order"));
    assert!(registry.get("order", 2).is_none());
}

/// Three stored rows of one name: v1 published, v2 retired, v3 retired, all well formed.
async fn retired_rows() -> Vec<spot_bpm_definitions::StoredVersion> {
    let store = MemoryDefinitionStore::new();
    for (n, marker) in ["Sales Order", "Purchase Order", "Delivery Note"]
        .iter()
        .enumerate()
    {
        let expected = u32::try_from(n).ok().filter(|n| *n > 0);
        store
            .publish(
                "order",
                &sample_text("order", marker),
                expected,
                "alice",
                None,
                now(),
            )
            .await
            .unwrap();
    }
    store.retire("order", 2, "alice", now()).await.unwrap();
    store.retire("order", 3, "alice", now()).await.unwrap();
    let mut rows = Vec::new();
    for n in 1..=3 {
        rows.push(store.get("order", n).await.unwrap().unwrap());
    }
    rows
}

#[tokio::test]
async fn a_retired_version_is_loaded_only_when_pinned() {
    let mut store = Shaped::new(retired_rows().await);
    store.1 = Pins::Some(vec![3]);
    let registry = ProcessRegistry::new();
    let report = load_into(&registry, &store).await.unwrap();
    assert!(report.is_clean(), "{report:?}");
    assert_eq!(
        report.loaded,
        vec![("order".to_owned(), 1), ("order".to_owned(), 3)]
    );
    assert!(registry.get("order", 2).is_none(), "retired and unpinned");
    assert_eq!(
        registry.get("order", 3).unwrap().version.status,
        VersionStatus::Retired
    );
    assert_eq!(registry.latest("order").unwrap().number(), 1);
}

#[tokio::test]
async fn a_pinned_retired_version_with_a_wrong_hash_is_reported_and_the_rest_load() {
    let mut rows = retired_rows().await;
    rows[1].content_hash = "0".repeat(64);
    let mut store = Shaped::new(rows);
    store.1 = Pins::Some(vec![2, 3]);
    let registry = ProcessRegistry::new();
    let report = load_into(&registry, &store).await.unwrap();
    assert_eq!(report.failures.len(), 1, "{report:?}");
    assert_eq!(report.failures[0].name, "order");
    assert_eq!(report.failures[0].version, 2);
    assert_eq!(report.failures[0].kind, LoadFailureKind::HashMismatch);
    assert_eq!(
        report.loaded,
        vec![("order".to_owned(), 1), ("order".to_owned(), 3)]
    );
    assert!(registry.get("order", 2).is_none());
}

#[tokio::test]
async fn an_unreadable_pin_list_aborts_the_load() {
    let mut store = Shaped::new(retired_rows().await);
    store.1 = Pins::Fails;
    let registry = ProcessRegistry::new();
    let error = load_into(&registry, &store).await.unwrap_err();
    assert_eq!(error.code(), codes::STORE_FAILED);
    assert!(
        registry.get("order", 1).is_none(),
        "nothing is registered from a half-read store"
    );
}

#[test]
fn every_code_has_a_catalogue_entry() {
    let catalogue = spot_errors::Catalogue::load();
    assert_eq!(catalogue.load_error(), None);
    for code in [
        codes::VERSION_CONFLICT,
        codes::NOT_FOUND,
        codes::TOO_LARGE,
        codes::NAME_INVALID,
        codes::INPUT_INVALID,
        codes::STORE_FAILED,
    ] {
        assert!(catalogue.lookup(code).is_some(), "{code} is not catalogued");
    }
    let error = DefinitionError::StoreFailed {
        store: "memory",
        class: "pool".into(),
    };
    assert_eq!(error.code(), codes::STORE_FAILED);
}
