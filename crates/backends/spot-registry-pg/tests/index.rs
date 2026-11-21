//! The PostgreSQL index against a real server (`SPOT_TEST_PG_URL`; skipped with a message when
//! unset, a failure when `SPOT_REQUIRE_PG=1`). One case table runs against the in-memory
//! reference index and against PostgreSQL, so the two are held to one contract.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use spot_pg_bridge::{Bridge, BridgeConfig};
use spot_registry::*;
use spot_registry_pg::PgIndexStore;

fn memory() -> Arc<MemoryIndexStore> {
    Arc::new(MemoryIndexStore::default())
}

/// Real signed releases from a throwaway in-memory registry.
fn source() -> Setup<MemoryIndexStore> {
    Setup::new(memory())
}

const RICH: &str = "requires_host = \"^0.1\"\ndepends = [{ app = \"base\", version = \"^2\" }]\n\
    [capabilities]\nemail = true\nfiles = \"read\"\nnetwork = [\"example.test:443\"]\n\
    doctypes = { read = [\"Document\"], write = [\"Document\"] }\n";

fn rich_setup() -> Setup<MemoryIndexStore> {
    Setup::with_policy(
        memory(),
        RegistryPolicy {
            capabilities: spot_bundle::Policy {
                allow_email: true,
                ..Default::default()
            },
            ..Default::default()
        },
    )
}

// ---------------------------------------------------------------------------
// The shared case table
// ---------------------------------------------------------------------------

type Case = (&'static str, fn(&dyn IndexStore));

fn cases() -> Vec<Case> {
    vec![
        ("the full contract of the trait docs", contract),
        ("an idempotent re-commit inserts once", idempotent_recommit),
        (
            "a removal marks once and keeps the first reason",
            removal_marks_once,
        ),
        (
            "a transfer is a compare-and-swap on the generation",
            transfer_generation,
        ),
        (
            "describe curates every release and survives a new one",
            describe_survives,
        ),
        ("a suspended publisher cannot mutate", suspended_publisher),
        (
            "a rich release round-trips with all its evidence",
            rich_release_roundtrip,
        ),
        ("unknown records are absent, not errors", absent_records),
    ]
}

fn contract(index: &dyn IndexStore) {
    let source = source();
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
    let removal = |reason: &str| Removal {
        at: NOW,
        reason: reason.into(),
    };
    assert_code(
        index.remove(
            &r1.app,
            &r1.version,
            "publisher13",
            removal("unauthorized"),
            false,
        ),
        "registry.not_owner",
    );
    let marked = index
        .remove(&r2.app, &r2.version, "publisher12", removal("yank"), false)
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

fn idempotent_recommit(index: &dyn IndexStore) {
    let s = source();
    let r = s.publish("demo", "1.0.0", "", 12);
    let first = index.commit(r.clone(), publisher(12)).unwrap();
    assert!(first.inserted);
    let again = index.commit(r.clone(), publisher(12)).unwrap();
    assert!(!again.inserted);
    assert_eq!(again.release, first.release);
    assert_eq!(index.digests().unwrap(), vec![r.digest]);
    // The same digest under another version is a conflict, and build metadata is no new version.
    let mut other = r.clone();
    other.version = version("1.0.1");
    assert_code(
        index.commit(other, publisher(12)),
        "registry.version_conflict",
    );
    let mut built = r.clone();
    built.version = version("1.0.0+build7");
    assert_code(
        index.commit(built, publisher(12)),
        "registry.version_conflict",
    );
    // A different publisher's metadata for the same release is refused.
    assert_code(
        index.commit(r, publisher(13)),
        "registry.publisher_inactive",
    );
}

fn removal_marks_once(index: &dyn IndexStore) {
    let s = source();
    let r = s.publish("demo", "1.0.0", "", 12);
    index.commit(r.clone(), publisher(12)).unwrap();
    let removal = |at, reason: &str| Removal {
        at,
        reason: reason.into(),
    };
    let first = index
        .remove(
            &r.app,
            &r.version,
            "publisher12",
            removal(1, "first"),
            false,
        )
        .unwrap()
        .unwrap();
    assert_eq!(first.yanked, Some(removal(1, "first")));
    assert!(
        index
            .remove(
                &r.app,
                &r.version,
                "publisher12",
                removal(2, "second"),
                false
            )
            .unwrap()
            .is_none()
    );
    let stored = index.release(&r.app, &r.version).unwrap().unwrap();
    assert_eq!(stored.yanked, Some(removal(1, "first")));
    // A withdrawal is its own marker.
    let withdrawn = index
        .remove(
            &r.app,
            &r.version,
            "publisher12",
            removal(3, "malware"),
            true,
        )
        .unwrap()
        .unwrap();
    assert_eq!(withdrawn.withdrawn, Some(removal(3, "malware")));
    assert_eq!(withdrawn.yanked, Some(removal(1, "first")));
    assert_code(
        index.remove(
            &r.app,
            &version("9.9.9"),
            "publisher12",
            removal(4, "x"),
            false,
        ),
        "registry.not_found",
    );
}

fn transfer_generation(index: &dyn IndexStore) {
    let s = source();
    let r = s.publish("demo", "1.0.0", "", 12);
    index.commit(r.clone(), publisher(12)).unwrap();
    let first = transfer("demo", 12, 13, 0);
    index.transfer(first.clone(), publisher(13)).unwrap();
    let owner = index.ownership(&r.app).unwrap().unwrap();
    assert_eq!(
        (owner.publisher.as_str(), owner.generation),
        ("publisher13", 1)
    );
    // Replaying the same signed transfer is refused: the old owner no longer owns the app.
    assert_code(
        index.transfer(first.clone(), publisher(13)),
        "registry.not_owner",
    );
    // A transfer back signed for the stale generation is refused; the right one is accepted.
    assert_code(
        index.transfer(transfer("demo", 13, 12, 0), publisher(12)),
        "registry.transfer_replay",
    );
    let back = transfer("demo", 13, 12, 1);
    index.transfer(back.clone(), publisher(12)).unwrap();
    assert_eq!(index.ownership(&r.app).unwrap().unwrap().generation, 2);
    assert_eq!(index.transfers(&r.app).unwrap(), vec![first, back]);
    // A recipient that does not match the request is refused.
    assert_code(
        index.transfer(transfer("demo", 12, 14, 2), publisher(15)),
        "registry.transfer_invalid",
    );
    assert_code(
        index.transfer(transfer("demo", 12, 12, 2), publisher(12)),
        "registry.transfer_replay",
    );
}

fn describe_survives(index: &dyn IndexStore) {
    let s = source();
    let r1 = s.publish("demo", "1.0.0", "", 12);
    let r2 = s.publish("demo", "1.1.0", "", 12);
    index.commit(r1.clone(), publisher(12)).unwrap();
    assert_code(
        index.describe(&r1.app, "publisher13", "x".into(), "y".into()),
        "registry.not_owner",
    );
    index
        .describe(&r1.app, "publisher12", "Salary tools".into(), "HR".into())
        .unwrap();
    let stored = index.commit(r2.clone(), publisher(12)).unwrap().release;
    assert_eq!(stored.description, "Salary tools");
    assert_eq!(stored.category, "HR");
    let found = |text: &str, category: Option<&str>| {
        index
            .search(&SearchQuery {
                text: text.into(),
                category: category.map(Into::into),
                ..Default::default()
            })
            .unwrap()
            .releases
    };
    assert_eq!(found("salary", None).len(), 1);
    assert_eq!(found("", Some("hr")).len(), 1);
    assert!(found("", Some("finance")).is_empty());
    index
        .describe(&r1.app, "publisher12", "Ledger".into(), "Money".into())
        .unwrap();
    assert!(found("salary", None).is_empty());
    assert_eq!(found("ledger", None)[0].version, version("1.1.0"));
    assert_code(
        index.describe(&app("missing"), "publisher12", "x".into(), "y".into()),
        "registry.not_owner",
    );
}

fn suspended_publisher(index: &dyn IndexStore) {
    let s = source();
    let r = s.publish("demo", "1.0.0", "", 12);
    let next = s.publish("demo", "2.0.0", "", 12);
    index.commit(r.clone(), publisher(12)).unwrap();
    index
        .set_publisher_status("publisher12", PublisherStatus::Suspended)
        .unwrap();
    assert_eq!(
        index.publisher("publisher12").unwrap().unwrap().status,
        PublisherStatus::Suspended
    );
    assert_code(
        index.commit(next.clone(), publisher(12)),
        "registry.publisher_inactive",
    );
    assert_code(
        index.remove(
            &r.app,
            &r.version,
            "publisher12",
            Removal {
                at: 1,
                reason: "x".into(),
            },
            false,
        ),
        "registry.publisher_inactive",
    );
    assert_code(
        index.describe(&r.app, "publisher12", "x".into(), "y".into()),
        "registry.publisher_inactive",
    );
    index
        .set_publisher_status("publisher12", PublisherStatus::Active)
        .unwrap();
    index.commit(next, publisher(12)).unwrap();
    assert_code(
        index.set_publisher_status("nobody", PublisherStatus::Active),
        "registry.not_found",
    );
}

fn rich_release_roundtrip(index: &dyn IndexStore) {
    let s = rich_setup();
    let r = s.publish("rich", "1.2.3-rc.1", RICH, 12);
    assert!(!r.depends.is_empty() && !r.capabilities.network.is_empty());
    index.commit(r.clone(), publisher(12)).unwrap();
    let back = index.release(&r.app, &r.version).unwrap().unwrap();
    assert_eq!(back, r);
    assert_eq!(index.by_digest(r.digest).unwrap(), Some(r.clone()));
    assert_eq!(index.publisher("publisher12").unwrap(), Some(publisher(12)));
}

fn absent_records(index: &dyn IndexStore) {
    assert!(index.ownership(&app("nothing")).unwrap().is_none());
    assert!(index.publisher("nobody").unwrap().is_none());
    assert!(
        index
            .release(&app("nothing"), &version("1.0.0"))
            .unwrap()
            .is_none()
    );
    assert!(index.transfers(&app("nothing")).unwrap().is_empty());
    assert!(index.digests().unwrap().is_empty());
    assert_code(
        index.search(&SearchQuery {
            limit: 0,
            ..Default::default()
        }),
        "registry.invalid_query",
    );
    assert_code(
        index.search(&SearchQuery {
            cursor: Some("nope".into()),
            ..Default::default()
        }),
        "registry.invalid_cursor",
    );
}

#[test]
fn the_memory_index_passes_the_case_table() {
    for (name, case) in cases() {
        println!("memory: {name}");
        case(&MemoryIndexStore::default());
    }
}

/// Empties the registry tables between cases. The transfer history is append-only (a trigger
/// refuses DELETE), so the trigger is dropped and recreated around the truncate.
fn wipe(fx: &Fixture) {
    fx.rt.block_on(async {
        let client = fx.db.raw_client().await;
        client
            .batch_execute(
                "DROP TRIGGER IF EXISTS spot_registry_transfer_append_only \
                     ON spot_control.spot_registry_transfer; \
                 TRUNCATE spot_control.spot_registry_transfer, spot_control.spot_registry_release, \
                     spot_control.spot_registry_ownership, spot_control.spot_registry_publisher; \
                 UPDATE spot_control.spot_registry_meta SET revision = 0; \
                 CREATE TRIGGER spot_registry_transfer_append_only \
                     BEFORE UPDATE OR DELETE ON spot_control.spot_registry_transfer \
                     FOR EACH ROW EXECUTE FUNCTION spot_control.spot_registry_transfer_immutable();",
            )
            .await
            .unwrap();
    });
}

#[test]
fn the_postgres_index_passes_the_same_case_table() {
    let Some(fx) = fixture() else { return };
    let (bridge, store) = open_db(fx.db.config());
    for (name, case) in cases() {
        println!("postgres: {name}");
        wipe(&fx);
        case(&*store);
    }
    bridge.shutdown(Duration::from_secs(2)).unwrap();
}

// ---------------------------------------------------------------------------
// Search parity over a fixed corpus of 200 apps
// ---------------------------------------------------------------------------

const WORDS: [&str; 6] = ["ledger", "payroll", "stock", "alpha", "beta", "gamma"];
const CATEGORIES: [&str; 4] = ["Accounting", "hr", "Operations", "tools"];

fn corpus_release(template: &Release, i: usize) -> (Release, Publisher) {
    let mut r = template.clone();
    let id = 12 + u8::try_from(i % 3).unwrap();
    r.app = app(&format!("app{i:03}"));
    r.version = version(if i.is_multiple_of(11) {
        "1.0.0-beta.1"
    } else {
        "1.0.0"
    });
    r.digest = format!("{:064x}", 0x1000 + i).parse().unwrap();
    r.publisher = format!("publisher{id}");
    r.title = format!("{} {}", WORDS[i % 6], WORDS[(i / 6) % 6]);
    r.description = format!("{} suite for {}", WORDS[(i / 3) % 6], WORDS[(i * 5) % 6]);
    r.category = CATEGORIES[i % 4].to_owned();
    r.requires_host = match i % 3 {
        0 => Some(VersionReq::parse("^0.1").unwrap()),
        1 => Some(VersionReq::parse(">=0.2").unwrap()),
        _ => None,
    };
    r.capabilities.email = i.is_multiple_of(5);
    r.capabilities.network.clear();
    if i.is_multiple_of(7) {
        r.capabilities.network.push(spot_manifest::Endpoint {
            host: "example.test".into(),
            port: 443,
        });
    }
    if i.is_multiple_of(9) {
        r.capabilities.doctypes.read.push("Document".into());
    }
    (r, publisher(id))
}

fn build_corpus(index: &dyn IndexStore, caller: &Caller) {
    let template = source().publish("template", "1.0.0", "", 12);
    for i in 0..200 {
        let (r, p) = corpus_release(&template, i);
        assert!(index.commit(r, p).unwrap().inserted);
    }
    // Newer versions for some apps, a yanked one, a withdrawn one, a re-described one.
    for i in (0..200).step_by(10) {
        let (mut r, p) = corpus_release(&template, i);
        r.version = version("2.0.0");
        r.digest = format!("{:064x}", 0x9000 + i).parse().unwrap();
        index.commit(r, p).unwrap();
    }
    let removal = |reason: &str| Removal {
        at: 1,
        reason: reason.into(),
    };
    let publisher_of = |i: usize| format!("publisher{}", 12 + i % 3);
    for i in [5_usize, 17, 40] {
        let name = app(&format!("app{i:03}"));
        let v = if i.is_multiple_of(11) {
            "1.0.0-beta.1"
        } else {
            "1.0.0"
        };
        index
            .remove(&name, &version(v), &publisher_of(i), removal("yank"), false)
            .unwrap();
    }
    index
        .remove(
            &app("app023"),
            &version("1.0.0"),
            &publisher_of(23),
            removal("malware"),
            true,
        )
        .unwrap();
    for i in [3_usize, 30, 31] {
        index
            .describe(
                &app(&format!("app{i:03}")),
                &publisher_of(i),
                "curated unique zebra".into(),
                "Zebra".into(),
            )
            .unwrap();
    }
    let _ = caller;
}

fn queries(caller: &Caller) -> Vec<(&'static str, SearchQuery)> {
    let q = |f: &dyn Fn(&mut SearchQuery)| {
        let mut query = SearchQuery {
            limit: 7,
            ..Default::default()
        };
        f(&mut query);
        query
    };
    vec![
        ("blank", q(&|_| {})),
        ("exact", q(&|x| x.text = "app042".into())),
        ("prefix", q(&|x| x.text = "app01".into())),
        ("word", q(&|x| x.text = "ledger".into())),
        ("two words", q(&|x| x.text = "Payroll STOCK".into())),
        ("nothing", q(&|x| x.text = "zzzz".into())),
        ("punctuation only", q(&|x| x.text = "!!".into())),
        ("zebra", q(&|x| x.text = "unique zebra".into())),
        (
            "publisher",
            q(&|x| x.publisher = Some("publisher13".into())),
        ),
        ("category", q(&|x| x.category = Some("HR".into()))),
        (
            "category and text",
            q(&|x| {
                x.category = Some("operations".into());
                x.text = "alpha".into();
            }),
        ),
        (
            "host 0.1",
            q(&|x| x.compatible_host = Some(version("0.1.5"))),
        ),
        (
            "host 0.3",
            q(&|x| x.compatible_host = Some(version("0.3.0"))),
        ),
        ("network", q(&|x| x.capability = Some("network".into()))),
        ("email", q(&|x| x.capability = Some("email".into()))),
        ("files", q(&|x| x.capability = Some("files".into()))),
        (
            "doctypes read",
            q(&|x| x.capability = Some("doctypes.read".into())),
        ),
        (
            "doctypes write",
            q(&|x| x.capability = Some("doctypes.write".into())),
        ),
        (
            "unknown capability",
            q(&|x| x.capability = Some("teleport".into())),
        ),
        ("pre-releases", q(&|x| x.visibility.include_pre = true)),
        (
            "inactive as owner",
            q(&|x| {
                x.visibility.include_inactive = true;
                x.caller = Some(caller.clone());
            }),
        ),
        ("wide page", q(&|x| x.limit = 100)),
        (
            "page of one",
            q(&|x| {
                x.limit = 1;
                x.text = "beta".into();
            }),
        ),
    ]
}

/// Every page of a query, with its releases and its cursors.
fn pages(index: &dyn IndexStore, query: &SearchQuery) -> Vec<(Vec<Release>, Option<String>)> {
    let mut query = query.clone();
    let mut out = Vec::new();
    loop {
        let page = index.search(&query).unwrap();
        out.push((page.releases, page.next_cursor.clone()));
        match page.next_cursor {
            Some(cursor) => query.cursor = Some(cursor),
            None => return out,
        }
        assert!(out.len() < 100, "paging does not terminate");
    }
}

#[test]
fn search_ranking_and_tie_breaks_match_the_in_memory_index_over_200_apps() {
    let Some(fx) = fixture() else { return };
    let (bridge, store) = open_db(fx.db.config());
    let mem = memory();
    let caller_source = Setup::new(mem.clone());
    let caller = caller_source.caller(13);
    build_corpus(&*mem, &caller);
    build_corpus(&*store, &caller);
    assert_eq!(mem.digests().unwrap(), store.digests().unwrap());
    let mut non_empty = 0;
    for (name, query) in queries(&caller) {
        let expected = pages(&*mem, &query);
        let actual = pages(&*store, &query);
        assert_eq!(actual.len(), expected.len(), "{name}: page count");
        for (n, (a, e)) in actual.iter().zip(&expected).enumerate() {
            let names = |p: &(Vec<Release>, Option<String>)| -> Vec<String> {
                p.0.iter()
                    .map(|r| format!("{}@{}", r.app.as_str(), r.version))
                    .collect()
            };
            assert_eq!(names(a), names(e), "{name}: page {n}");
            assert_eq!(a.0, e.0, "{name}: page {n} full releases");
            assert_eq!(a.1, e.1, "{name}: page {n} cursor");
        }
        non_empty += usize::from(expected.iter().any(|p| !p.0.is_empty()));
    }
    assert!(
        non_empty >= 15,
        "the fixed queries must exercise real results"
    );
    // The ranking itself: the exact name comes before a prefix match, then by overlap, then name.
    let ranked = store
        .search(&SearchQuery {
            text: "app042".into(),
            limit: 5,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(ranked.releases[0].app.as_str(), "app042");
    // A cursor from before a mutation is refused after it (the cursor binds the revision).
    let first = store
        .search(&SearchQuery {
            limit: 7,
            ..Default::default()
        })
        .unwrap();
    let cursor = first.next_cursor.unwrap();
    store
        .describe(
            &app("app001"),
            "publisher13",
            "fresh".into(),
            "tools".into(),
        )
        .unwrap();
    assert_code(
        store.search(&SearchQuery {
            limit: 7,
            cursor: Some(cursor),
            ..Default::default()
        }),
        "registry.invalid_cursor",
    );
    bridge.shutdown(Duration::from_secs(2)).unwrap();
}

// ---------------------------------------------------------------------------
// Restart, threads, failure
// ---------------------------------------------------------------------------

#[test]
fn state_survives_a_restart() {
    let Some(fx) = fixture() else { return };
    let (bridge, store) = open_db(fx.db.config());
    let s = rich_setup();
    let r1 = s.publish("demo", "1.0.0", RICH, 12);
    store.commit(r1.clone(), publisher(12)).unwrap();
    store
        .transfer(transfer("demo", 12, 13, 0), publisher(13))
        .unwrap();
    drop(store);
    bridge.shutdown(Duration::from_secs(2)).unwrap();
    drop(bridge);
    let (bridge, store) = open_db(fx.db.config());
    assert_eq!(
        store.release(&r1.app, &r1.version).unwrap(),
        Some(r1.clone())
    );
    assert_eq!(store.ownership(&r1.app).unwrap().unwrap().generation, 1);
    assert_eq!(store.transfers(&r1.app).unwrap().len(), 1);
    // Preparing again keeps the rows.
    store.prepare().unwrap();
    assert_eq!(store.digests().unwrap(), vec![r1.digest]);
    bridge.shutdown(Duration::from_secs(2)).unwrap();
}

#[test]
fn a_call_from_inside_a_runtime_is_refused_not_deadlocked() {
    let Some(fx) = fixture() else { return };
    let (bridge, store) = open_db(fx.db.config());
    let err = fx
        .rt
        .block_on(async { store.ownership(&app("demo")) })
        .unwrap_err();
    assert_eq!(err.code, "registry.storage");
    assert!(err.message.contains("pg_bridge.wrong_thread"), "{err}");
    let r = source().publish("demo", "1.0.0", "", 12);
    let err = fx
        .rt
        .block_on(async { store.commit(r.clone(), publisher(12)) })
        .unwrap_err();
    assert_eq!(err.code, "registry.storage");
    assert!(store.release(&r.app, &r.version).unwrap().is_none());
    bridge.shutdown(Duration::from_secs(2)).unwrap();
}

#[test]
fn building_the_store_inside_a_current_thread_runtime_is_refused() {
    let Some(fx) = fixture() else { return };
    let pool = pool_of(fx.db.config(), 8);
    let bridge = Arc::new(Bridge::new(BridgeConfig::default(), pool.clone()).unwrap());
    let current = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let err = current
        .block_on(async { PgIndexStore::new(Arc::clone(&bridge), &pool) })
        .unwrap_err();
    assert_eq!(err.code(), "registry_pg.bad_setup");
    let small = pool_of(fx.db.config(), 3);
    let err = PgIndexStore::new(bridge, &small).unwrap_err();
    assert_eq!(err.code(), "registry_pg.bad_setup");
}

fn dead_store(fx: &Fixture) -> (Arc<Bridge>, PgIndexStore, spot_postgres::PgConfig) {
    let mut dead = fx.db.config();
    dead.host = "127.0.0.1".to_owned();
    dead.port = 1;
    let pool = pool_of(dead.clone(), 8);
    let bridge = Arc::new(Bridge::new(BridgeConfig::default(), pool.clone()).unwrap());
    let store = PgIndexStore::new(Arc::clone(&bridge), &pool).unwrap();
    (bridge, store, dead)
}

#[test]
fn an_unreachable_database_is_a_storage_failure_and_leaves_no_partial_row() {
    let Some(fx) = fixture() else { return };
    let (bridge, good) = open_db(fx.db.config());
    let (down_bridge, down, _) = dead_store(&fx);
    let r = source().publish("demo", "1.0.0", "", 12);
    let err = down.commit(r.clone(), publisher(12)).unwrap_err();
    assert_eq!(err.code, "registry.storage");
    assert_code(down.ownership(&r.app), "registry.storage");
    assert_code(down.release(&r.app, &r.version), "registry.storage");
    assert_code(down.digests(), "registry.storage");
    assert_code(down.search(&SearchQuery::default()), "registry.storage");
    assert!(good.ownership(&r.app).unwrap().is_none());
    assert!(good.publisher("publisher12").unwrap().is_none());
    assert!(good.digests().unwrap().is_empty());
    down_bridge.shutdown(Duration::from_secs(2)).unwrap();
    bridge.shutdown(Duration::from_secs(2)).unwrap();
}

#[test]
fn no_secret_or_connection_string_appears_in_any_error_text() {
    let Some(fx) = fixture() else { return };
    let (down_bridge, down, dead) = dead_store(&fx);
    let r = source().publish("demo", "1.0.0", "", 12);
    let errors = [
        down.commit(r.clone(), publisher(12)).unwrap_err(),
        down.ownership(&r.app).unwrap_err(),
        down.search(&SearchQuery::default()).unwrap_err(),
        down.set_publisher_status("publisher12", PublisherStatus::Active)
            .unwrap_err(),
    ];
    for error in &errors {
        for text in [format!("{error}"), format!("{error:?}")] {
            for forbidden in [
                dead.password.as_str(),
                dead.host.as_str(),
                dead.user.as_str(),
                dead.dbname.as_str(),
                "postgres://",
                "SELECT",
                "INSERT",
                "UPDATE",
                "publisher12",
            ] {
                if !forbidden.is_empty() {
                    assert!(!text.contains(forbidden), "{forbidden:?} leaked in {text}");
                }
            }
        }
    }
    let setup = down.prepare().unwrap_err();
    let text = format!("{setup} {setup:?}");
    assert!(!text.contains(&dead.password) && !text.contains("postgres://"));
    down_bridge.shutdown(Duration::from_secs(2)).unwrap();
}

#[test]
fn a_stored_row_that_no_longer_rebuilds_is_a_storage_failure() {
    let Some(fx) = fixture() else { return };
    let (bridge, store) = open_db(fx.db.config());
    let r = source().publish("demo", "1.0.0", "", 12);
    store.commit(r.clone(), publisher(12)).unwrap();
    fx.rt.block_on(async {
        let client = fx.db.raw_client().await;
        client
            .execute(
                "UPDATE spot_control.spot_registry_release SET evidence = '{\"depends\": 5}'::jsonb",
                &[],
            )
            .await
            .unwrap();
    });
    assert_code(store.release(&r.app, &r.version), "registry.storage");
    assert_code(store.by_digest(r.digest), "registry.storage");
    assert_code(
        store.releases(&r.app, &Visibility::default(), None),
        "registry.storage",
    );
    bridge.shutdown(Duration::from_secs(2)).unwrap();
}

#[test]
fn the_transfer_history_is_append_only() {
    let Some(fx) = fixture() else { return };
    let (bridge, store) = open_db(fx.db.config());
    let r = source().publish("demo", "1.0.0", "", 12);
    store.commit(r.clone(), publisher(12)).unwrap();
    store
        .transfer(transfer("demo", 12, 13, 0), publisher(13))
        .unwrap();
    let refused = fx.rt.block_on(async {
        let client = fx.db.raw_client().await;
        let update = client
            .execute(
                "UPDATE spot_control.spot_registry_transfer SET reason = 'x'",
                &[],
            )
            .await;
        let delete = client
            .execute("DELETE FROM spot_control.spot_registry_transfer", &[])
            .await;
        (update.is_err(), delete.is_err())
    });
    assert_eq!(refused, (true, true));
    assert_eq!(store.transfers(&r.app).unwrap().len(), 1);
    bridge.shutdown(Duration::from_secs(2)).unwrap();
}
